//! The rules for every request: Host, Origin, the bearer token, body limits,
//! error shapes, and `/health`, `/v1/models`, `/v1/status`.

mod common;

use axum::body::Body;
use axum::http::StatusCode;
use common::*;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_is_open() {
    let t = TestCore::new(ALL);
    let (status, body) = send_json(
        &t.app(),
        get_request("/health").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["name"], "jai");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
}

/// HMAC-SHA256 (RFC 2104), written out again here to check the server's.
fn hmac_sha256(key: &[u8], message: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut block = [0u8; 64];
    block[..key.len()].copy_from_slice(key);
    let inner = Sha256::new()
        .chain_update(block.map(|b| b ^ 0x36))
        .chain_update(message)
        .finalize();
    Sha256::new()
        .chain_update(block.map(|b| b ^ 0x5c))
        .chain_update(inner)
        .finalize()
        .to_vec()
}

/// `/health?nonce=…` proves that the server holds the token: base64url of
/// HMAC-SHA256(token, "jai-health:<port>:<nonce>"), as DESIGN.md says.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_proves_the_token_for_a_nonce() {
    use base64::Engine as _;
    let t = TestCore::new(ALL);
    let app = t.app();
    let nonce = "N0nce-0123456789_abcdef";
    let (status, body) = send_json(
        &app,
        get_request(&format!("/health?nonce={nonce}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], "jai");
    let mac = hmac_sha256(
        TOKEN.as_bytes(),
        format!("jai-health:{PORT}:{nonce}").as_bytes(),
    );
    let expected = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac);
    assert_eq!(body["proof"], expected.as_str());

    let (_, body) = send_json(&app, get_request("/health").body(Body::empty()).unwrap()).await;
    assert!(body.get("proof").is_none(), "{body}");

    for query in [
        "nonce=short",
        "nonce=abcdefghijklmnop&nonce=abcdefghijklmnop",
        "nonce=%41bcdefghijklmnop",
    ] {
        let (status, body) = send_json(
            &app,
            get_request(&format!("/health?{query}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_error(
            status,
            &body,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "invalid_nonce",
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v1_needs_the_token() {
    let t = TestCore::new(ALL);
    let app = t.app();

    let (status, headers, body) =
        send(&app, get_request("/v1/models").body(Body::empty()).unwrap()).await;
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_error(
        status,
        &body,
        StatusCode::UNAUTHORIZED,
        "auth_error",
        "unauthorized",
    );
    assert!(headers.contains_key("www-authenticate"));

    let wrong = get_request("/v1/models")
        .header("authorization", "Bearer jai_wrong-token-0123456789")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send_json(&app, wrong).await;
    assert_error(
        status,
        &body,
        StatusCode::UNAUTHORIZED,
        "auth_error",
        "unauthorized",
    );

    let basic = get_request("/v1/models")
        .header("authorization", format!("Basic {TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let (status, _) = send_json(&app, basic).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // The token is checked before the body is even looked at.
    let unauthed_post = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("host", HOST)
        .body(Body::from("not json"))
        .unwrap();
    let (status, _) = send_json(&app, unauthed_post).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = send_json(&app, authed_get("/v1/models")).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_changes_apply_at_once() {
    let t = TestCore::new(ALL);
    let app = t.app();
    *t.core.token.write().unwrap() = "jai_regenerated".to_owned();
    let (status, _) = send_json(&app, authed_get("/v1/models")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let req = get_request("/v1/models")
        .header("authorization", "Bearer jai_regenerated")
        .body(Body::empty())
        .unwrap();
    let (status, _) = send_json(&app, req).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_hosts_are_refused() {
    let t = TestCore::new(ALL);
    let app = t.app();
    for host in [
        "evil.example:5240",
        "127.0.0.1:5241",
        "127.0.0.1",
        "localhost",
        "0.0.0.0:5240",
    ] {
        for path in ["/health", "/v1/models"] {
            let req = axum::http::Request::builder()
                .uri(path)
                .header("host", host)
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap();
            let (status, body) = send_json(&app, req).await;
            assert_error(
                status,
                &body,
                StatusCode::FORBIDDEN,
                "auth_error",
                "bad_host",
            );
        }
    }
    for host in ["localhost:5240", "[::1]:5240", "127.0.0.1:5240"] {
        let req = axum::http::Request::builder()
            .uri("/v1/models")
            .header("host", host)
            .header("authorization", format!("Bearer {TOKEN}"))
            .body(Body::empty())
            .unwrap();
        let (status, _) = send_json(&app, req).await;
        assert_eq!(status, StatusCode::OK, "{host}");
    }
    // No Host at all, and two of them.
    let none = axum::http::Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let (status, _) = send_json(&app, none).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let two = axum::http::Request::builder()
        .uri("/health")
        .header("host", HOST)
        .header("host", "evil.example:5240")
        .body(Body::empty())
        .unwrap();
    let (status, _) = send_json(&app, two).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // An absolute request URI must name this server too.
    let absolute = axum::http::Request::builder()
        .uri("http://evil.example:5240/health")
        .header("host", HOST)
        .body(Body::empty())
        .unwrap();
    let (status, _) = send_json(&app, absolute).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn web_origins_are_refused() {
    let t = TestCore::new(ALL);
    let app = t.app();
    for origin in [
        "https://evil.example",
        "null",
        "http://127.0.0.1:5240",
        "http://localhost:5240",
    ] {
        let req = get_request("/v1/models")
            .header("authorization", format!("Bearer {TOKEN}"))
            .header("origin", origin)
            .body(Body::empty())
            .unwrap();
        let (status, body) = send_json(&app, req).await;
        assert_error(
            status,
            &body,
            StatusCode::FORBIDDEN,
            "auth_error",
            "bad_origin",
        );
    }
    // /health checks Origin too.
    let req = get_request("/health")
        .header("origin", "https://evil.example")
        .body(Body::empty())
        .unwrap();
    let (status, _) = send_json(&app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extension_origins_are_allowed() {
    let t = TestCore::new(ALL);
    let app = t.app();
    for origin in [
        "chrome-extension://abcdefghijklmnopabcdefghijklmnop",
        "moz-extension://2f1a6c1e-8a1b-4e5c-9d8f-0123456789ab",
        "safari-web-extension://0123ABCD",
    ] {
        let req = get_request("/v1/models")
            .header("authorization", format!("Bearer {TOKEN}"))
            .header("origin", origin)
            .body(Body::empty())
            .unwrap();
        let (status, headers, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::OK, "{origin}");
        // Never any CORS headers.
        assert!(!headers.contains_key("access-control-allow-origin"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_paths_and_methods() {
    let t = TestCore::new(ALL);
    let app = t.app();
    let (status, body) = send_json(&app, authed_get("/v1/nothing-here")).await;
    assert_error(
        status,
        &body,
        StatusCode::NOT_FOUND,
        "invalid_request_error",
        "not_found",
    );
    // Without the token, an unknown path does not reveal whether it exists.
    let (status, _) = send_json(
        &app,
        get_request("/v1/nothing-here").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, body) = send_json(&app, authed_get("/v1/chat/completions")).await;
    assert_error(
        status,
        &body,
        StatusCode::METHOD_NOT_ALLOWED,
        "invalid_request_error",
        "method_not_allowed",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn body_limits() {
    let t = TestCore::new(NOTHING);
    let app = t.app();
    // Over 1 MiB on the image endpoint.
    let big = json!({ "prompt": "x".repeat(1_100_000) })
        .to_string()
        .into_bytes();
    let (status, body) = send_json(&app, post_bytes("/v1/images/generations", big)).await;
    assert_error(
        status,
        &body,
        StatusCode::PAYLOAD_TOO_LARGE,
        "invalid_request_error",
        "body_too_large",
    );
    // 2 MiB is fine for chat (its limit is 64 MiB): it gets as far as the
    // install check.
    let chat = json!({ "messages": [{ "role": "user", "content": "x".repeat(2_000_000) }] });
    let (status, body) = send_json(&app, authed_post("/v1/chat/completions", &chat)).await;
    assert_error(
        status,
        &body,
        StatusCode::CONFLICT,
        "unavailable_error",
        "models_missing",
    );
    // Over 64 MiB for chat.
    let huge = vec![b' '; 64 * 1024 * 1024 + 1];
    let (status, body) = send_json(&app, post_bytes("/v1/chat/completions", huge)).await;
    assert_error(
        status,
        &body,
        StatusCode::PAYLOAD_TOO_LARGE,
        "invalid_request_error",
        "body_too_large",
    );
    // Not JSON at all.
    let (status, body) =
        send_json(&app, post_bytes("/v1/chat/completions", b"{oops".to_vec())).await;
    assert_error(
        status,
        &body,
        StatusCode::BAD_REQUEST,
        "invalid_request_error",
        "invalid_json",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn models_lists_both_with_install_state() {
    let t = TestCore::new(Install {
        llm: true,
        image: false,
    });
    let (status, body) = send_json(&t.app(), authed_get("/v1/models")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["object"], "list");
    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 2);
    assert_eq!(data[0]["id"], "qwen3.8-27b");
    assert_eq!(data[0]["object"], "model");
    assert_eq!(data[0]["owned_by"], "jai");
    assert_eq!(data[0]["kind"], "llm");
    assert_eq!(data[0]["installed"], true);
    assert_eq!(data[1]["id"], "flux-schnell");
    assert_eq!(data[1]["kind"], "image");
    assert_eq!(data[1]["installed"], false);
    assert_eq!(
        data[1]["aliases"],
        json!(["black-forest-labs/flux-schnell"])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_reports_engines_and_install() {
    let t = TestCore::new(ALL);
    let (status, body) = send_json(&t.app(), authed_get("/v1/status")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(body["installed"], true);
    assert_eq!(body["backend"], "cpu");
    assert!(body["engines"]["state"].is_string(), "{body}");
    assert!(body["engines"]["active"].is_number(), "{body}");
    assert!(body.get("gpu").is_some(), "{body}");
    if !body["gpu"].is_null() {
        assert!(body["gpu"]["name"].is_string(), "{body}");
        assert!(body["gpu"]["vramTotalMb"].is_number(), "{body}");
    }

    let t = TestCore::new(Install {
        llm: true,
        image: false,
    });
    let (_, body) = send_json(&t.app(), authed_get("/v1/status")).await;
    assert_eq!(body["installed"], false);
}
