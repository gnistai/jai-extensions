//! `native.fetch` and `native.fetchAbort`: one buffered HTTP request with
//! reqwest, for the JavaScript `fetch` in `js/fetch.js`. Only the origin of
//! `ServerRef.url` is reachable (DESIGN.md sections 8 and 12); any other URL,
//! and any redirect to another origin, is a TypeError "fetch blocked".

use std::cell::OnceCell;
use std::rc::Rc;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT};
use reqwest::redirect::Policy;
use reqwest::Method;
use rquickjs::prelude::{Async, Func};
use rquickjs::{Ctx, Exception, Object, TypedArray};
use tokio_util::sync::CancellationToken;
use url::{Origin, Url};

use crate::types::ServerRef;
use crate::web::Cancels;

/// Largest response body a fetch buffers.
const MAX_BODY: usize = 256 << 20;
/// Redirects followed before a fetch fails, as in browsers.
const MAX_REDIRECTS: usize = 20;

/// Request headers a page may not set (the Fetch Standard's forbidden request
/// header names); they are dropped silently, as browsers do.
const FORBIDDEN_HEADERS: [&str; 21] = [
    "accept-charset",
    "accept-encoding",
    "access-control-request-headers",
    "access-control-request-method",
    "connection",
    "content-length",
    "cookie",
    "cookie2",
    "date",
    "dnt",
    "expect",
    "host",
    "keep-alive",
    "origin",
    "referer",
    "set-cookie",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "via",
];

#[derive(Debug, thiserror::Error)]
enum FetchError {
    #[error("fetch blocked: {0} is not the local server")]
    Blocked(String),
    #[error("fetch blocked: a redirect left the local server")]
    BlockedRedirect,
    #[error("{0}")]
    Invalid(String),
    #[error("fetch failed: {0}")]
    Network(String),
    #[error("fetch failed: the response is larger than {} MiB", MAX_BODY >> 20)]
    TooLarge,
    #[error("This operation was aborted")]
    Aborted,
}

impl FetchError {
    fn throw(self, ctx: &Ctx<'_>) -> rquickjs::Error {
        match self {
            FetchError::Aborted => {
                Exception::throw_dom(ctx, "AbortError", "This operation was aborted")
            }
            other => Exception::throw_type(ctx, &other.to_string()),
        }
    }

    fn network(error: reqwest::Error) -> FetchError {
        let mut source: Option<&dyn std::error::Error> = Some(&error);
        let mut parts = Vec::new();
        while let Some(e) = source {
            if e.downcast_ref::<RedirectBlocked>().is_some() {
                return FetchError::BlockedRedirect;
            }
            let text = e.to_string();
            if !parts.iter().any(|p: &String| p.contains(&text)) {
                parts.push(text);
            }
            source = e.source();
        }
        FetchError::Network(parts.join(": "))
    }
}

/// The redirect policy's error for a redirect to another origin.
#[derive(Debug, thiserror::Error)]
#[error("redirect to another origin")]
struct RedirectBlocked;

/// What `native.fetch` resolves to.
struct Reply {
    status: u16,
    status_text: &'static str,
    url: String,
    redirected: bool,
    /// `[name, value, name, value, ...]`, names lowercase.
    headers: Vec<String>,
    body: Vec<u8>,
}

/// Fetch state for one runtime.
struct FetchState {
    /// The one origin requests may go to; None blocks everything.
    allowed: Option<Origin>,
    client: OnceCell<reqwest::Client>,
    requests: Cancels,
}

impl FetchState {
    /// The HTTP client, built on first use: no proxy (the server is local and
    /// the token must not reach a proxy), same-origin redirects only.
    fn client(&self) -> Result<&reqwest::Client, FetchError> {
        if let Some(client) = self.client.get() {
            return Ok(client);
        }
        let allowed = self.allowed.clone();
        let policy = Policy::custom(move |attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                attempt.error("too many redirects")
            } else if allowed.as_ref() == Some(&attempt.url().origin()) {
                attempt.follow()
            } else {
                attempt.error(RedirectBlocked)
            }
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(policy)
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(FetchError::network)?;
        Ok(self.client.get_or_init(|| client))
    }
}

/// Registers `native.fetch(id, url, method, headers, body | null)` and
/// `native.fetchAbort(id)`.
pub(crate) fn install<'js>(
    native: &Object<'js>,
    server: Option<&ServerRef>,
) -> rquickjs::Result<()> {
    let allowed = server
        .and_then(|s| Url::parse(&s.url).ok())
        .map(|u| u.origin())
        .filter(Origin::is_tuple);
    let state = Rc::new(FetchState {
        allowed,
        client: OnceCell::new(),
        requests: Cancels::default(),
    });

    let fetching = state.clone();
    native.set(
        "fetch",
        Func::from(Async(
            move |ctx: Ctx<'js>,
                  id: u32,
                  url: String,
                  method: String,
                  headers: Vec<String>,
                  body: Option<TypedArray<'js, u8>>| {
                let state = fetching.clone();
                // Copy the body out now: the future must not borrow JavaScript memory.
                // SAFETY: no JavaScript runs while the bytes are copied.
                let body = body
                    .map(|b| unsafe { b.as_bytes() }.map(<[u8]>::to_vec))
                    .map(|bytes| {
                        bytes.ok_or_else(|| {
                            FetchError::Invalid("The request body's ArrayBuffer is detached".into())
                        })
                    })
                    .transpose();
                let token = state.requests.register(id);
                async move {
                    let result = match body {
                        Ok(body) => send(&state, &url, &method, &headers, body, &token).await,
                        Err(e) => Err(e),
                    };
                    state.requests.finish(id);
                    match result {
                        Ok(reply) => reply_object(&ctx, reply),
                        Err(e) => Err(e.throw(&ctx)),
                    }
                }
            },
        )),
    )?;
    native.set(
        "fetchAbort",
        Func::from(move |id: u32| state.requests.cancel(id)),
    )?;
    Ok(())
}

async fn send(
    state: &FetchState,
    url: &str,
    method: &str,
    headers: &[String],
    body: Option<Vec<u8>>,
    token: &CancellationToken,
) -> Result<Reply, FetchError> {
    let mut target = Url::parse(url)
        .map_err(|_| FetchError::Invalid(format!("Failed to parse URL from {url}")))?;
    let origin = target.origin();
    if state.allowed.as_ref() != Some(&origin) {
        return Err(FetchError::Blocked(origin.ascii_serialization()));
    }
    if !target.username().is_empty() || target.password().is_some() {
        return Err(FetchError::Invalid(
            "Request cannot be constructed from a URL that includes credentials".into(),
        ));
    }
    target.set_fragment(None);
    let method = Method::from_bytes(method.as_bytes())
        .map_err(|_| FetchError::Invalid(format!("'{method}' is not a valid HTTP method.")))?;
    let header_map = request_headers(headers)?;

    let mut request = state
        .client()?
        .request(method, target.clone())
        .headers(header_map);
    if let Some(bytes) = body {
        request = request.body(bytes);
    }
    let mut response = tokio::select! {
        r = request.send() => r.map_err(FetchError::network)?,
        () = token.cancelled() => return Err(FetchError::Aborted),
    };
    let status = response.status();
    let mut final_url = response.url().clone();
    final_url.set_fragment(None);
    let redirected = final_url != target;
    let headers = response
        .headers()
        .iter()
        .flat_map(|(name, value)| [name.as_str().to_owned(), latin1_decode(value.as_bytes())])
        .collect();
    let mut data = Vec::new();
    loop {
        let chunk = tokio::select! {
            c = response.chunk() => c.map_err(FetchError::network)?,
            () = token.cancelled() => return Err(FetchError::Aborted),
        };
        let Some(chunk) = chunk else { break };
        if data.len() + chunk.len() > MAX_BODY {
            return Err(FetchError::TooLarge);
        }
        data.extend_from_slice(&chunk);
    }
    Ok(Reply {
        status: status.as_u16(),
        status_text: status.canonical_reason().unwrap_or(""),
        url: final_url.into(),
        redirected,
        headers,
        body: data,
    })
}

/// The request headers from `[name, value, ...]` (already validated by the
/// Headers class), minus forbidden ones, plus `accept: */*` when absent.
fn request_headers(flat: &[String]) -> Result<HeaderMap, FetchError> {
    let mut map = HeaderMap::new();
    for [name, value] in flat.as_chunks::<2>().0 {
        let name = name.to_ascii_lowercase();
        if FORBIDDEN_HEADERS.contains(&name.as_str())
            || name.starts_with("proxy-")
            || name.starts_with("sec-")
        {
            continue;
        }
        let invalid = || FetchError::Invalid(format!("Invalid header: {name}"));
        let header = HeaderName::from_bytes(name.as_bytes()).map_err(|_| invalid())?;
        let bytes = latin1_encode(value).ok_or_else(invalid)?;
        let value = HeaderValue::from_bytes(&bytes).map_err(|_| invalid())?;
        map.append(header, value);
    }
    if !map.contains_key(ACCEPT) {
        map.insert(ACCEPT, HeaderValue::from_static("*/*"));
    }
    Ok(map)
}

/// A ByteString as bytes: every char must be at most U+00FF.
fn latin1_encode(text: &str) -> Option<Vec<u8>> {
    text.chars()
        .map(|c| u8::try_from(u32::from(c)).ok())
        .collect()
}

/// Header bytes as a ByteString (one char per byte), as browsers expose them.
fn latin1_decode(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

fn reply_object<'js>(ctx: &Ctx<'js>, reply: Reply) -> rquickjs::Result<Object<'js>> {
    let out = Object::new(ctx.clone())?;
    out.set("status", reply.status)?;
    out.set("statusText", reply.status_text)?;
    out.set("url", reply.url)?;
    out.set("redirected", reply.redirected)?;
    out.set("headers", reply.headers)?;
    out.set("body", TypedArray::new(ctx.clone(), reply.body)?)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forbidden_headers_are_dropped_and_accept_is_added() {
        let flat: Vec<String> = [
            "Host",
            "evil",
            "x-test",
            "1",
            "Origin",
            "https://x",
            "sec-fetch-mode",
            "cors",
            "content-type",
            "text/plain",
        ]
        .map(String::from)
        .to_vec();
        let map = request_headers(&flat).unwrap();
        assert!(
            map.get("host").is_none()
                && map.get("origin").is_none()
                && map.get("sec-fetch-mode").is_none()
        );
        assert_eq!(map.get("x-test").unwrap(), "1");
        assert_eq!(map.get("accept").unwrap(), "*/*");
        assert_eq!(map.get("content-type").unwrap(), "text/plain");
    }

    #[test]
    fn latin1_round_trip() {
        assert_eq!(latin1_encode("caf\u{e9}").unwrap(), b"caf\xe9");
        assert!(latin1_encode("\u{100}").is_none());
        assert_eq!(latin1_decode(b"caf\xe9"), "caf\u{e9}");
    }
}
