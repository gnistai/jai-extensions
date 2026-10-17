//! Errors in OpenAI's shape: `{"error": {"message", "type", "code"}}`
//! (DESIGN.md section 7).

use std::borrow::Cow;

use axum::extract::rejection::BytesRejection;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use jai_engines::EngineError;
use serde_json::{json, Value};

/// The message of every `models_missing` error.
pub(crate) const MODELS_MISSING: &str =
    "Download the models first: open JAI Studio or run `jai models download`.";

/// The `type` field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ErrorType {
    InvalidRequest,
    Auth,
    Server,
    Unavailable,
}

impl ErrorType {
    fn as_str(self) -> &'static str {
        match self {
            ErrorType::InvalidRequest => "invalid_request_error",
            ErrorType::Auth => "auth_error",
            ErrorType::Server => "server_error",
            ErrorType::Unavailable => "unavailable_error",
        }
    }
}

/// One error response.
#[derive(Debug)]
pub(crate) struct ApiError {
    pub status: StatusCode,
    pub kind: ErrorType,
    pub code: Cow<'static, str>,
    pub message: String,
}

impl ApiError {
    pub fn new(
        status: StatusCode,
        kind: ErrorType,
        code: impl Into<Cow<'static, str>>,
        message: impl Into<String>,
    ) -> ApiError {
        ApiError {
            status,
            kind,
            code: code.into(),
            message: message.into(),
        }
    }

    /// 400 for a request that breaks a rule.
    pub fn invalid(code: &'static str, message: impl Into<String>) -> ApiError {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            ErrorType::InvalidRequest,
            code,
            message,
        )
    }

    pub fn bad_host() -> ApiError {
        ApiError::new(
            StatusCode::FORBIDDEN,
            ErrorType::Auth,
            "bad_host",
            "The Host header must name this server on 127.0.0.1, localhost or [::1].",
        )
    }

    pub fn bad_origin() -> ApiError {
        ApiError::new(
            StatusCode::FORBIDDEN,
            ErrorType::Auth,
            "bad_origin",
            "Web pages may not use JAI Studio; only browser extensions may.",
        )
    }

    pub fn unauthorized() -> ApiError {
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            ErrorType::Auth,
            "unauthorized",
            "Send the JAI Studio token as `Authorization: Bearer <token>` (see `jai token`).",
        )
    }

    pub fn models_missing() -> ApiError {
        ApiError::new(
            StatusCode::CONFLICT,
            ErrorType::Unavailable,
            "models_missing",
            MODELS_MISSING,
        )
    }

    pub fn not_found() -> ApiError {
        ApiError::new(
            StatusCode::NOT_FOUND,
            ErrorType::InvalidRequest,
            "not_found",
            "No such endpoint.",
        )
    }

    pub fn method_not_allowed() -> ApiError {
        ApiError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            ErrorType::InvalidRequest,
            "method_not_allowed",
            "This endpoint does not take that method.",
        )
    }

    pub fn shutting_down() -> ApiError {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorType::Unavailable,
            "shutting_down",
            "JAI Studio is shutting down.",
        )
    }

    pub fn internal(message: impl Into<String>) -> ApiError {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorType::Server,
            "internal_error",
            message,
        )
    }

    /// A body that could not be read, usually one over the size limit.
    pub fn body(rejection: BytesRejection) -> ApiError {
        if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                ErrorType::InvalidRequest,
                "body_too_large",
                "The request body is too large.",
            )
        } else {
            ApiError::invalid(
                "invalid_body",
                format!("Could not read the body: {rejection}"),
            )
        }
    }

    /// The scheduler could not provide the engine.
    pub fn engine(err: EngineError) -> ApiError {
        match err {
            EngineError::NotInstalled(what) => {
                tracing::warn!(%what, "engine not installed");
                ApiError::models_missing()
            }
            EngineError::StartFailed { kind, message, log } => {
                tracing::warn!(%kind, %message, log = %log.join("\n"), "engine failed to start");
                ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    ErrorType::Unavailable,
                    "engine_start_failed",
                    format!("The {kind} engine failed to start: {message}"),
                )
            }
            EngineError::ShuttingDown => ApiError::shutting_down(),
        }
    }

    /// The engine could not be reached, or broke off mid-response.
    pub fn upstream_io(engine: &str, err: &reqwest::Error) -> ApiError {
        tracing::warn!(engine, error = %err, "engine request failed");
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            ErrorType::Server,
            "upstream_error",
            format!("The {engine} engine stopped answering: {err}"),
        )
    }

    /// The engine answered with something that is not what its API promises.
    pub fn upstream_invalid(engine: &str, what: impl std::fmt::Display) -> ApiError {
        tracing::warn!(engine, %what, "unexpected engine response");
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            ErrorType::Server,
            "upstream_error",
            format!("The {engine} engine sent an unexpected response: {what}"),
        )
    }

    /// An error status from an engine. The engine's own 401 or 404 means
    /// Studio called it wrongly, never that the client did.
    pub fn upstream_status(engine: &str, status: StatusCode, body: &[u8]) -> ApiError {
        let (message, upstream_code) = upstream_error_parts(body);
        let message = if message.is_empty() {
            format!("The {engine} engine answered {status}.")
        } else {
            message
        };
        tracing::warn!(engine, %status, %message, "engine returned an error");
        match status.as_u16() {
            401 | 403 | 404 | 405 => ApiError::new(
                StatusCode::BAD_GATEWAY,
                ErrorType::Server,
                "upstream_error",
                format!("The {engine} engine refused Studio's request ({status}): {message}"),
            ),
            429 | 503 => ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorType::Unavailable,
                "engine_busy",
                message,
            ),
            400..=499 => ApiError::new(
                status,
                ErrorType::InvalidRequest,
                upstream_code.unwrap_or(Cow::Borrowed("upstream_rejected")),
                message,
            ),
            _ => ApiError::new(
                StatusCode::BAD_GATEWAY,
                ErrorType::Server,
                "upstream_error",
                message,
            ),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({
            "error": {
                "message": self.message,
                "type": self.kind.as_str(),
                "code": self.code,
            }
        });
        let mut res = (self.status, Json(body)).into_response();
        if self.status == StatusCode::UNAUTHORIZED {
            res.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"jai\""),
            );
        }
        res
    }
}

/// The message and code in an engine's error body. llama-server sends
/// `{"error": {"message", "type"}}`; sd-server sends `{"error": "..."}`,
/// sometimes with a separate `message`.
fn upstream_error_parts(body: &[u8]) -> (String, Option<Cow<'static, str>>) {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        let text = String::from_utf8_lossy(body);
        return (truncate(text.trim(), 500), None);
    };
    match value.get("error") {
        Some(Value::Object(err)) => {
            let message = err.get("message").and_then(Value::as_str).unwrap_or("");
            let code = err
                .get("type")
                .and_then(Value::as_str)
                .filter(|c| is_code(c))
                .map(|c| Cow::Owned(c.to_owned()));
            (truncate(message, 2000), code)
        }
        Some(Value::String(err)) => {
            let message = match value.get("message").and_then(Value::as_str) {
                Some(detail) => format!("{err}: {detail}"),
                None => err.clone(),
            };
            (truncate(&message, 2000), None)
        }
        _ => (truncate(&value.to_string(), 500), None),
    }
}

/// A plausible error code: short snake_case ASCII.
fn is_code(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn truncate(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn llama_error_body() {
        let body = br#"{"error":{"code":400,"message":"context too long","type":"exceed_context_size_error"}}"#;
        let e = ApiError::upstream_status("LLM", StatusCode::BAD_REQUEST, body);
        assert_eq!(e.status, StatusCode::BAD_REQUEST);
        assert_eq!(e.kind, ErrorType::InvalidRequest);
        assert_eq!(e.code, "exceed_context_size_error");
        assert_eq!(e.message, "context too long");
    }

    #[test]
    fn upstream_auth_is_not_the_clients_fault() {
        let body =
            br#"{"error":{"code":401,"message":"Invalid API Key","type":"authentication_error"}}"#;
        let e = ApiError::upstream_status("LLM", StatusCode::UNAUTHORIZED, body);
        assert_eq!(e.status, StatusCode::BAD_GATEWAY);
        assert_eq!(e.kind, ErrorType::Server);
    }

    #[test]
    fn sd_error_bodies() {
        let e = ApiError::upstream_status(
            "image",
            StatusCode::BAD_REQUEST,
            br#"{"error":"invalid generation parameters"}"#,
        );
        assert_eq!(e.status, StatusCode::BAD_REQUEST);
        assert_eq!(e.code, "upstream_rejected");
        assert_eq!(e.message, "invalid generation parameters");

        let e = ApiError::upstream_status(
            "image",
            StatusCode::INTERNAL_SERVER_ERROR,
            br#"{"error":"server_error","message":"boom"}"#,
        );
        assert_eq!(e.status, StatusCode::BAD_GATEWAY);
        assert_eq!(e.message, "server_error: boom");

        let e = ApiError::upstream_status(
            "image",
            StatusCode::TOO_MANY_REQUESTS,
            br#"{"error":"job queue is full"}"#,
        );
        assert_eq!(e.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(e.code, "engine_busy");
    }

    #[test]
    fn truncates_on_char_boundaries() {
        assert_eq!(truncate("æøå", 2), "æø…");
        assert_eq!(truncate("ab", 2), "ab");
    }
}
