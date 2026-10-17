//! Test harness: a fake `Launcher` whose engines are axum mocks of
//! llama-server and sd-server, a `Core` around it, and request helpers.

#![allow(dead_code)]

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use jai_core::{Backend, Component, Config, Paths, Role, Selection, Unpack};
use jai_engines::{EngineKind, LaunchAttempt, Launcher, RunningEngine, Scheduler, SchedulerConfig};
use jai_server::Core;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::{oneshot, Notify};
use tower::ServiceExt;

pub const TOKEN: &str = "jai_test-token-0123456789";
/// The key the mock llama-server accepts.
pub const ENGINE_KEY: &str = "engine-key-42";
pub const PORT: u16 = 5240;
pub const HOST: &str = "127.0.0.1:5240";
/// A 1×1 PNG.
pub const PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

/// What the mock engines saw. Shared by every engine a launcher starts.
#[derive(Default)]
pub struct Seen {
    pub launches: Mutex<Vec<EngineKind>>,
    /// Makes every launch fail, as an engine that cannot start.
    pub fail_launch: AtomicBool,
    pub chat_bodies: Mutex<Vec<Value>>,
    pub chat_auth: Mutex<Vec<Option<String>>>,
    /// A "hang" chat request arrived.
    pub chat_started: Notify,
    /// A "hang" chat request was dropped because its connection closed.
    pub chat_aborted: AtomicUsize,
    pub img_bodies: Mutex<Vec<Value>>,
    pub img_submitted: Notify,
    pub jobs: Mutex<HashMap<String, Job>>,
    pub cancels: Mutex<Vec<String>>,
    next_job: AtomicUsize,
}

impl Seen {
    pub fn launches(&self) -> Vec<EngineKind> {
        self.launches.lock().unwrap().clone()
    }

    pub fn chat_bodies(&self) -> Vec<Value> {
        self.chat_bodies.lock().unwrap().clone()
    }

    pub fn img_bodies(&self) -> Vec<Value> {
        self.img_bodies.lock().unwrap().clone()
    }

    pub fn cancels(&self) -> Vec<String> {
        self.cancels.lock().unwrap().clone()
    }
}

/// A mock sd-server job: queued on the first poll, generating on the second,
/// completed after that. A held job stays queued until it is cancelled.
#[derive(Clone, Debug)]
pub struct Job {
    pub polls: u32,
    pub n: u64,
    pub hold: bool,
    pub cancelled: bool,
}

impl Job {
    fn status(&self) -> &'static str {
        match (self.cancelled, self.hold, self.polls) {
            (true, _, _) => "cancelled",
            (false, true, _) | (false, false, 0..=1) => "queued",
            (false, false, 2) => "generating",
            _ => "completed",
        }
    }

    fn json(&self, id: &str) -> Value {
        let status = self.status();
        let result = (status == "completed").then(|| {
            let images: Vec<Value> = (0..self.n)
                .map(|i| json!({ "index": i, "b64_json": PNG_B64 }))
                .collect();
            json!({ "output_format": "png", "images": images })
        });
        let error = self
            .cancelled
            .then(|| json!({ "code": "cancelled", "message": "job cancelled by client" }));
        json!({
            "id": id, "kind": "img_gen", "status": status, "created": 1,
            "started": null, "completed": null, "queue_position": 0,
            "result": result, "error": error,
        })
    }
}

/// Starts mock engines on random loopback ports.
pub struct FakeLauncher {
    pub seen: Arc<Seen>,
    /// The API key the launcher reports for the LLM.
    pub key: String,
}

#[async_trait::async_trait]
impl Launcher for FakeLauncher {
    async fn launch(
        &self,
        kind: EngineKind,
        _attempt: LaunchAttempt,
    ) -> anyhow::Result<Box<dyn RunningEngine>> {
        self.seen.launches.lock().unwrap().push(kind);
        if self.seen.fail_launch.load(Ordering::SeqCst) {
            anyhow::bail!("CUDA error: out of memory");
        }
        let (app, key) = match kind {
            EngineKind::Llm => (llama_app(Arc::clone(&self.seen)), Some(self.key.clone())),
            EngineKind::Image => (sd_app(Arc::clone(&self.seen)), None),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let (tx, rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = rx.await;
                })
                .await;
        });
        Ok(Box::new(FakeEngine {
            base_url: format!("http://{addr}"),
            key,
            stop: Some(tx),
            task,
        }))
    }
}

struct FakeEngine {
    base_url: String,
    key: Option<String>,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

#[async_trait::async_trait]
impl RunningEngine for FakeEngine {
    fn base_url(&self) -> String {
        self.base_url.clone()
    }

    fn api_key(&self) -> Option<String> {
        self.key.clone()
    }

    fn exited(&mut self) -> bool {
        self.task.is_finished()
    }

    fn log_tail(&self, _n: usize) -> Vec<String> {
        Vec::new()
    }

    async fn stop(mut self: Box<Self>) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        self.task.abort();
    }
}

// ---- mock llama-server ----

fn llama_app(seen: Arc<Seen>) -> Router {
    Router::new()
        .route("/health", get(|| async { Json(json!({ "status": "ok" })) }))
        .route("/v1/chat/completions", post(llama_chat))
        .with_state(seen)
}

/// Counts a "hang" request whose handler was dropped.
struct AbortGuard(Arc<Seen>);

impl Drop for AbortGuard {
    fn drop(&mut self) {
        self.0.chat_aborted.fetch_add(1, Ordering::SeqCst);
    }
}

async fn llama_chat(
    State(seen): State<Arc<Seen>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    seen.chat_auth.lock().unwrap().push(auth.clone());
    seen.chat_bodies.lock().unwrap().push(body.clone());
    if auth.as_deref() != Some(&format!("Bearer {ENGINE_KEY}")) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": { "code": 401, "message": "Invalid API Key", "type": "authentication_error" } })),
        )
            .into_response();
    }
    if body["messages"][0]["content"] == "hang" {
        let _guard = AbortGuard(Arc::clone(&seen));
        seen.chat_started.notify_one();
        std::future::pending::<()>().await;
    }
    if body["messages"][0]["content"] == "too long" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": { "code": 400, "message": "the request exceeds the context size", "type": "exceed_context_size_error" } })),
        )
            .into_response();
    }
    if body["messages"][0]["content"] == "break" && body["stream"] == true {
        // One event, half of the next, then the engine dies mid-stream.
        let parts: [Result<Bytes, std::io::Error>; 2] = [
            Ok(Bytes::from_static(
                b"data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"mock.gguf\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\ndata: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hel",
            )),
            Err(std::io::Error::other("the engine crashed")),
        ];
        let stream = futures::stream::iter(parts).then(|part| async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            part
        });
        return Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(stream))
            .unwrap();
    }
    if body["stream"] == true {
        // The second event is split inside its JSON, as TCP may split it.
        let parts = [
            "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"mock.gguf\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
            "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"mo",
            "del\":\"mock.gguf\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hel\"}}]}\n\n",
            "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"mock.gguf\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
        ];
        let stream = futures::stream::iter(parts).then(|part| async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            Ok::<_, Infallible>(Bytes::from_static(part.as_bytes()))
        });
        return Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(stream))
            .unwrap();
    }
    Json(json!({
        "id": "c1", "object": "chat.completion", "created": 1, "model": "mock.gguf",
        "choices": [{ "index": 0, "message": { "role": "assistant", "content": "hello" }, "finish_reason": "stop" }],
        "usage": { "prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4 },
    }))
    .into_response()
}

// ---- mock sd-server ----

fn sd_app(seen: Arc<Seen>) -> Router {
    Router::new()
        .route(
            "/v1/models",
            get(|| async { Json(json!({ "data": [{ "id": "sd-cpp-local" }] })) }),
        )
        .route("/sdcpp/v1/img_gen", post(sd_submit))
        .route("/sdcpp/v1/jobs/{id}", get(sd_job))
        .route("/sdcpp/v1/jobs/{id}/cancel", post(sd_cancel))
        .with_state(seen)
}

/// A prompt with "slow submit" queues the job at once but answers a second
/// later, as an sd-server under load might.
async fn sd_submit(State(seen): State<Arc<Seen>>, Json(body): Json<Value>) -> Response {
    seen.img_bodies.lock().unwrap().push(body.clone());
    let id = format!("job_{}", seen.next_job.fetch_add(1, Ordering::SeqCst) + 1);
    let prompt = body["prompt"].as_str().unwrap_or("");
    let job = Job {
        polls: 0,
        n: body["batch_count"].as_u64().unwrap_or(1),
        hold: prompt.contains("hold"),
        cancelled: false,
    };
    let slow = prompt.contains("slow submit");
    seen.jobs.lock().unwrap().insert(id.clone(), job);
    seen.img_submitted.notify_one();
    if slow {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    (
        StatusCode::ACCEPTED,
        Json(json!({ "id": id, "kind": "img_gen", "status": "queued", "created": 1, "poll_url": format!("/sdcpp/v1/jobs/{id}") })),
    )
        .into_response()
}

async fn sd_job(State(seen): State<Arc<Seen>>, Path(id): Path<String>) -> Response {
    let mut jobs = seen.jobs.lock().unwrap();
    match jobs.get_mut(&id) {
        Some(job) => {
            job.polls += 1;
            Json(job.json(&id)).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "job not found" })),
        )
            .into_response(),
    }
}

async fn sd_cancel(State(seen): State<Arc<Seen>>, Path(id): Path<String>) -> Response {
    let mut jobs = seen.jobs.lock().unwrap();
    let Some(job) = jobs.get_mut(&id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "job not found" })),
        )
            .into_response();
    };
    seen.cancels.lock().unwrap().push(id.clone());
    match job.status() {
        "queued" => {
            job.cancelled = true;
            Json(job.json(&id)).into_response()
        }
        "generating" => (
            StatusCode::CONFLICT,
            Json(json!({ "error": "job is currently generating and cannot be interrupted yet" })),
        )
            .into_response(),
        _ => Json(job.json(&id)).into_response(),
    }
}

// ---- the Core ----

/// Which engines have their components on disk.
#[derive(Clone, Copy, Debug)]
pub struct Install {
    pub llm: bool,
    pub image: bool,
}

pub const ALL: Install = Install {
    llm: true,
    image: true,
};
pub const NOTHING: Install = Install {
    llm: false,
    image: false,
};

pub struct TestCore {
    pub core: Arc<Core>,
    pub seen: Arc<Seen>,
    _dir: tempfile::TempDir,
}

impl TestCore {
    pub fn new(install: Install) -> TestCore {
        TestCore::with_engine_key(install, ENGINE_KEY)
    }

    /// `key` is the LLM key the launcher reports; the mock only accepts
    /// `ENGINE_KEY`.
    pub fn with_engine_key(install: Install, key: &str) -> TestCore {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path().join("jai"));
        std::fs::create_dir_all(&paths.root).unwrap();
        let selection = selection(&paths, install);
        let seen = Arc::new(Seen::default());
        let launcher = Arc::new(FakeLauncher {
            seen: Arc::clone(&seen),
            key: key.to_owned(),
        });
        let scheduler = Scheduler::new(
            launcher,
            SchedulerConfig {
                max_llm: 1,
                max_image: 1,
                max_batch_wait: Duration::from_secs(120),
                idle_unload: None,
                start_timeout: Duration::from_secs(10),
                mtp: false,
            },
        );
        let mut config = Config::default();
        config.server.port = 0;
        let core = Arc::new(Core {
            paths,
            config: Arc::new(RwLock::new(config)),
            token: Arc::new(RwLock::new(TOKEN.to_owned())),
            selection,
            scheduler,
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
        });
        TestCore {
            core,
            seen,
            _dir: dir,
        }
    }

    pub fn app(&self) -> Router {
        jai_server::router(Arc::clone(&self.core), PORT)
    }

    /// Serves the router on a random loopback port, for tests that need a
    /// real connection to hang up.
    pub async fn serve(&self) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = jai_server::router(Arc::clone(&self.core), addr.port());
        tokio::spawn(async move { axum::serve(listener, app).await });
        addr
    }

    /// The scheduler reports no active lease.
    pub async fn wait_idle(&self) {
        self.wait_active(0).await;
    }

    /// The scheduler reports `n` active leases.
    pub async fn wait_active(&self, n: usize) {
        let core = Arc::clone(&self.core);
        eventually(&format!("{n} active leases"), move || {
            core.scheduler.status().active == n
        })
        .await;
    }
}

/// One runtime archive per engine. Runtimes have no executable, so the
/// `engines/<dir>/.ok-<id>` marker with the sha256 makes them Installed
/// (DESIGN.md section 5).
fn selection(paths: &Paths, install: Install) -> Selection {
    let component = |id: &str, role: Role| Component {
        id: id.to_owned(),
        role,
        label: id.to_owned(),
        file: format!("{id}.zip"),
        url: "https://example.invalid/runtime.zip".to_owned(),
        size: 1,
        sha256: "ab".repeat(32),
        license: "MIT".to_owned(),
        source: "test".to_owned(),
        os: None,
        backend: None,
        unpack: Some(Unpack::Zip),
        strip: None,
        dir: Some(format!("{id}-dir")),
        exe: None,
    };
    let components = vec![
        component("jai-test-llm-runtime", Role::LlmRuntime),
        component("jai-test-image-runtime", Role::ImageRuntime),
    ];
    for c in &components {
        let wanted = match c.role {
            Role::LlmRuntime => install.llm,
            _ => install.image,
        };
        if wanted {
            let dir = paths.engines().join(c.dir.as_deref().unwrap());
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!(".ok-{}", c.id)), &c.sha256).unwrap();
        }
    }
    Selection {
        components,
        llm_backend: Backend::Cpu,
        image_backend: Backend::Cpu,
    }
}

// ---- requests ----

/// A GET with this server's Host and no token.
pub fn get_request(path: &str) -> axum::http::request::Builder {
    Request::builder()
        .method("GET")
        .uri(path)
        .header("host", HOST)
}

pub fn authed_get(path: &str) -> Request<Body> {
    get_request(path)
        .header("authorization", format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap()
}

pub fn authed_post(path: &str, body: &Value) -> Request<Body> {
    post_bytes(path, body.to_string().into_bytes())
}

pub fn post_bytes(path: &str, body: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("host", HOST)
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap()
}

/// Status, headers and the whole body.
pub async fn send(app: &Router, req: Request<Body>) -> (StatusCode, HeaderMap, Bytes) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, headers, body)
}

/// Status and the body as JSON.
pub async fn send_json(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let (status, _, body) = send(app, req).await;
    let value = serde_json::from_slice(&body)
        .unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(&body)));
    (status, value)
}

/// Asserts an error response in OpenAI's shape.
pub fn assert_error(status: StatusCode, body: &Value, want: StatusCode, kind: &str, code: &str) {
    assert_eq!(status, want, "{body}");
    assert_eq!(body["error"]["type"], kind, "{body}");
    assert_eq!(body["error"]["code"], code, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty()),
        "{body}"
    );
}

/// Writes a POST over a raw connection, for tests that hang up early.
pub async fn raw_post(addr: SocketAddr, path: &str, body: &Value) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let body = body.to_string();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {TOKEN}\r\n\
         Content-Type: application/json\r\nContent-Length: {len}\r\n\r\n{body}",
        port = addr.port(),
        len = body.len(),
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    stream
}

/// Polls `check` until it holds, failing after 10 s.
pub async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Bounds a whole test, so a hang fails instead of blocking the run.
pub async fn within<F: std::future::Future>(secs: u64, f: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(secs), f)
        .await
        .expect("the test took too long")
}
