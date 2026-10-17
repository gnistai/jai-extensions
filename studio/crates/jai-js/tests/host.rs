//! jai-js's public API against `dist/studio/host.mjs`: validate on a real
//! `.jai` file, and render and encode against an axum mock of the Studio API
//! (`/v1/images/generations` returns a small image; `/v1/chat/completions`
//! answers in the shape its response_format schema asks for).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use base64::Engine as _;
use jai_js::{
    imageops, DocInput, EncodeOptions, EncodeSink, EncodeTarget, RenderOptions, ServerRef,
};
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "jai_test-token";

/// `examples/encoding/earthrise.jai` from the spec repository: `$JAI_SPEC_DIR`,
/// else `../jai` next to jai-tools.
fn earthrise() -> Vec<u8> {
    let spec = std::env::var_os("JAI_SPEC_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../../jai"));
    let path = spec.join("examples").join("encoding").join("earthrise.jai");
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// A 64×64 image whose colour depends on `seed`, as PNG or JPEG.
fn test_image(seed: u64, format: &str) -> Vec<u8> {
    let shade = (seed * 53 % 200) as u8;
    let img = image::RgbImage::from_fn(64, 64, |x, y| {
        image::Rgb([shade, (x * 4) as u8, (y * 4) as u8])
    });
    let mut out = std::io::Cursor::new(Vec::new());
    let fmt = if format == "jpeg" {
        image::ImageFormat::Jpeg
    } else {
        image::ImageFormat::Png
    };
    img.write_to(&mut out, fmt).unwrap();
    out.into_inner()
}

/// What the mock saw and how it behaves.
#[derive(Clone, Default)]
struct Studio {
    image_delay: Duration,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
}

impl Studio {
    fn seen(&self, kind: &str) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _)| k == kind)
            .map(|(_, v)| v.clone())
            .collect()
    }
}

fn unauthorized(headers: &HeaderMap) -> Option<Response> {
    let ok = headers.get("authorization").and_then(|v| v.to_str().ok())
        == Some(&format!("Bearer {TOKEN}"));
    (!ok).then(|| {
        let body = json!({ "error": { "message": "Missing or invalid token", "type": "auth_error", "code": "unauthorized" } });
        (StatusCode::UNAUTHORIZED, Json(body)).into_response()
    })
}

async fn images(
    State(studio): State<Studio>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Some(denied) = unauthorized(&headers) {
        return denied;
    }
    studio
        .requests
        .lock()
        .unwrap()
        .push(("image".into(), body.clone()));
    tokio::time::sleep(studio.image_delay).await;
    let seed = body["seed"].as_u64().unwrap_or(777);
    let format = body["output_format"].as_str().unwrap_or("png").to_owned();
    let b64 = base64::engine::general_purpose::STANDARD.encode(test_image(seed, &format));
    Json(json!({
        "created": 0, "model": "flux-schnell", "width": body["width"], "height": body["height"], "steps": 4,
        "output_format": format, "data": [{ "b64_json": b64, "seed": seed }], "timings": { "queueMs": 0, "generateMs": 5 },
    }))
    .into_response()
}

/// Answers each analyst call in the shape of its response_format schema.
async fn chat(
    State(studio): State<Studio>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Some(denied) = unauthorized(&headers) {
        return denied;
    }
    let format = &body["response_format"]["json_schema"];
    let name = format["name"].as_str().unwrap_or_default().to_owned();
    studio
        .requests
        .lock()
        .unwrap()
        .push((name.clone(), body.clone()));
    // Each call's drafts differ, as a model's would: the engine measures a repeated draft only once.
    let call_no = studio.requests.lock().unwrap().len();
    let props = &format["schema"]["properties"];
    let scene = json!({ "horizon_y": 800, "horizon_shape": "curved", "camera": "telephoto", "lighting": "hard sunlight", "medium": "photograph" });
    let mut answer = match name.as_str() {
        "jai_drafts" => {
            let n = props["drafts"]["minItems"].as_u64().unwrap_or(1);
            let drafts: Vec<Value> = (0..n)
                .map(|i| json!({ "prompt": format!("A small blue planet above a grey lunar horizon, variant {call_no}-{i}"), "style": ["space photograph"], "avoid": ["text"] }))
                .collect();
            json!({ "notes": "mock notes", "drafts": drafts })
        }
        "jai_additions" => {
            let n = props["additions"]["minItems"].as_u64().unwrap_or(1);
            json!({ "notes": "", "additions": (0..n).map(|i| format!("addition {i}")).collect::<Vec<_>>() })
        }
        "jai_target_inspection" => {
            json!({ "subjects": [{ "name": "planet", "bbox_2d": [450, 400, 600, 550] }] })
        }
        "jai_render_inspection" => {
            let names: Vec<Value> = props["subjects"]["items"]["properties"]["name"]["enum"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            json!({ "subjects": names.into_iter().map(|n| json!({ "name": n, "bbox_2d": [440, 380, 610, 560] })).collect::<Vec<_>>() })
        }
        other => {
            return (
                StatusCode::BAD_REQUEST,
                format!("unexpected response_format {other}"),
            )
                .into_response()
        }
    };
    if name.contains("inspection") {
        if let (Some(a), Some(s)) = (answer.as_object_mut(), scene.as_object()) {
            a.extend(s.clone());
        }
    }
    Json(json!({
        "id": "chatcmpl-mock", "object": "chat.completion", "model": "qwen3.8-27b",
        "choices": [{ "index": 0, "message": { "role": "assistant", "content": answer.to_string() }, "finish_reason": "stop" }],
        "usage": { "prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30 },
    }))
    .into_response()
}

async fn mock_studio(studio: Studio) -> String {
    let app = Router::new()
        .route("/v1/images/generations", post(images))
        .route("/v1/chat/completions", post(chat))
        .with_state(studio);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("mock studio stopped: {e}");
        }
    });
    format!("http://{addr}")
}

fn server(url: &str) -> ServerRef {
    ServerRef {
        url: url.into(),
        token: TOKEN.into(),
    }
}

#[test]
fn validate_reads_a_real_jai_file() {
    let r = jai_js::validate(DocInput::Bytes(earthrise())).unwrap();
    assert!(r.ok && !r.fatal, "{r:?}");
    let text = r.text.unwrap();
    assert!(
        text.starts_with("Photograph of a distant Earthrise over the Moon"),
        "{text}"
    );
    assert!(
        text.contains(";model=black-forest-labs/flux-schnell;"),
        "{text}"
    );
    assert!(r.diagnostics.is_empty(), "{:?}", r.diagnostics);
}

#[test]
fn validate_reads_text_as_text_and_bytes_as_a_jai_file_only() {
    let text = jai_js::validate(DocInput::Text("a red fox;style=watercolor".into())).unwrap();
    assert_eq!(
        (text.ok, text.text.as_deref()),
        (true, Some("a red fox;style=watercolor"))
    );
    // JSON in text is prose like any other: there is no JSON text form.
    let json = jai_js::validate(DocInput::Text("{\"prompt\":[\"a red fox\"]}".into())).unwrap();
    assert_eq!(
        (json.ok, json.text.as_deref()),
        (true, Some("{\"prompt\":[\"a red fox\"]}"))
    );
    let broken = jai_js::validate(DocInput::Bytes(vec![
        0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255, 1, 2, 3,
    ]))
    .unwrap();
    assert!(
        !broken.ok && broken.fatal && broken.text.is_none(),
        "{broken:?}"
    );
    assert!(
        broken
            .diagnostics
            .iter()
            .any(|d| d.code == "malformed-container"),
        "{broken:?}"
    );
    // Bytes that are not one gzip member are not a .jai file: no fallback to
    // JSON or text.
    // An unreadable document is not validated: only the decoder's diagnostic,
    // no no-prompt for the empty document it decoded to.
    for raw in [&b"a red fox"[..], br#"{"jai":1,"prompt":"a red fox"}"#, b""] {
        let r = jai_js::validate(DocInput::Bytes(raw.to_vec())).unwrap();
        let codes: Vec<&str> = r.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(
            r.fatal && r.text.is_none() && codes == ["malformed-container"],
            "{r:?}"
        );
    }
}

#[test]
fn validate_reads_data_image_jai_urls_and_nothing_else_that_begins_with_data() {
    let codes = |r: &jai_js::ValidateResult| -> Vec<String> {
        r.diagnostics.iter().map(|d| d.code.clone()).collect()
    };
    // The body is the text form; `;base64` is a media-type parameter.
    let b64 = jai_js::validate(DocInput::Text(
        "data:image/jai;base64,QSBjYXN0bGU7c3R5bGU9aW5r".into(),
    ))
    .unwrap();
    assert_eq!(
        (b64.ok, b64.text.as_deref(), codes(&b64)),
        (
            true,
            Some("A castle;style=ink"),
            vec!["data-url-parameter".into()]
        )
    );
    let bad = jai_js::validate(DocInput::Text("data:image/jai,%FF".into())).unwrap();
    assert!(bad.fatal && bad.text.is_none(), "{bad:?}");
    assert_eq!(codes(&bad), ["undecodable"]);
    // Prose that begins with "Data:" and other data: URLs are JAI text.
    for prose in ["Data: a bar chart of sales", "data:text/plain,A castle"] {
        let r = jai_js::validate(DocInput::Text(prose.into())).unwrap();
        assert_eq!((r.ok, r.text.as_deref()), (true, Some(prose)), "{r:?}");
    }
    // A data:image/jai URL that carries no document is refused.
    let e = jai_js::validate(DocInput::Text("data:image/jai;base64".into())).unwrap_err();
    assert!(format!("{e:#}").contains("carries no document"), "{e:#}");
}

#[tokio::test]
async fn render_against_a_mock_server() {
    let studio = Studio::default();
    let url = mock_studio(studio.clone()).await;
    let opts = RenderOptions {
        server: server(&url),
        seed: Some("42".into()),
        output_format: None,
    };
    let out = jai_js::render(DocInput::Bytes(earthrise()), opts)
        .await
        .unwrap();

    let sent = studio.seen("image");
    assert_eq!(sent.len(), 1);
    let request = &sent[0];
    assert_eq!(out.bytes, test_image(42, "png"));
    assert_eq!(out.mime, "image/png");
    assert_eq!(out.model, "black-forest-labs/flux-schnell");
    assert_eq!(out.model_label, "FLUX.1 schnell");
    assert_eq!(out.seed, 42);
    assert_eq!(
        (out.width, out.height),
        (1024, 1024),
        "aspect=1:1 renders at 1024x1024"
    );
    assert_eq!(request["seed"], 42);
    assert_eq!(request["output_format"], "png");
    assert_eq!(out.sent_prompt, request["prompt"].as_str().unwrap());
    assert!(
        out.sent_prompt
            .starts_with("Photograph of a distant Earthrise"),
        "{}",
        out.sent_prompt
    );
    assert!(
        out.sent_prompt.contains("Apollo space photography"),
        "{}",
        out.sent_prompt
    );
    assert!(out
        .description
        .starts_with("Photograph of a distant Earthrise"));
    assert!(out.document_text.starts_with("Photograph"));
}

/// An ISO BMFF box.
fn bmff(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut b = u32::try_from(body.len() + 8)
        .unwrap()
        .to_be_bytes()
        .to_vec();
    b.extend_from_slice(kind);
    b.extend_from_slice(body);
    b
}

#[tokio::test]
async fn render_takes_the_ratio_of_the_preview_without_an_aspect() {
    let url = mock_studio(Studio::default()).await;
    // Headers only: the host reads a preview's size and never decodes it.
    let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    png.extend_from_slice(&1600u32.to_be_bytes());
    png.extend_from_slice(&900u32.to_be_bytes());
    let mut jpeg = vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10];
    jpeg.extend_from_slice(&[0; 14]);
    jpeg.extend_from_slice(&[0xff, 0xc0, 0x00, 0x11, 0x08, 0x04, 0xb0, 0x03, 0x84, 0x03]);
    let mut ispe = vec![0; 4];
    ispe.extend_from_slice(&900u32.to_be_bytes());
    ispe.extend_from_slice(&2100u32.to_be_bytes());
    let mut meta = vec![0; 4];
    meta.extend(bmff(b"iprp", &bmff(b"ipco", &bmff(b"ispe", &ispe))));
    let mut avif = bmff(b"ftyp", b"avif\0\0\0\0mif1");
    avif.extend(bmff(b"meta", &meta));
    let preview = |mime: &str, bytes: &[u8]| {
        let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
        format!("preview=data:{mime};base64,{b64}")
    };
    for (doc, size) in [
        (
            format!("A castle;{}", preview("image/png", &png)),
            (1344, 768),
        ),
        (
            format!("A castle;{}", preview("image/jpeg", &jpeg)),
            (896, 1152),
        ),
        (
            format!("A castle;{}", preview("image/avif", &avif)),
            (640, 1536),
        ),
        (
            format!("A castle;aspect=1:1;{}", preview("image/png", &png)),
            (1024, 1024),
        ),
    ] {
        let opts = RenderOptions {
            server: server(&url),
            seed: None,
            output_format: None,
        };
        let out = jai_js::render(DocInput::Text(doc.clone()), opts)
            .await
            .unwrap();
        assert_eq!((out.width, out.height), size, "{doc}");
    }
}

#[tokio::test]
async fn render_errors_name_the_cause() {
    let url = mock_studio(Studio::default()).await;
    let wrong = RenderOptions {
        server: ServerRef {
            url: url.clone(),
            token: "wrong".into(),
        },
        seed: None,
        output_format: None,
    };
    let e = jai_js::render(DocInput::Bytes(earthrise()), wrong)
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("HTTP 401"), "{e:#}");
    let opts = RenderOptions {
        server: server(&url),
        seed: None,
        output_format: None,
    };
    let e = jai_js::render(DocInput::Bytes(vec![0x1f, 0x8b, 1, 2]), opts.clone())
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("malformed-container"), "{e:#}");
    let e = jai_js::render(DocInput::Text("data:image/jai,%FF".into()), opts)
        .await
        .unwrap_err();
    assert!(
        format!("{e:#}").contains("The document cannot be read: undecodable"),
        "{e:#}"
    );
}

#[tokio::test]
async fn dropping_a_render_closes_its_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (closed_tx, closed_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut buf = [0u8; 4096];
        // Read the request and anything after it; the read ends when the client hangs up.
        while let Ok(n) = socket.read(&mut buf).await {
            if n == 0 {
                break;
            }
        }
        let _ = closed_tx.send(());
    });
    let opts = RenderOptions {
        server: server(&url),
        seed: None,
        output_format: None,
    };
    let render = jai_js::render(DocInput::Bytes(earthrise()), opts);
    assert!(
        tokio::time::timeout(Duration::from_millis(500), render)
            .await
            .is_err(),
        "the render should still be waiting"
    );
    tokio::time::timeout(Duration::from_secs(5), closed_rx)
        .await
        .expect("the connection stayed open after the render was dropped")
        .expect("the server task ended without a request");
}

/// The encode target: a dark square with a blue disc, as PNG.
fn target_png() -> Vec<u8> {
    let img = image::RgbImage::from_fn(320, 240, |x, y| {
        let (dx, dy) = (x as i32 - 180, y as i32 - 100);
        if dx * dx + dy * dy < 900 {
            image::Rgb([40, 90, 220])
        } else if y > 200 {
            image::Rgb([90, 90, 90])
        } else {
            image::Rgb([0, 0, 0])
        }
    });
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

fn encode_options(url: &str, generations: u32) -> EncodeOptions {
    let target = imageops::prepare_target(&target_png()).unwrap();
    EncodeOptions {
        server: server(url),
        target: EncodeTarget {
            url: target.data_url,
            width: target.width,
            height: target.height,
        },
        target_thumbnail: target.thumb,
        generations,
        population: 2,
        seeds: vec![1],
        restarts: 1,
        goal: 0.999,
        time_limit_ms: None,
        render_scale: Some(0.25),
        thinking: None,
        source_name: Some("disc.png".into()),
    }
}

fn collecting_sink(dir: &std::path::Path) -> (EncodeSink, Arc<Mutex<Vec<Value>>>) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let seen = events.clone();
    let sink = EncodeSink {
        on_event: Arc::new(move |e| seen.lock().unwrap().push(e)),
        render_dir: dir.to_path_buf(),
    };
    (sink, events)
}

#[tokio::test]
async fn encode_against_a_mock_server() {
    let studio = Studio::default();
    let url = mock_studio(studio.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let renders = dir.path().join("renders");
    let (sink, events) = collecting_sink(&renders);
    let out = jai_js::encode(encode_options(&url, 2), sink, CancellationToken::new())
        .await
        .unwrap();

    // The result document, as text and as a .jai file.
    assert!(out.text.starts_with("A small blue planet"), "{}", out.text);
    assert!(
        out.text.contains("model=black-forest-labs/flux-schnell"),
        "{}",
        out.text
    );
    assert_eq!(out.jai[..2], [0x1f, 0x8b]);
    let check = jai_js::validate(DocInput::Bytes(out.jai.clone())).unwrap();
    assert!(check.ok && check.diagnostics.is_empty(), "{check:?}");
    assert_eq!(check.text.as_deref(), Some(out.text.as_str()));
    assert!(
        ["generations", "goal", "plateau"].contains(&out.stop_reason.as_str()),
        "{}",
        out.stop_reason
    );
    assert!(out.best.fitness.is_finite() && out.seconds >= 0.0);
    assert_eq!(out.error, None, "a run that ends normally names no error");

    // Every call went to the mock with the right shapes.
    let images = studio.seen("image");
    assert_eq!(images.len(), 4, "2 generations x 2 candidates x 1 seed");
    assert!(images.iter().all(|r| r["output_format"] == "jpeg"
        && r["seed"] == 1
        && r["width"].as_u64().unwrap() % 64 == 0));
    assert_eq!(studio.seen("jai_target_inspection").len(), 1);
    assert_eq!(studio.seen("jai_render_inspection").len(), 4);
    assert_eq!(
        studio.seen("jai_drafts").len(),
        2,
        "describe, then one proposal"
    );
    for call in studio
        .seen("jai_drafts")
        .iter()
        .chain(studio.seen("jai_target_inspection").iter())
    {
        let url = call["messages"][1]["content"][1]["image_url"]["url"]
            .as_str()
            .unwrap_or_default();
        assert!(
            url.starts_with("data:image/jpeg;base64,"),
            "the task text, then one image per call, as a data URL"
        );
    }

    // Renders were saved and events point at them.
    let mut files: Vec<String> = std::fs::read_dir(&renders)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    files.sort();
    assert_eq!(files.len(), 4, "{files:?}");
    assert!(files.iter().all(|f| f.ends_with(".jpg")), "{files:?}");
    let events = events.lock().unwrap().clone();
    let candidates: Vec<&Value> = events.iter().filter(|e| e["type"] == "candidate").collect();
    assert_eq!(candidates.len(), 4, "{events:?}");
    for c in &candidates {
        for r in c["candidate"]["renders"].as_array().unwrap() {
            let file = r["file"].as_str().expect("a render file");
            assert!(files.iter().any(|f| f == file), "{file} not in {files:?}");
            assert!(
                r.get("result").is_none(),
                "render bytes must not reach events"
            );
        }
    }
    assert_eq!(out.report["candidates"].as_array().map(Vec::len), Some(4));
    assert_eq!(out.report["source"], "disc.png");
}

#[tokio::test]
async fn cancelling_an_encode_stops_it_promptly() {
    let studio = Studio {
        image_delay: Duration::from_secs(60),
        ..Studio::default()
    };
    let url = mock_studio(studio.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let (sink, _) = collecting_sink(dir.path());
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let watched = studio.clone();
    tokio::spawn(async move {
        // Cancel once the renders are waiting on the slow image endpoint.
        while watched.seen("image").is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        trigger.cancel();
    });
    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        jai_js::encode(encode_options(&url, 2), sink, cancel),
    )
    .await
    .expect("the encode did not stop");
    let elapsed = started.elapsed();
    match result {
        Ok(out) => assert_eq!(out.stop_reason, "canceled"),
        Err(e) => assert!(format!("{e:#}").contains("canceled"), "{e:#}"),
    }
    assert!(
        elapsed < Duration::from_secs(10),
        "took {elapsed:?}: __jaiAbort did not stop the encode"
    );
}

#[tokio::test]
async fn a_cancelled_token_stops_before_any_request() {
    let studio = Studio::default();
    let url = mock_studio(studio.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let (sink, _) = collecting_sink(dir.path());
    let cancel = CancellationToken::new();
    cancel.cancel();
    let e = jai_js::encode(encode_options(&url, 1), sink, cancel)
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("canceled"), "{e:#}");
    assert!(studio.requests.lock().unwrap().is_empty());
}

#[test]
fn the_async_api_can_run_on_any_executor_thread() {
    fn send<T: Send + 'static>(_: T) {}
    let opts = RenderOptions {
        server: server("http://127.0.0.1:1"),
        seed: None,
        output_format: None,
    };
    send(jai_js::render(DocInput::Text(String::new()), opts));
    let sink = EncodeSink {
        on_event: Arc::new(|_| {}),
        render_dir: PathBuf::new(),
    };
    send(jai_js::encode(
        encode_options("http://127.0.0.1:1", 1),
        sink,
        CancellationToken::new(),
    ));
}
