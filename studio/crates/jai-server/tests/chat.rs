//! `POST /v1/chat/completions`: validation, forwarding, streaming, errors.

mod common;

use std::sync::atomic::Ordering;
use std::sync::Arc;

use std::io::Cursor;

use axum::http::StatusCode;
use base64::Engine as _;
use common::*;
use futures::StreamExt;
use jai_engines::EngineKind;
use serde_json::{json, Value};
use tower::ServiceExt;

const CHAT: &str = "/v1/chat/completions";

fn hello() -> Value {
    json!({ "messages": [{ "role": "user", "content": "hi" }] })
}

/// A data URL of a small image in this format.
fn image_url(format: image::ImageFormat, mime: &str) -> String {
    let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(5, 3, |x, _| {
        image::Rgb([(x * 50) as u8, 100, 150])
    }));
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, format).unwrap();
    let b64 = base64::engine::general_purpose::STANDARD.encode(out.into_inner());
    format!("data:{mime};base64,{b64}")
}

/// CRC-32 as PNG chunks use it.
fn png_crc(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// The type and the image of a forwarded data URL.
fn open_data_url(url: &str) -> (String, image::DynamicImage) {
    let rest = url.strip_prefix("data:").expect("a data URL");
    let (mime, data) = rest.split_once(";base64,").expect("base64");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .unwrap();
    (mime.to_owned(), image::load_from_memory(&bytes).unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chat_is_forwarded() {
    within(30, async {
        let t = TestCore::new(ALL);
        let request = json!({
            "model": "qwen3.8-27b",
            "messages": [
                { "role": "system", "content": "be brief" },
                { "role": "user", "content": [
                    { "type": "text", "text": "what is this?" },
                    { "type": "image_url", "image_url": { "url": format!("data:image/png;base64,{PNG_B64}") } },
                ] },
            ],
            "max_tokens": 100000,
            "temperature": 0.5, "top_p": 0.9, "top_k": 20, "presence_penalty": 0.1, "seed": 7,
            "stop": ["END"], "response_format": { "type": "json_object" },
            "chat_template_kwargs": { "enable_thinking": false },
            "reasoning_effort": "none",
            "id_slot": 3, "n_probs": 4, "user": "someone",
        });
        let (status, body) = send_json(&t.app(), authed_post(CHAT, &request)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["model"], "qwen3.8-27b");
        assert_eq!(body["choices"][0]["message"]["content"], "hello");
        assert_eq!(body["usage"]["total_tokens"], 4);

        let sent = t.seen.chat_bodies();
        assert_eq!(sent.len(), 1);
        let sent = &sent[0];
        assert_eq!(sent["max_tokens"], 8192);
        // The image went on decoded and encoded again; the rest unchanged.
        let forwarded = sent["messages"][1]["content"][1]["image_url"]["url"]
            .as_str()
            .unwrap()
            .to_owned();
        let (mime, image) = open_data_url(&forwarded);
        assert_eq!(mime, "image/png");
        assert_eq!((image.width(), image.height()), (1, 1));
        let mut expected = request["messages"].clone();
        expected[1]["content"][1]["image_url"]["url"] = json!(forwarded);
        assert_eq!(sent["messages"], expected);
        for key in [
            "temperature", "top_p", "top_k", "presence_penalty", "seed", "stop",
            "response_format", "chat_template_kwargs", "reasoning_effort",
        ] {
            assert_eq!(sent[key], request[key], "{key}");
        }
        for key in ["id_slot", "n_probs", "user", "model", "stream"] {
            assert!(sent.get(key).is_none(), "{key} was forwarded");
        }
        assert_eq!(
            t.seen.chat_auth.lock().unwrap()[0].as_deref(),
            Some(format!("Bearer {ENGINE_KEY}").as_str())
        );
        assert_eq!(t.seen.launches(), vec![EngineKind::Llm]);
        t.wait_idle().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn default_token_limit_is_sent() {
    within(30, async {
        let t = TestCore::new(ALL);
        let (status, _) = send_json(&t.app(), authed_post(CHAT, &hello())).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(t.seen.chat_bodies()[0]["max_tokens"], 8192);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chat_streams_sse() {
    within(30, async {
        let t = TestCore::new(ALL);
        let mut request = hello();
        request["stream"] = json!(true);
        let res = t.app().oneshot(authed_post(CHAT, &request)).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()["content-type"], "text/event-stream");

        // The lease is held while the stream runs.
        t.wait_active(1).await;

        let mut body = res.into_body().into_data_stream();
        let mut text = String::new();
        while let Some(chunk) = body.next().await {
            text.push_str(std::str::from_utf8(&chunk.unwrap()).unwrap());
        }
        let data: Vec<&str> = text
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .collect();
        assert_eq!(data.last(), Some(&"[DONE]"), "{text}");
        let mut content = String::new();
        for line in &data[..data.len() - 1] {
            let chunk: Value = serde_json::from_str(line).unwrap();
            assert_eq!(chunk["model"], "qwen3.8-27b", "{line}");
            content.push_str(
                chunk["choices"][0]["delta"]["content"]
                    .as_str()
                    .unwrap_or(""),
            );
        }
        assert_eq!(data.len(), 4, "{text}");
        assert_eq!(content, "hello");
        assert_eq!(t.seen.chat_bodies()[0]["stream"], true);
        t.wait_idle().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_a_stream_releases_the_lease() {
    within(30, async {
        let t = TestCore::new(ALL);
        let mut request = hello();
        request["stream"] = json!(true);
        let res = t.app().oneshot(authed_post(CHAT, &request)).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let mut body = res.into_body().into_data_stream();
        let _first = body.next().await;
        t.wait_active(1).await;
        drop(body);
        t.wait_idle().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn validation_errors() {
    let t = TestCore::new(ALL);
    let app = t.app();
    let image = |url: &str| json!({ "type": "image_url", "image_url": { "url": url } });
    let png = format!("data:image/png;base64,{PNG_B64}");
    let cases = [
        (json!({}), "missing_messages"),
        (json!({ "messages": [] }), "invalid_messages"),
        (
            json!({ "messages": [{ "role": "user", "content": "hi" }], "model": "gpt-4o" }),
            "model_not_found",
        ),
        (
            json!({ "messages": [{ "role": "user", "content": "hi" }], "n": 2 }),
            "invalid_value",
        ),
        (
            json!({ "messages": [{ "role": "user", "content": "hi" }], "reasoning_effort": "max" }),
            "invalid_value",
        ),
        (
            json!({ "messages": [{ "role": "user", "content": "hi" }], "max_tokens": 0 }),
            "invalid_value",
        ),
        (
            json!({ "messages": [{ "role": "user", "content": [image("https://example.com/cat.png")] }] }),
            "invalid_image",
        ),
        (
            json!({ "messages": [{ "role": "user", "content": [image("C:\\cat.png")] }] }),
            "invalid_image",
        ),
        (
            json!({ "messages": [{ "role": "user", "content": [image("data:image/webp;base64,UklGRiQAAABXRUJQVlA4IBgAAAAwAQCdASoBAAEAAwA0JaQAA3AA/vuUAAA=")] }] }),
            "invalid_image",
        ),
        (
            json!({ "messages": [{ "role": "user", "content": vec![image(&png); 9] }] }),
            "too_many_images",
        ),
        (
            json!({ "messages": [{ "role": "user", "content": [{ "type": "video_url", "video_url": { "url": "https://example.com/v.mp4" } }] }] }),
            "unsupported_content",
        ),
    ];
    for (request, code) in cases {
        let (status, body) = send_json(&app, authed_post(CHAT, &request)).await;
        assert_error(
            status,
            &body,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            code,
        );
    }
    // Nothing reached the engine; it was not even started.
    assert!(t.seen.launches().is_empty());
}

/// SERVER-JS-2: llama-server only ever receives images that decoded here,
/// encoded again; anything else is a 400 before the engine is even started.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn images_are_reencoded_before_the_engine_sees_them() {
    within(30, async {
        let t = TestCore::new(ALL);
        let app = t.app();
        let part = |url: &str| json!({ "type": "image_url", "image_url": { "url": url } });
        let b64 = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);

        // A PNG signature in front of an MP4 box, a "BM" in front of junk,
        // and a PNG whose header claims 30000x30000.
        let mut fake_png = b"\x89PNG\r\n\x1a\n".to_vec();
        fake_png.extend_from_slice(b"\0\0\0\x18ftypmp42\0\0\0\0mp42isom\0\0\0\x08free");
        let mut fake_bmp = b"BM".to_vec();
        fake_bmp.extend_from_slice(&[0x41; 64]);
        let huge = {
            let mut png = base64::engine::general_purpose::STANDARD
                .decode(PNG_B64)
                .unwrap();
            png[16..24].copy_from_slice(&[0, 0, 0x75, 0x30, 0, 0, 0x75, 0x30]);
            let crc = png_crc(&png[12..29]);
            png[29..33].copy_from_slice(&crc.to_be_bytes());
            png
        };
        for (url, why) in [
            (
                format!("data:image/png;base64,{}", b64(&fake_png)),
                "fake png",
            ),
            (
                format!("data:image/bmp;base64,{}", b64(&fake_bmp)),
                "fake bmp",
            ),
            (
                format!("data:image/png;base64,{}", b64(&huge)),
                "huge header",
            ),
        ] {
            let request = json!({ "messages": [{ "role": "user", "content": [part(&url)] }] });
            let (status, body) = send_json(&app, authed_post(CHAT, &request)).await;
            assert_error(
                status,
                &body,
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid_image",
            );
            let message = body["error"]["message"].as_str().unwrap();
            assert!(
                message.starts_with("messages[0].content[0]: "),
                "{why}: {message}"
            );
            if why == "huge header" {
                assert!(message.contains("64 megapixels"), "{message}");
            }
        }
        assert!(t.seen.launches().is_empty(), "the engine was started");

        // GIF and BMP go on as PNG, a JPEG as a JPEG.
        let request = json!({ "messages": [{ "role": "user", "content": [
            { "type": "text", "text": "compare" },
            part(&image_url(image::ImageFormat::Gif, "image/gif")),
            part(&image_url(image::ImageFormat::Bmp, "image/bmp")),
            part(&image_url(image::ImageFormat::Jpeg, "image/jpeg")),
        ] }] });
        let (status, body) = send_json(&app, authed_post(CHAT, &request)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let sent = &t.seen.chat_bodies()[0];
        let content = &sent["messages"][0]["content"];
        assert_eq!(content[0], json!({ "type": "text", "text": "compare" }));
        for (k, want) in [(1, "image/png"), (2, "image/png"), (3, "image/jpeg")] {
            let (mime, image) = open_data_url(content[k]["image_url"]["url"].as_str().unwrap());
            assert_eq!(mime, want, "part {k}");
            assert_eq!((image.width(), image.height()), (5, 3), "part {k}");
        }
    })
    .await;
}

/// SERVER-JS-7: when the engine dies with half a line sent, the client still
/// gets the error as an event of its own, and every event parses.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_broken_stream_ends_with_a_parsable_error_event() {
    within(30, async {
        let t = TestCore::new(ALL);
        let request =
            json!({ "messages": [{ "role": "user", "content": "break" }], "stream": true });
        let res = t.app().oneshot(authed_post(CHAT, &request)).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        // Events as a client parses them: data lines joined, split at blank lines.
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
        assert!(data.is_empty(), "the stream ends inside an event: {text:?}");
        assert_eq!(events.len(), 2, "{text:?}");
        let first: Value = serde_json::from_str(&events[0]).unwrap();
        assert_eq!(first["model"], "qwen3.8-27b");
        let error: Value =
            serde_json::from_str(&events[1]).unwrap_or_else(|e| panic!("{e}: {:?}", events[1]));
        assert_eq!(error["error"]["code"], "upstream_error", "{error}");
        t.wait_idle().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn engine_errors_keep_their_meaning() {
    within(30, async {
        let t = TestCore::new(ALL);
        let request = json!({ "messages": [{ "role": "user", "content": "too long" }] });
        let (status, body) = send_json(&t.app(), authed_post(CHAT, &request)).await;
        assert_error(
            status,
            &body,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "exceed_context_size_error",
        );
        t.wait_idle().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn engine_key_rejection_is_not_a_client_auth_error() {
    within(30, async {
        let t = TestCore::with_engine_key(ALL, "some-other-key");
        let (status, body) = send_json(&t.app(), authed_post(CHAT, &hello())).await;
        assert_error(
            status,
            &body,
            StatusCode::BAD_GATEWAY,
            "server_error",
            "upstream_error",
        );
        t.wait_idle().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn models_missing() {
    let t = TestCore::new(Install {
        llm: false,
        image: true,
    });
    let (status, body) = send_json(&t.app(), authed_post(CHAT, &hello())).await;
    assert_error(
        status,
        &body,
        StatusCode::CONFLICT,
        "unavailable_error",
        "models_missing",
    );
    assert_eq!(
        body["error"]["message"],
        "Download the models first: open JAI Studio or run `jai models download`."
    );
    assert!(t.seen.launches().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_disconnect_aborts_upstream_and_frees_the_lease() {
    within(30, async {
        let t = TestCore::new(ALL);
        let addr = t.serve().await;

        let started = t.seen.chat_started.notified();
        let request = json!({ "messages": [{ "role": "user", "content": "hang" }] });
        let stream = raw_post(addr, CHAT, &request).await;
        started.await;
        t.wait_active(1).await;
        drop(stream);

        let seen = Arc::clone(&t.seen);
        eventually("llama-server to see the request dropped", move || {
            seen.chat_aborted.load(Ordering::SeqCst) == 1
        })
        .await;
        t.wait_idle().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn leaving_the_gpu_queue_frees_the_place() {
    within(30, async {
        let t = TestCore::new(ALL);
        let addr = t.serve().await;

        // A holds the only LLM lease.
        let started = t.seen.chat_started.notified();
        let hang = json!({ "messages": [{ "role": "user", "content": "hang" }] });
        let a = raw_post(addr, CHAT, &hang).await;
        started.await;

        // B waits for the GPU, then leaves.
        let b = raw_post(addr, CHAT, &hello()).await;
        let core = Arc::clone(&t.core);
        eventually("B to queue", move || {
            core.scheduler.status().waiting_llm == 1
        })
        .await;
        drop(b);
        let core = Arc::clone(&t.core);
        eventually("B to leave the queue", move || {
            core.scheduler.status().waiting_llm == 0
        })
        .await;

        // A leaves, and the lease comes back.
        drop(a);
        t.wait_idle().await;

        // C gets the engine; B never reached it.
        let (status, body) = send_json(&t.app(), authed_post(CHAT, &hello())).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let sent = t.seen.chat_bodies();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0]["messages"][0]["content"], "hang");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn engine_start_failure_is_unavailable() {
    within(30, async {
        let t = TestCore::new(ALL);
        t.seen.fail_launch.store(true, Ordering::SeqCst);
        let (status, body) = send_json(&t.app(), authed_post(CHAT, &hello())).await;
        assert_error(
            status,
            &body,
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable_error",
            "engine_start_failed",
        );
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains("out of memory"), "{message}");

        // The next request tries again.
        t.seen.fail_launch.store(false, Ordering::SeqCst);
        let (status, body) = send_json(&t.app(), authed_post(CHAT, &hello())).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requests_after_shutdown_are_unavailable() {
    within(30, async {
        let t = TestCore::new(ALL);
        t.core.scheduler.shutdown().await;
        let (status, body) = send_json(&t.app(), authed_post(CHAT, &hello())).await;
        assert_error(
            status,
            &body,
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable_error",
            "shutting_down",
        );
        let request = json!({ "prompt": "a fox", "size": "256x256" });
        let (status, body) =
            send_json(&t.app(), authed_post("/v1/images/generations", &request)).await;
        assert_error(
            status,
            &body,
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable_error",
            "shutting_down",
        );
        assert!(t.seen.launches().is_empty());
    })
    .await;
}
