//! `POST /v1/chat/completions`: checked, then forwarded to llama-server
//! (DESIGN.md section 7).
//!
//! Images are decoded and encoded again first (`chat_image`), before the GPU
//! is asked for, so llama-server only ever sees images Studio wrote.
//!
//! The lease is held until the upstream response ends: for a stream, the
//! response body owns it. When the client goes away, hyper drops the handler
//! or the body, which drops the lease and the upstream request with it, and
//! llama-server stops generating when its connection closes.

use std::convert::Infallible;
use std::pin::Pin;
use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::extract::rejection::BytesRejection;
use axum::extract::State;
use axum::http::{header, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine as _;
use futures::{Stream, StreamExt};
use jai_engines::{EngineKind, Lease};
use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;

use crate::body::{field, json_object};
use crate::chat_image::{self, MAX_IMAGE_BYTES};
use crate::error::ApiError;
use crate::server::Shared;
use crate::status::{require_installed, LLM_MODEL};

/// Name of the engine in error messages.
const ENGINE: &str = "LLM";
/// Upper bound for `max_tokens` and `max_completion_tokens`, and the value
/// sent when the client sets neither.
const MAX_TOKENS: u64 = 8192;
/// Images per request.
const MAX_IMAGES: usize = 8;
const REASONING_EFFORTS: [&str; 5] = ["none", "low", "medium", "high", "xhigh"];
/// Fields forwarded unchanged. Anything not checked or listed here is dropped.
const PASS_THROUGH: [&str; 8] = [
    "chat_template_kwargs",
    "response_format",
    "temperature",
    "top_p",
    "top_k",
    "presence_penalty",
    "seed",
    "stop",
];
/// Accepted image types and their leading bytes.
const IMAGE_TYPES: [(&str, &[&[u8]]); 4] = [
    ("png", &[b"\x89PNG\r\n\x1a\n"]),
    ("jpeg", &[b"\xff\xd8\xff"]),
    ("gif", &[b"GIF87a", b"GIF89a"]),
    ("bmp", &[b"BM"]),
];
/// A partial SSE line longer than this is passed on as it is.
const MAX_SSE_LINE: usize = 1 << 20;

/// A checked request, ready for llama-server.
#[derive(Debug)]
pub(crate) struct Checked {
    pub body: Map<String, Value>,
    pub stream: bool,
}

/// `POST /v1/chat/completions`
pub(crate) async fn completions(
    State(shared): State<Shared>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let arrived = Instant::now();
    let mut checked = check(json_object(body)?)?;
    checked.body = reencode_images(&shared, checked.body).await?;
    require_installed(&shared.core, EngineKind::Llm).await?;
    let lease = shared
        .core
        .scheduler
        .acquire(EngineKind::Llm)
        .await
        .map_err(ApiError::engine)?;
    forward(&shared, lease, checked, arrived).await
}

/// Applies the request rules and builds the body for llama-server.
pub(crate) fn check(mut req: Map<String, Value>) -> Result<Checked, ApiError> {
    match field(&req, "model") {
        None => {}
        Some(Value::String(m)) if m == LLM_MODEL || m == "default" => {}
        Some(other) => {
            return Err(ApiError::invalid(
                "model_not_found",
                format!("Unknown model {other}: use \"{LLM_MODEL}\"."),
            ))
        }
    }
    if let Some(n) = field(&req, "n") {
        if n.as_u64() != Some(1) {
            return Err(ApiError::invalid("invalid_value", "n must be 1."));
        }
    }
    let stream = match field(&req, "stream") {
        None => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => {
            return Err(ApiError::invalid(
                "invalid_value",
                "stream must be true or false.",
            ))
        }
    };
    check_messages(field(&req, "messages"))?;

    let mut body = Map::new();
    let mut limited = false;
    for key in ["max_tokens", "max_completion_tokens"] {
        if let Some(value) = field(&req, key) {
            match value.as_u64() {
                Some(n) if n >= 1 => {
                    body.insert(key.to_owned(), json!(n.min(MAX_TOKENS)));
                    limited = true;
                }
                _ => {
                    return Err(ApiError::invalid(
                        "invalid_value",
                        format!("{key} must be a positive integer."),
                    ))
                }
            }
        }
    }
    if !limited {
        body.insert("max_tokens".to_owned(), json!(MAX_TOKENS));
    }
    if let Some(effort) = field(&req, "reasoning_effort") {
        match effort.as_str() {
            Some(e) if REASONING_EFFORTS.contains(&e) => {
                body.insert("reasoning_effort".to_owned(), effort.clone());
            }
            _ => {
                return Err(ApiError::invalid(
                    "invalid_value",
                    "reasoning_effort must be none, low, medium, high or xhigh.",
                ))
            }
        }
    }
    for key in PASS_THROUGH {
        if let Some(value) = req.remove(key).filter(|v| !v.is_null()) {
            body.insert(key.to_owned(), value);
        }
    }
    if stream {
        body.insert("stream".to_owned(), Value::Bool(true));
    }
    if let Some(messages) = req.remove("messages") {
        body.insert("messages".to_owned(), messages);
    }
    Ok(Checked { body, stream })
}

/// `messages` is a non-empty array of objects whose content is a string or
/// text and image parts, with at most eight images in all.
fn check_messages(messages: Option<&Value>) -> Result<(), ApiError> {
    let messages = match messages {
        Some(Value::Array(m)) if !m.is_empty() => m,
        Some(Value::Array(_)) => {
            return Err(ApiError::invalid(
                "invalid_messages",
                "messages must not be empty.",
            ))
        }
        Some(_) => {
            return Err(ApiError::invalid(
                "invalid_messages",
                "messages must be an array.",
            ))
        }
        None => {
            return Err(ApiError::invalid(
                "missing_messages",
                "messages is required.",
            ))
        }
    };
    let mut images = 0;
    for (i, message) in messages.iter().enumerate() {
        let Some(message) = message.as_object() else {
            return Err(ApiError::invalid(
                "invalid_messages",
                format!("messages[{i}] must be an object."),
            ));
        };
        match message.get("content") {
            None | Some(Value::Null) | Some(Value::String(_)) => {}
            Some(Value::Array(parts)) => {
                for (j, part) in parts.iter().enumerate() {
                    if check_part(part).map_err(|why| {
                        ApiError::invalid(why.0, format!("messages[{i}].content[{j}]: {}", why.1))
                    })? {
                        images += 1;
                    }
                }
            }
            Some(_) => {
                return Err(ApiError::invalid(
                    "invalid_messages",
                    format!("messages[{i}].content must be a string or an array of parts."),
                ))
            }
        }
    }
    if images > MAX_IMAGES {
        return Err(ApiError::invalid(
            "too_many_images",
            format!("At most {MAX_IMAGES} images per request; this one has {images}."),
        ));
    }
    Ok(())
}

/// One content part: `text`, or `image_url` with a data URL. Ok(true) for an
/// image. Other part types could make llama-server fetch URLs or read
/// files, so they are refused.
fn check_part(part: &Value) -> Result<bool, (&'static str, String)> {
    let Some(part) = part.as_object() else {
        return Err(("invalid_content", "a part must be an object".into()));
    };
    match part.get("type").and_then(Value::as_str) {
        Some("text") => match part.get("text") {
            Some(Value::String(_)) => Ok(false),
            _ => Err(("invalid_content", "text must be a string".into())),
        },
        Some("image_url") => {
            let url = match part.get("image_url") {
                Some(Value::Object(o)) => o.get("url").and_then(Value::as_str),
                Some(Value::String(s)) => Some(s.as_str()),
                _ => None,
            };
            let url = url.ok_or(("invalid_image", "image_url.url is required".to_owned()))?;
            check_image_url(url).map_err(|why| ("invalid_image", why))?;
            Ok(true)
        }
        Some(other) => Err((
            "unsupported_content",
            format!("parts of type {other:?} are not accepted; use \"text\" and \"image_url\""),
        )),
        None => Err(("invalid_content", "type is required".into())),
    }
}

/// `data:image/(png|jpeg|gif|bmp);base64,…` whose first bytes are that
/// type: a quick check, before `reencode_images` decodes the whole image.
fn check_image_url(url: &str) -> Result<(), String> {
    let (subtype, data) = data_url(url)?;
    let Some(&(_, signatures)) = IMAGE_TYPES.iter().find(|(t, _)| *t == subtype) else {
        return Err(format!(
            "image/{subtype} is not accepted; send PNG, JPEG, GIF or BMP"
        ));
    };
    if data.len() / 4 * 3 > MAX_IMAGE_BYTES {
        return Err("an image may be at most 40 MB".into());
    }
    let head_len = data.len().min(16) / 4 * 4;
    let head = base64::engine::general_purpose::STANDARD
        .decode(&data.as_bytes()[..head_len])
        .map_err(|_| "the image data is not valid base64".to_owned())?;
    if !signatures.iter().any(|s| head.starts_with(s)) {
        return Err(format!("the image data is not a {subtype} image"));
    }
    Ok(())
}

/// The subtype and the base64 data of `data:image/<subtype>;base64,<data>`.
fn data_url(url: &str) -> Result<(&str, &str), String> {
    let Some(rest) = url.strip_prefix("data:image/") else {
        return Err(
            "images must be data:image/…;base64 URLs; remote URLs and file paths \
                    are not accepted"
                .into(),
        );
    };
    rest.split_once(";base64,")
        .ok_or_else(|| "images must be base64 data URLs".into())
}

/// Replaces every image of a checked request by its copy from
/// `chat_image::reencode`. The work runs on the blocking pool, at most
/// `IMAGE_WORK` requests at a time.
async fn reencode_images(
    shared: &Shared,
    mut body: Map<String, Value>,
) -> Result<Map<String, Value>, ApiError> {
    if !has_images(&body) {
        return Ok(body);
    }
    let _turn = shared
        .image_work
        .acquire()
        .await
        .map_err(|_| ApiError::shutting_down())?;
    tokio::task::spawn_blocking(move || {
        reencode_all(&mut body)?;
        Ok(body)
    })
    .await
    .map_err(|e| ApiError::internal(format!("the image check stopped: {e}")))?
}

/// The image parts of a checked request, with their message and part index.
fn image_urls(body: &mut Map<String, Value>) -> Vec<(usize, usize, &mut String)> {
    let mut urls = Vec::new();
    let Some(Value::Array(messages)) = body.get_mut("messages") else {
        return urls;
    };
    for (i, message) in messages.iter_mut().enumerate() {
        let Some(Value::Array(parts)) = message.get_mut("content") else {
            continue;
        };
        for (j, part) in parts.iter_mut().enumerate() {
            if part.get("type").and_then(Value::as_str) != Some("image_url") {
                continue;
            }
            let url = match part.get_mut("image_url") {
                Some(Value::Object(o)) => o.get_mut("url"),
                Some(s @ Value::String(_)) => Some(s),
                _ => None,
            };
            if let Some(Value::String(url)) = url {
                urls.push((i, j, url));
            }
        }
    }
    urls
}

fn has_images(body: &Map<String, Value>) -> bool {
    body.get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| m.get("content").and_then(Value::as_array))
        .flatten()
        .any(|part| part.get("type").and_then(Value::as_str) == Some("image_url"))
}

fn reencode_all(body: &mut Map<String, Value>) -> Result<(), ApiError> {
    for (i, j, url) in image_urls(body) {
        let fresh = data_url(url)
            .and_then(|(subtype, data)| chat_image::reencode(subtype, data))
            .map_err(|why| {
                ApiError::invalid(
                    "invalid_image",
                    format!("messages[{i}].content[{j}]: {why}"),
                )
            })?;
        *url = fresh;
    }
    Ok(())
}

/// Sends the request to llama-server and relays the answer.
async fn forward(
    shared: &Shared,
    lease: Lease,
    checked: Checked,
    arrived: Instant,
) -> Result<Response, ApiError> {
    let Checked { body, stream } = checked;
    let url = format!(
        "{}/v1/chat/completions",
        lease.base_url.trim_end_matches('/')
    );
    let mut request = shared.core.http.post(url).json(&body);
    drop(body);
    if let Some(key) = lease.api_key.as_deref() {
        request = request.bearer_auth(key);
    }
    let upstream = request
        .send()
        .await
        .map_err(|e| ApiError::upstream_io(ENGINE, &e))?;
    let status = upstream.status();
    if !status.is_success() {
        let body = upstream.bytes().await.unwrap_or_default();
        return Err(ApiError::upstream_status(ENGINE, status, &body));
    }
    if stream {
        tracing::info!(ms = millis(arrived), "chat stream started");
        return Ok(sse_response(upstream, lease, shared.shutdown.clone()));
    }
    let bytes = upstream
        .bytes()
        .await
        .map_err(|e| ApiError::upstream_io(ENGINE, &e))?;
    drop(lease);
    let mut value: Value =
        serde_json::from_slice(&bytes).map_err(|e| ApiError::upstream_invalid(ENGINE, e))?;
    let Some(object) = value.as_object_mut() else {
        return Err(ApiError::upstream_invalid(ENGINE, "not a JSON object"));
    };
    object.insert("model".to_owned(), Value::String(LLM_MODEL.to_owned()));
    tracing::info!(ms = millis(arrived), "chat completed");
    Ok(Json(value).into_response())
}

fn millis(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// An SSE response that relays llama-server's stream with the model renamed,
/// holding the lease until the stream ends or the client leaves.
fn sse_response(
    upstream: reqwest::Response,
    lease: Lease,
    shutdown: CancellationToken,
) -> Response {
    let relay = Relay {
        upstream: Box::pin(upstream.bytes_stream()),
        lines: SseRewriter::default(),
        lease: Some(lease),
        shutdown,
    };
    let mut res = Body::from_stream(relay.into_stream()).into_response();
    let headers = res.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    res
}

type ByteStream = Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>;

/// The state of one relayed stream. `lease` is None once it has ended.
struct Relay {
    upstream: ByteStream,
    lines: SseRewriter,
    lease: Option<Lease>,
    shutdown: CancellationToken,
}

/// What happened next upstream.
enum Step {
    Chunk(Bytes),
    End,
    Failed(String),
    Shutdown,
}

impl Relay {
    fn into_stream(self) -> impl Stream<Item = Result<Bytes, Infallible>> + Send + 'static {
        futures::stream::unfold(self, |mut relay| async move {
            relay.lease.as_ref()?;
            loop {
                let step = tokio::select! {
                    chunk = relay.upstream.next() => match chunk {
                        Some(Ok(bytes)) => Step::Chunk(bytes),
                        Some(Err(err)) => Step::Failed(err.to_string()),
                        None => Step::End,
                    },
                    () = relay.shutdown.cancelled() => Step::Shutdown,
                };
                let out = match step {
                    Step::Chunk(bytes) => {
                        let out = relay.lines.push(&bytes);
                        if out.is_empty() {
                            continue;
                        }
                        out
                    }
                    Step::End => {
                        tracing::debug!("chat stream finished");
                        relay.lease = None;
                        relay.lines.flush()
                    }
                    Step::Failed(message) => {
                        tracing::warn!(error = %message, "LLM stream broke off");
                        relay.lease = None;
                        let mut out = relay.lines.abort();
                        out.extend(error_event(
                            "server_error",
                            "upstream_error",
                            &format!("The LLM engine stopped answering: {message}"),
                        ));
                        out
                    }
                    Step::Shutdown => {
                        relay.lease = None;
                        let mut out = relay.lines.abort();
                        out.extend(error_event(
                            "unavailable_error",
                            "shutting_down",
                            "JAI Studio is shutting down.",
                        ));
                        out
                    }
                };
                if out.is_empty() {
                    return None;
                }
                return Some((Ok(Bytes::from(out)), relay));
            }
        })
    }
}

/// `data: {"error": …}`, the way OpenAI reports an error inside a stream.
/// It must start a new event: see `SseRewriter::abort`.
fn error_event(kind: &str, code: &str, message: &str) -> Vec<u8> {
    let event = json!({ "error": { "message": message, "type": kind, "code": code } });
    format!("data: {event}\n\n").into_bytes()
}

/// Splits an SSE byte stream into lines and renames the model in each JSON
/// `data:` line.
#[derive(Default)]
struct SseRewriter {
    /// The start of a line whose end has not arrived.
    pending: Vec<u8>,
    /// The start of the current line was passed on already (it was longer
    /// than `MAX_SSE_LINE`).
    mid_line: bool,
    /// What was passed on ends inside an event: its last line was not blank.
    mid_event: bool,
}

impl SseRewriter {
    /// The complete lines in `chunk` (after what is pending), rewritten. A
    /// partial line waits for the next chunk.
    fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut start = 0;
        while let Some(pos) = self.pending[start..].iter().position(|&b| b == b'\n') {
            let end = start + pos;
            let line = &self.pending[start..end];
            if self.mid_line {
                // The end of a line whose start went out as it was.
                out.extend_from_slice(line);
                out.push(b'\n');
                self.mid_event = true;
            } else {
                rewrite_line(line, &mut out);
                self.mid_event = !line.strip_suffix(b"\r").unwrap_or(line).is_empty();
            }
            self.mid_line = false;
            start = end + 1;
        }
        self.pending.drain(..start);
        if self.pending.len() > MAX_SSE_LINE {
            out.append(&mut self.pending);
            self.mid_line = true;
            self.mid_event = true;
        }
        out
    }

    /// Whatever is left at the end of the stream.
    fn flush(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        let rest = std::mem::take(&mut self.pending);
        if self.mid_line {
            out.extend_from_slice(&rest);
            out.push(b'\n');
        } else if !rest.is_empty() {
            rewrite_line(&rest, &mut out);
        }
        self.mid_line = false;
        out
    }

    /// Ends the stream early, before an error event: a partial line that was
    /// never passed on is dropped, and a half-sent line or event is ended, so
    /// the error is an event of its own that clients can parse.
    fn abort(&mut self) -> Vec<u8> {
        self.pending.clear();
        let mut out = Vec::new();
        if self.mid_line {
            out.push(b'\n');
            self.mid_line = false;
        }
        if self.mid_event {
            out.push(b'\n');
            self.mid_event = false;
        }
        out
    }
}

/// Appends one line and its newline, with `model` set in a JSON chunk.
fn rewrite_line(line: &[u8], out: &mut Vec<u8>) {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if let Some(rewritten) = line.strip_prefix(b"data:").and_then(rename_model) {
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(&rewritten);
    } else {
        out.extend_from_slice(line);
    }
    out.push(b'\n');
}

/// The JSON payload with `model` set, if it is a completion chunk.
fn rename_model(payload: &[u8]) -> Option<Vec<u8>> {
    let payload = payload.strip_prefix(b" ").unwrap_or(payload);
    let Ok(Value::Object(mut chunk)) = serde_json::from_slice::<Value>(payload) else {
        return None;
    };
    if !chunk.contains_key("model") && !chunk.contains_key("choices") {
        return None;
    }
    chunk.insert("model".to_owned(), Value::String(LLM_MODEL.to_owned()));
    serde_json::to_vec(&chunk).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG_URL: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

    fn req(json: Value) -> Map<String, Value> {
        json.as_object().unwrap().clone()
    }

    fn msg_with(parts: Value) -> Map<String, Value> {
        req(json!({ "messages": [{ "role": "user", "content": parts }] }))
    }

    fn code(r: Result<Checked, ApiError>) -> String {
        r.unwrap_err().code.into_owned()
    }

    #[test]
    fn minimal_request() {
        let c = check(req(
            json!({ "messages": [{ "role": "user", "content": "hi" }] }),
        ))
        .unwrap();
        assert!(!c.stream);
        assert_eq!(c.body["max_tokens"], 8192);
        assert!(c.body.get("model").is_none());
        assert!(c.body.get("stream").is_none());
        assert_eq!(c.body["messages"][0]["content"], "hi");
    }

    #[test]
    fn model_and_n() {
        let base = json!({ "messages": [{ "role": "user", "content": "hi" }] });
        for ok in [json!("qwen3.8-27b"), json!("default"), Value::Null] {
            let mut r = req(base.clone());
            r.insert("model".into(), ok);
            assert!(check(r).is_ok());
        }
        let mut r = req(base.clone());
        r.insert("model".into(), json!("gpt-4o"));
        assert_eq!(code(check(r)), "model_not_found");
        let mut r = req(base.clone());
        r.insert("n".into(), json!(2));
        assert_eq!(code(check(r)), "invalid_value");
        let mut r = req(base);
        r.insert("n".into(), json!(1));
        assert!(check(r).is_ok());
    }

    #[test]
    fn messages_are_required() {
        assert_eq!(code(check(req(json!({})))), "missing_messages");
        assert_eq!(
            code(check(req(json!({ "messages": [] })))),
            "invalid_messages"
        );
        assert_eq!(
            code(check(req(json!({ "messages": "hi" })))),
            "invalid_messages"
        );
        assert_eq!(
            code(check(req(json!({ "messages": [1] })))),
            "invalid_messages"
        );
    }

    #[test]
    fn token_limits_are_clamped() {
        let c = check(req(json!({
            "messages": [{ "role": "user", "content": "hi" }],
            "max_tokens": 100000, "max_completion_tokens": 50,
        })))
        .unwrap();
        assert_eq!(c.body["max_tokens"], 8192);
        assert_eq!(c.body["max_completion_tokens"], 50);
        let c = check(req(json!({
            "messages": [{ "role": "user", "content": "hi" }],
            "max_completion_tokens": 9000,
        })))
        .unwrap();
        assert_eq!(c.body["max_completion_tokens"], 8192);
        assert!(c.body.get("max_tokens").is_none());
        for bad in [json!(0), json!(-1), json!(1.5), json!("10")] {
            let r = req(
                json!({ "messages": [{ "role": "user", "content": "hi" }], "max_tokens": bad }),
            );
            assert_eq!(code(check(r)), "invalid_value");
        }
    }

    #[test]
    fn reasoning_effort() {
        for ok in REASONING_EFFORTS {
            let r = req(
                json!({ "messages": [{ "role": "user", "content": "hi" }], "reasoning_effort": ok }),
            );
            assert_eq!(check(r).unwrap().body["reasoning_effort"], ok);
        }
        for bad in ["minimal", "max", ""] {
            let r = req(
                json!({ "messages": [{ "role": "user", "content": "hi" }], "reasoning_effort": bad }),
            );
            assert_eq!(code(check(r)), "invalid_value");
        }
    }

    #[test]
    fn pass_through_and_drop() {
        let c = check(req(json!({
            "messages": [{ "role": "user", "content": "hi" }],
            "stream": true,
            "chat_template_kwargs": { "enable_thinking": false },
            "response_format": { "type": "json_object" },
            "temperature": 0.7, "top_p": 0.8, "top_k": 20, "presence_penalty": 0.5,
            "seed": 3, "stop": ["\n\n"],
            "n_probs": 5, "id_slot": 0, "user": "someone", "logit_bias": {},
        })))
        .unwrap();
        assert!(c.stream);
        assert_eq!(c.body["stream"], true);
        for key in PASS_THROUGH {
            assert!(c.body.contains_key(key), "{key}");
        }
        for key in ["n_probs", "id_slot", "user", "logit_bias"] {
            assert!(!c.body.contains_key(key), "{key}");
        }
        let r = req(json!({ "messages": [{ "role": "user", "content": "hi" }], "stream": "yes" }));
        assert_eq!(code(check(r)), "invalid_value");
    }

    #[test]
    fn image_parts() {
        let ok = msg_with(json!([
            { "type": "text", "text": "what is this?" },
            { "type": "image_url", "image_url": { "url": PNG_URL } },
        ]));
        assert!(check(ok).is_ok());

        let remote = msg_with(
            json!([{ "type": "image_url", "image_url": { "url": "https://example.com/a.png" } }]),
        );
        assert_eq!(code(check(remote)), "invalid_image");
        let file = msg_with(
            json!([{ "type": "image_url", "image_url": { "url": "file:///etc/passwd" } }]),
        );
        assert_eq!(code(check(file)), "invalid_image");
        let webp = msg_with(
            json!([{ "type": "image_url", "image_url": { "url": "data:image/webp;base64,UklGRiQAAABXRUJQVlA4IBgAAAAwAQCdASoBAAEAAwA0JaQAA3AA/vuUAAA=" } }]),
        );
        assert_eq!(code(check(webp)), "invalid_image");
        // WebP bytes labelled as PNG.
        let disguised = msg_with(
            json!([{ "type": "image_url", "image_url": { "url": "data:image/png;base64,UklGRiQAAABXRUJQVlA4IBgAAAAwAQCdASoBAAEAAwA0JaQAA3AA/vuUAAA=" } }]),
        );
        assert_eq!(code(check(disguised)), "invalid_image");
        let not_base64 = msg_with(
            json!([{ "type": "image_url", "image_url": { "url": "data:image/png,rawbytes" } }]),
        );
        assert_eq!(code(check(not_base64)), "invalid_image");
        let audio = msg_with(
            json!([{ "type": "input_audio", "input_audio": { "data": "AAAA", "format": "wav" } }]),
        );
        assert_eq!(code(check(audio)), "unsupported_content");
        let video = msg_with(
            json!([{ "type": "video_url", "video_url": { "url": "https://example.com/v.mp4" } }]),
        );
        assert_eq!(code(check(video)), "unsupported_content");
    }

    #[test]
    fn at_most_eight_images() {
        let image = json!({ "type": "image_url", "image_url": { "url": PNG_URL } });
        let eight = msg_with(Value::Array(vec![image.clone(); 8]));
        assert!(check(eight).is_ok());
        let nine = msg_with(Value::Array(vec![image; 9]));
        assert_eq!(code(check(nine)), "too_many_images");
    }

    #[test]
    fn image_signatures() {
        assert!(check_image_url(PNG_URL).is_ok());
        assert!(check_image_url("data:image/jpeg;base64,/9j/4AAQSkZJRgABAQ==").is_ok());
        assert!(check_image_url("data:image/gif;base64,R0lGODlhAQABAAAAACw=").is_ok());
        assert!(check_image_url("data:image/bmp;base64,Qk06AAAAAAAAADYAAAA=").is_ok());
        assert!(check_image_url("data:image/jpg;base64,/9j/4AAQSkZJRgABAQ==").is_err());
        assert!(check_image_url("data:image/png;base64,").is_err());
        assert!(check_image_url("data:image/png;base64,!!!!!!!!").is_err());
    }

    #[test]
    fn sse_lines_are_rewritten_across_chunks() {
        let mut sse = SseRewriter::default();
        let mut out = sse.push(b"data: {\"choices\":[],\"model\":\"Qwen.gguf\"");
        assert!(out.is_empty());
        out.extend(sse.push(b",\"id\":\"x\"}\r\n\r\ndata: [DONE]\n"));
        out.extend(sse.push(b"\n"));
        out.extend(sse.flush());
        let text = String::from_utf8(out).unwrap();
        let mut lines = text.split('\n');
        let first: Value =
            serde_json::from_str(lines.next().unwrap().strip_prefix("data: ").unwrap()).unwrap();
        assert_eq!(first["model"], "qwen3.8-27b");
        assert_eq!(first["id"], "x");
        assert_eq!(lines.next(), Some(""));
        assert_eq!(lines.next(), Some("data: [DONE]"));
        assert_eq!(lines.next(), Some(""));
    }

    /// The events of an SSE text as a client parses them: data lines joined
    /// with newlines, an unterminated event at the end dropped.
    fn sse_events(text: &[u8]) -> Vec<String> {
        let text = String::from_utf8_lossy(text);
        let mut events = Vec::new();
        let mut data: Vec<&str> = Vec::new();
        for line in text.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.is_empty() {
                if !data.is_empty() {
                    events.push(data.join("\n"));
                    data.clear();
                }
            } else if let Some(d) = line.strip_prefix("data:") {
                data.push(d.strip_prefix(' ').unwrap_or(d));
            }
        }
        events
    }

    fn assert_error_last(out: &[u8]) {
        let events = sse_events(out);
        for event in &events {
            serde_json::from_str::<Value>(event)
                .unwrap_or_else(|e| panic!("{e}: {event:?} in {}", String::from_utf8_lossy(out)));
        }
        let last: Value = serde_json::from_str(events.last().unwrap()).unwrap();
        assert_eq!(last["error"]["code"], "upstream_error", "{last}");
    }

    fn broke_off(sse: &mut SseRewriter, mut out: Vec<u8>) -> Vec<u8> {
        out.extend(sse.abort());
        out.extend(error_event("server_error", "upstream_error", "gone"));
        out
    }

    /// SERVER-JS-7: an error after half a line is an event of its own.
    #[test]
    fn an_error_after_a_partial_line_is_an_event_of_its_own() {
        let mut sse = SseRewriter::default();
        let out = sse.push(
            b"data: {\"choices\":[],\"model\":\"m\"}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"hel",
        );
        let out = broke_off(&mut sse, out);
        assert_error_last(&out);
        assert_eq!(sse_events(&out).len(), 2);
        assert!(!String::from_utf8_lossy(&out).contains("hel"));
    }

    /// The blank line that ends an event had not arrived.
    #[test]
    fn an_error_before_the_blank_line_ends_the_event_first() {
        let mut sse = SseRewriter::default();
        let out = sse.push(b"data: {\"choices\":[],\"model\":\"m\"}\r\n");
        let out = broke_off(&mut sse, out);
        assert_error_last(&out);
        assert_eq!(sse_events(&out).len(), 2);

        // Between events nothing needs ending.
        let mut sse = SseRewriter::default();
        let out = sse.push(b"data: {\"choices\":[]}\n\n");
        assert!(sse.abort().is_empty());
        assert_error_last(&broke_off(&mut sse, out));
    }

    #[test]
    fn an_error_inside_an_overlong_line_ends_it() {
        let mut sse = SseRewriter::default();
        let mut out = sse.push(b"data: {\"choices\":[]}\n\ndata: \"");
        out.extend(sse.push(&vec![b'x'; MAX_SSE_LINE + 1]));
        assert!(out.len() > MAX_SSE_LINE, "the long line is passed on");
        let out = broke_off(&mut sse, out);
        let events = sse_events(&out);
        assert_eq!(events.len(), 3);
        let last: Value = serde_json::from_str(&events[2]).unwrap();
        assert_eq!(last["error"]["code"], "upstream_error");
    }

    #[test]
    fn sse_errors_and_comments_pass_through() {
        let mut sse = SseRewriter::default();
        let out = sse.push(b": ping\ndata: {\"error\":{\"message\":\"x\"}}\n");
        assert_eq!(out, b": ping\ndata: {\"error\":{\"message\":\"x\"}}\n");
        let out = sse.push(b"data: no newline at the end");
        assert!(out.is_empty());
        assert_eq!(sse.flush(), b"data: no newline at the end\n");
    }
}
