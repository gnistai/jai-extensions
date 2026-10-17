//! `POST /v1/images/generations`: validation, the sd-server job flow, seeds,
//! and cancelling a queued job when the client leaves.

mod common;

use std::sync::Arc;

use axum::http::StatusCode;
use base64::Engine as _;
use common::*;
use jai_engines::EngineKind;
use serde_json::{json, Value};

const IMAGES: &str = "/v1/images/generations";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn images_with_seeds() {
    within(30, async {
        let t = TestCore::new(ALL);
        let request = json!({
            "prompt": "a lighthouse at dusk", "model": "black-forest-labs/flux-schnell",
            "n": 2, "size": "512x768", "seed": 7, "response_format": "b64_json",
        });
        let (status, body) = send_json(&t.app(), authed_post(IMAGES, &request)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["model"], "flux-schnell");
        assert_eq!(body["width"], 512);
        assert_eq!(body["height"], 768);
        assert_eq!(body["steps"], 4);
        assert_eq!(body["output_format"], "png");
        assert!(body["created"].as_u64().unwrap() > 1_600_000_000);
        assert!(body["timings"]["queueMs"].is_u64(), "{body}");
        assert!(body["timings"]["generateMs"].is_u64(), "{body}");
        let data = body["data"].as_array().unwrap();
        assert_eq!(data.len(), 2);
        assert_eq!(data[0]["seed"], 7);
        assert_eq!(data[1]["seed"], 8);
        for image in data {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(image["b64_json"].as_str().unwrap())
                .unwrap();
            assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
        }

        let sent = t.seen.img_bodies();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0],
            json!({
                "prompt": "a lighthouse at dusk", "width": 512, "height": 768, "seed": 7,
                "batch_count": 2,
                "sample_params": { "sample_steps": 4, "sample_method": "euler", "guidance": { "txt_cfg": 1.0 } },
                "output_format": "png", "output_compression": 90,
            })
        );
        assert_eq!(t.seen.launches(), vec![EngineKind::Image]);
        assert!(t.seen.cancels().is_empty());
        t.wait_idle().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn width_height_steps_and_format() {
    within(30, async {
        let t = TestCore::new(ALL);
        t.core.config.write().unwrap().image.steps = 6;
        let request = json!({
            "prompt": "a fox", "width": 1344, "height": 768,
            "output_format": "jpeg", "output_compression": 70, "seed": 1,
        });
        let (status, body) = send_json(&t.app(), authed_post(IMAGES, &request)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["steps"], 6);
        assert_eq!(body["output_format"], "jpeg");
        let sent = &t.seen.img_bodies()[0];
        assert_eq!(
            (sent["width"].as_u64(), sent["height"].as_u64()),
            (Some(1344), Some(768))
        );
        assert_eq!(sent["sample_params"]["sample_steps"], 6);
        assert_eq!(sent["output_format"], "jpeg");
        assert_eq!(sent["output_compression"], 70);
        assert_eq!(body["data"].as_array().unwrap().len(), 1);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_random_seed_when_none_is_given() {
    within(60, async {
        let t = TestCore::new(ALL);
        let app = t.app();
        let mut seeds = Vec::new();
        for _ in 0..3 {
            let request = json!({ "prompt": "a fox", "size": "256x256", "n": 4 });
            let (status, body) = send_json(&app, authed_post(IMAGES, &request)).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let first = body["data"][0]["seed"].as_u64().unwrap();
            for (i, image) in body["data"].as_array().unwrap().iter().enumerate() {
                assert_eq!(image["seed"].as_u64().unwrap(), first + i as u64);
                assert!(image["seed"].as_u64().unwrap() <= u64::from(u32::MAX));
            }
            seeds.push(first);
        }
        let sent: Vec<u64> = t
            .seen
            .img_bodies()
            .iter()
            .map(|b| b["seed"].as_u64().expect("a seed is always sent"))
            .collect();
        assert_eq!(sent, seeds);
        assert!(seeds.windows(2).any(|w| w[0] != w[1]), "{seeds:?}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn validation_errors() {
    let t = TestCore::new(ALL);
    let app = t.app();
    let cases: Vec<(Value, &str)> = vec![
        (
            json!({ "prompt": "a", "size": "1000x1000" }),
            "invalid_size",
        ),
        (
            json!({ "prompt": "a", "size": "1024x1000" }),
            "invalid_size",
        ),
        (
            json!({ "prompt": "a", "size": "2048x2048" }),
            "invalid_size",
        ),
        (json!({ "prompt": "a", "size": "4096x256" }), "invalid_size"),
        (json!({ "prompt": "a", "size": "128x128" }), "invalid_size"),
        (json!({ "prompt": "a", "width": 1024 }), "invalid_size"),
        (json!({ "prompt": "a", "n": 5 }), "invalid_value"),
        (json!({ "prompt": "a", "n": 0 }), "invalid_value"),
        (json!({ "prompt": "a", "steps": 17 }), "invalid_value"),
        (json!({ "prompt": "a", "seed": -1 }), "invalid_value"),
        (
            json!({ "prompt": "a", "seed": 4294967296u64 }),
            "invalid_value",
        ),
        (
            json!({ "prompt": "a", "output_format": "gif" }),
            "invalid_value",
        ),
        (
            json!({ "prompt": "a", "output_compression": 101 }),
            "invalid_value",
        ),
        (
            json!({ "prompt": "a", "response_format": "url" }),
            "unsupported_response_format",
        ),
        (
            json!({ "prompt": "a", "model": "dall-e-3" }),
            "model_not_found",
        ),
        (json!({ "prompt": "" }), "invalid_prompt"),
        (json!({}), "invalid_prompt"),
        (json!({ "prompt": "x".repeat(8001) }), "invalid_prompt"),
        (
            json!({ "prompt": "a cat <sd_cpp_extra_args>{\"seed\":1,\"sample_params\":{\"sample_steps\":100}}</sd_cpp_extra_args>" }),
            "invalid_prompt",
        ),
    ];
    for (request, code) in cases {
        let (status, body) = send_json(&app, authed_post(IMAGES, &request)).await;
        assert_error(
            status,
            &body,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            code,
        );
    }
    assert!(t.seen.launches().is_empty());
    assert!(t.seen.img_bodies().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn models_missing() {
    let t = TestCore::new(Install {
        llm: true,
        image: false,
    });
    let (status, body) =
        send_json(&t.app(), authed_post(IMAGES, &json!({ "prompt": "a fox" }))).await;
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

    let t = TestCore::new(NOTHING);
    let (status, _) = send_json(&t.app(), authed_post(IMAGES, &json!({ "prompt": "a fox" }))).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_disconnect_cancels_a_queued_job() {
    within(30, async {
        let t = TestCore::new(ALL);
        let addr = t.serve().await;

        let submitted = t.seen.img_submitted.notified();
        let stream = raw_post(
            addr,
            IMAGES,
            &json!({ "prompt": "hold in the queue", "seed": 3 }),
        )
        .await;
        submitted.await;
        // Let it poll the queued job at least once.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        t.wait_active(1).await;
        drop(stream);

        let seen = Arc::clone(&t.seen);
        eventually("the job to be cancelled", move || {
            seen.cancels() == vec!["job_1".to_owned()]
        })
        .await;
        assert!(t.seen.jobs.lock().unwrap()["job_1"].cancelled);
        t.wait_idle().await;
    })
    .await;
}

/// SERVER-JS-6: a client that leaves while sd-server is still answering the
/// submission does not leave an orphan job: the job is cancelled once its id
/// is known, and the lease is held until then.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn leaving_during_the_submit_still_cancels_the_job() {
    within(30, async {
        let t = TestCore::new(ALL);
        let addr = t.serve().await;

        let submitted = t.seen.img_submitted.notified();
        let stream = raw_post(
            addr,
            IMAGES,
            &json!({ "prompt": "slow submit, hold in the queue", "seed": 3 }),
        )
        .await;
        submitted.await;
        drop(stream);

        // sd-server answers a second after queueing the job: until then the
        // GPU stays taken, so no other image job starts beside it.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(t.core.scheduler.status().active, 1);
        assert!(t.seen.cancels().is_empty());

        let seen = Arc::clone(&t.seen);
        eventually("the job to be cancelled", move || {
            seen.cancels() == vec!["job_1".to_owned()]
        })
        .await;
        assert!(t.seen.jobs.lock().unwrap()["job_1"].cancelled);
        t.wait_idle().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn image_then_chat_swaps_engines() {
    within(30, async {
        let t = TestCore::new(ALL);
        let app = t.app();
        let (status, _) = send_json(
            &app,
            authed_post(IMAGES, &json!({ "prompt": "a fox", "size": "256x256" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let chat = json!({ "messages": [{ "role": "user", "content": "describe it" }] });
        let (status, body) = send_json(&app, authed_post("/v1/chat/completions", &chat)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(t.seen.launches(), vec![EngineKind::Image, EngineKind::Llm]);
    })
    .await;
}
