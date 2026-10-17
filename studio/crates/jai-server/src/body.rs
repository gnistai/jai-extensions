//! Reading JSON request bodies, with errors in the API's shape.

use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use serde_json::{Map, Value};

use crate::error::ApiError;

/// The body as a JSON object. The size limit is enforced while reading it.
pub(crate) fn json_object(
    body: Result<Bytes, BytesRejection>,
) -> Result<Map<String, Value>, ApiError> {
    let bytes = body.map_err(ApiError::body)?;
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(ApiError::invalid(
            "invalid_json",
            "The body must be a JSON object.",
        )),
        Err(err) => Err(ApiError::invalid(
            "invalid_json",
            format!("The body is not valid JSON: {err}"),
        )),
    }
}

/// An optional field; JSON `null` counts as absent.
pub(crate) fn field<'a>(req: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    req.get(key).filter(|v| !v.is_null())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn objects_only() {
        assert!(json_object(Ok(Bytes::from_static(b"{\"a\":1}"))).is_ok());
        let err = json_object(Ok(Bytes::from_static(b"[1]"))).unwrap_err();
        assert_eq!(err.code, "invalid_json");
        let err = json_object(Ok(Bytes::from_static(b"{"))).unwrap_err();
        assert_eq!(err.code, "invalid_json");
    }

    #[test]
    fn null_is_absent() {
        let map: Map<String, Value> = serde_json::from_str(r#"{"a":null,"b":1}"#).unwrap();
        assert!(field(&map, "a").is_none());
        assert!(field(&map, "b").is_some());
        assert!(field(&map, "c").is_none());
    }
}
