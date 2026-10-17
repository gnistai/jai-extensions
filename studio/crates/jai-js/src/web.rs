//! The web globals the bundles use: console, timers, fetch (local server only),
//! Headers, AbortController/AbortSignal, TextEncoder/TextDecoder, URL and
//! URLSearchParams, and crypto (DESIGN.md section 8, "Globals the host installs").
//!
//! Rust supplies small primitives on a private `native` object; the WHATWG
//! classes are JavaScript (`src/js/*.js`) built on top of them. Native
//! functions never hold JavaScript values, so nothing can form a reference
//! cycle the QuickJS collector cannot see.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use rand::RngCore;
use rquickjs::context::EvalOptions;
use rquickjs::prelude::{Async, Func, List, Opt};
use rquickjs::{ArrayBuffer, Ctx, Exception, Function, Object, TypedArray, Value};
use sha2::{Digest, Sha256, Sha384, Sha512};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::fetch;
use crate::types::ServerRef;

/// The shim files, in evaluation order (later files use earlier ones).
const SHIMS: [(&str, &str); 6] = [
    ("prelude.js", include_str!("js/prelude.js")),
    ("events.js", include_str!("js/events.js")),
    ("encoding.js", include_str!("js/encoding.js")),
    ("url.js", include_str!("js/url.js")),
    ("fetch.js", include_str!("js/fetch.js")),
    ("crypto.js", include_str!("js/crypto.js")),
];

/// Installs every web global on the context's global object. `server` is the
/// only origin `fetch` may reach; without it every fetch is blocked.
pub(crate) fn install(ctx: &Ctx<'_>, server: Option<&ServerRef>) -> rquickjs::Result<()> {
    let native = Object::new(ctx.clone())?;
    native.set("log", Func::from(log))?;
    install_timers(&native)?;
    native.set("utf8Encode", Func::from(utf8_encode))?;
    native.set("utf8Decode", Func::from(utf8_decode))?;
    native.set("urlParse", Func::from(url_parse))?;
    native.set("urlSet", Func::from(url_set))?;
    native.set("formParse", Func::from(form_parse))?;
    native.set("formSerialize", Func::from(form_serialize))?;
    native.set("randomFill", Func::from(random_fill))?;
    native.set("randomUUID", Func::from(random_uuid))?;
    native.set("digest", Func::from(digest))?;
    fetch::install(&native, server)?;

    let internal = Object::new(ctx.clone())?;
    for (name, source) in SHIMS {
        let mut options = EvalOptions::default();
        options.filename = Some(format!("jai-host/{name}"));
        let shim: Function = ctx.eval_with_options(source, options)?;
        shim.call::<_, ()>((native.clone(), internal.clone()))?;
    }
    Ok(())
}

/// Cancellation tokens of in-flight native operations (timers, fetches), by
/// the id JavaScript gave them.
#[derive(Default)]
pub(crate) struct Cancels(RefCell<HashMap<u32, CancellationToken>>);

impl Cancels {
    /// A fresh token for `id`; an older operation with the same id is cancelled.
    pub(crate) fn register(&self, id: u32) -> CancellationToken {
        let token = CancellationToken::new();
        if let Some(old) = self.0.borrow_mut().insert(id, token.clone()) {
            old.cancel();
        }
        token
    }

    pub(crate) fn cancel(&self, id: u32) {
        if let Some(token) = self.0.borrow_mut().remove(&id) {
            token.cancel();
        }
    }

    /// Forgets `id` once its operation has ended.
    pub(crate) fn finish(&self, id: u32) {
        self.0.borrow_mut().remove(&id);
    }
}

/// Every console line and host warning as `level: text`, for tests to search
/// by a unique marker.
#[cfg(test)]
pub(crate) static LOG_LINES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) fn test_log(level: &str, text: &str) {
    if let Ok(mut lines) = LOG_LINES.lock() {
        lines.push(format!("{level}: {text}"));
    }
}

/// `native.log(level, text)`: console output goes to tracing.
fn log(level: String, text: String) {
    const TARGET: &str = "jai_js::console";
    #[cfg(test)]
    test_log(&level, &text);
    match level.as_str() {
        "error" => tracing::error!(target: TARGET, "{text}"),
        "warn" => tracing::warn!(target: TARGET, "{text}"),
        "debug" => tracing::debug!(target: TARGET, "{text}"),
        "trace" => tracing::trace!(target: TARGET, "{text}"),
        _ => tracing::info!(target: TARGET, "{text}"),
    }
}

/// `native.timerStart(id, ms)` resolves to true when the delay has passed and
/// to false when `native.timerCancel(id)` came first.
fn install_timers(native: &Object<'_>) -> rquickjs::Result<()> {
    let timers = Rc::new(Cancels::default());
    let started = timers.clone();
    native.set(
        "timerStart",
        Func::from(Async(move |id: u32, ms: f64| {
            let token = started.register(id);
            let timers = started.clone();
            async move {
                let delay = Duration::from_secs_f64(if ms.is_finite() {
                    ms.max(0.0) / 1000.0
                } else {
                    0.0
                });
                let fired = tokio::select! {
                    () = tokio::time::sleep(delay) => true,
                    () = token.cancelled() => false,
                };
                timers.finish(id);
                rquickjs::Result::Ok(fired)
            }
        })),
    )?;
    native.set("timerCancel", Func::from(move |id: u32| timers.cancel(id)))?;
    Ok(())
}

/// `native.utf8Encode(text)`; the shim passes well-formed strings only.
fn utf8_encode<'js>(ctx: Ctx<'js>, text: String) -> rquickjs::Result<TypedArray<'js, u8>> {
    TypedArray::new(ctx, text.into_bytes())
}

/// `native.utf8Decode(bytes, fatal, stream)` returns `[text, kept]`: with
/// `stream`, an incomplete sequence at the end is not decoded and its `kept`
/// bytes are left for the next call. Malformed input is replaced by U+FFFD per
/// maximal subpart (the WHATWG UTF-8 decoder), or is a TypeError when `fatal`.
fn utf8_decode<'js>(
    ctx: Ctx<'js>,
    bytes: TypedArray<'js, u8>,
    fatal: bool,
    stream: bool,
) -> rquickjs::Result<List<(String, u32)>> {
    let Some(raw) = bytes.as_raw() else {
        return Err(Exception::throw_type(&ctx, "The ArrayBuffer is detached"));
    };
    // SAFETY: no JavaScript runs while the slice is in use; it is dropped
    // before anything below can call back into the engine.
    let decoded = {
        let data = unsafe { raw.as_ref() };
        let kept = if stream { incomplete_tail(data) } else { 0 };
        let body = &data[..data.len() - kept];
        let text = if fatal {
            std::str::from_utf8(body).ok().map(str::to_owned)
        } else {
            Some(String::from_utf8_lossy(body).into_owned())
        };
        text.map(|t| (t, kept))
    };
    match decoded {
        Some((text, kept)) => Ok(List((text, u32::try_from(kept).unwrap_or(0)))),
        None => Err(Exception::throw_type(
            &ctx,
            "The encoded data was not valid for encoding utf-8",
        )),
    }
}

/// The length of a trailing UTF-8 sequence that is a valid but unfinished
/// prefix (0 when the input ends on a boundary or with invalid bytes).
fn incomplete_tail(data: &[u8]) -> usize {
    let n = data.len();
    for back in 1..=n.min(3) {
        let lead = data[n - back];
        if lead & 0xc0 == 0x80 {
            continue;
        }
        let need = match lead {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => return 0,
        };
        if back >= need {
            return 0;
        }
        let (lo, hi) = match lead {
            0xe0 => (0xa0, 0xbf),
            0xed => (0x80, 0x9f),
            0xf0 => (0x90, 0xbf),
            0xf4 => (0x80, 0x8f),
            _ => (0x80, 0xbf),
        };
        let valid = data[n - back + 1..].iter().enumerate().all(|(i, &b)| {
            let (l, h) = if i == 0 { (lo, hi) } else { (0x80, 0xbf) };
            (l..=h).contains(&b)
        });
        return if valid { back } else { 0 };
    }
    0
}

/// The URL API's components, in the order `js/url.js` reads them.
fn url_components(url: &Url) -> Vec<String> {
    use url::quirks as q;
    vec![
        q::href(url).to_owned(),
        q::origin(url),
        q::protocol(url).to_owned(),
        q::username(url).to_owned(),
        q::password(url).to_owned(),
        q::host(url).to_owned(),
        q::hostname(url).to_owned(),
        q::port(url).to_owned(),
        q::pathname(url).to_owned(),
        q::search(url).to_owned(),
        q::hash(url).to_owned(),
    ]
}

/// `native.urlParse(input, base?)`: the components, or undefined when the
/// input (or the base) is not a valid URL.
fn url_parse(input: String, base: Opt<String>) -> Option<Vec<String>> {
    let url = match base.0 {
        Some(base) => Url::parse(&base).ok()?.join(&input).ok()?,
        None => Url::parse(&input).ok()?,
    };
    Some(url_components(&url))
}

/// `native.urlSet(href, index, value)`: runs the URL API setter for component
/// `index` and returns the new components. Setters that reject their input
/// leave the URL unchanged, except `href` (index 0), which returns undefined.
fn url_set(href: String, index: u32, value: String) -> Option<Vec<String>> {
    use url::quirks as q;
    let mut url = Url::parse(&href).ok()?;
    match index {
        0 => q::set_href(&mut url, &value).ok()?,
        2 => drop(q::set_protocol(&mut url, &value)),
        3 => drop(q::set_username(&mut url, &value)),
        4 => drop(q::set_password(&mut url, &value)),
        5 => drop(q::set_host(&mut url, &value)),
        6 => drop(q::set_hostname(&mut url, &value)),
        7 => drop(q::set_port(&mut url, &value)),
        8 => q::set_pathname(&mut url, &value),
        9 => q::set_search(&mut url, &value),
        10 => q::set_hash(&mut url, &value),
        _ => return None,
    }
    Some(url_components(&url))
}

/// `native.formParse(query)`: application/x-www-form-urlencoded pairs,
/// flattened to `[name, value, name, value, ...]`.
fn form_parse(input: String) -> Vec<String> {
    url::form_urlencoded::parse(input.as_bytes())
        .flat_map(|(k, v)| [k.into_owned(), v.into_owned()])
        .collect()
}

/// `native.formSerialize([name, value, ...])`.
fn form_serialize(pairs: Vec<String>) -> String {
    let mut out = url::form_urlencoded::Serializer::new(String::new());
    for [name, value] in pairs.as_chunks::<2>().0 {
        out.append_pair(name, value);
    }
    out.finish()
}

/// `native.randomFill(bytes)` fills the view from a CSPRNG.
fn random_fill<'js>(ctx: Ctx<'js>, target: TypedArray<'js, u8>) -> rquickjs::Result<()> {
    let Some(raw) = target.as_raw() else {
        return Err(Exception::throw_type(&ctx, "The ArrayBuffer is detached"));
    };
    // SAFETY: the view's bytes stay valid while no JavaScript runs, and none
    // runs during this call.
    rand::rng().fill_bytes(unsafe { &mut *raw.as_ptr() });
    Ok(())
}

/// `native.randomUUID()`: an RFC 9562 version 4 UUID.
fn random_uuid() -> String {
    let mut b = [0u8; 16];
    rand::rng().fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// `native.digest(name, bytes)` for SHA-256, SHA-384 and SHA-512.
fn digest<'js>(
    ctx: Ctx<'js>,
    algorithm: String,
    data: TypedArray<'js, u8>,
) -> rquickjs::Result<ArrayBuffer<'js>> {
    let hash: fn(&[u8]) -> Vec<u8> = match algorithm.as_str() {
        "SHA-256" => |d| Sha256::digest(d).to_vec(),
        "SHA-384" => |d| Sha384::digest(d).to_vec(),
        "SHA-512" => |d| Sha512::digest(d).to_vec(),
        _ => {
            return Err(Exception::throw_dom(
                &ctx,
                "NotSupportedError",
                "Unrecognized algorithm name",
            ))
        }
    };
    let Some(raw) = data.as_raw() else {
        return Err(Exception::throw_type(&ctx, "The ArrayBuffer is detached"));
    };
    // SAFETY: no JavaScript runs while the slice is hashed.
    let out = hash(unsafe { raw.as_ref() });
    ArrayBuffer::new(ctx, out)
}

/// The bytes of a JavaScript BufferSource (an ArrayBuffer or any
/// ArrayBufferView), copied out.
pub(crate) fn buffer_source<'js>(ctx: &Ctx<'js>, value: &Value<'js>) -> rquickjs::Result<Vec<u8>> {
    let detached = || Exception::throw_type(ctx, "The ArrayBuffer is detached");
    let Some(object) = value.as_object() else {
        return Err(Exception::throw_type(
            ctx,
            "Expected an ArrayBuffer or an ArrayBufferView",
        ));
    };
    if let Some(array) = object.as_typed_array::<u8>() {
        // SAFETY: copied before any JavaScript runs.
        return unsafe { array.as_bytes() }
            .map(<[u8]>::to_vec)
            .ok_or_else(detached);
    }
    if let Some(buffer) = object.as_array_buffer() {
        // SAFETY: as above.
        return unsafe { buffer.as_bytes() }
            .map(<[u8]>::to_vec)
            .ok_or_else(detached);
    }
    // Other typed arrays and DataView: their window on the underlying buffer.
    let view = (
        object.get::<_, Value>("buffer"),
        object.get::<_, Option<usize>>("byteOffset"),
        object.get::<_, Option<usize>>("byteLength"),
    );
    if let (Ok(buffer), Ok(Some(offset)), Ok(Some(length))) = view {
        if let Some(buffer) = buffer.as_object().and_then(|o| o.as_array_buffer()) {
            // SAFETY: as above.
            let all = unsafe { buffer.as_bytes() }.ok_or_else(detached)?;
            if let Some(window) = all.get(offset..offset.saturating_add(length)) {
                return Ok(window.to_vec());
            }
        }
    }
    Err(Exception::throw_type(
        ctx,
        "Expected an ArrayBuffer or an ArrayBufferView",
    ))
}

#[cfg(test)]
mod tests {
    use super::incomplete_tail;

    #[test]
    fn incomplete_tails() {
        assert_eq!(incomplete_tail(b""), 0);
        assert_eq!(incomplete_tail(b"abc"), 0);
        assert_eq!(incomplete_tail(&[0x61, 0xe2]), 1);
        assert_eq!(incomplete_tail(&[0xe2, 0x82]), 2);
        assert_eq!(incomplete_tail(&[0xe2, 0x82, 0xac]), 0);
        assert_eq!(incomplete_tail(&[0xf0, 0x9f, 0x8f]), 3);
        assert_eq!(incomplete_tail(&[0xf0, 0x9f, 0x8f, 0xb0]), 0);
        // Not a valid prefix: decoded (with U+FFFD) right away.
        assert_eq!(incomplete_tail(&[0xe0, 0x80]), 0);
        assert_eq!(incomplete_tail(&[0xed, 0xa0]), 0);
        assert_eq!(incomplete_tail(&[0xff]), 0);
        assert_eq!(incomplete_tail(&[0x80, 0x80, 0x80]), 0);
    }
}
