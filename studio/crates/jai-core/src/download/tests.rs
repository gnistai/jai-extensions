//! Installer tests against a local axum file server on 127.0.0.1: full
//! download, resume (206), restart (200), retries, hash mismatch, cancel,
//! zip and tar.gz unpacking with strip, zip-slip, reuse and the disk check.

use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use bytes::Bytes;
use sha2::{Digest, Sha256};

use super::*;
use crate::catalog::{Backend, Role, Unpack};

// ---------------------------------------------------------------------------
// The file server

#[derive(Clone, Debug)]
struct Hit {
    range: Option<String>,
    user_agent: Option<String>,
    status: u16,
}

/// How the server misbehaves.
struct Behaviour {
    ignore_range: bool,
    /// The next `n` responses break the connection after this many body bytes.
    cut: Option<(usize, usize)>,
    /// The next `n` requests get this status.
    fail: Option<(usize, StatusCode)>,
    chunk: usize,
    /// Pause before each chunk.
    delay: Option<Duration>,
}

impl Default for Behaviour {
    fn default() -> Self {
        Behaviour {
            ignore_range: false,
            cut: None,
            fail: None,
            chunk: 16 * 1024,
            delay: None,
        }
    }
}

#[derive(Default)]
struct Shared {
    files: Mutex<HashMap<String, Bytes>>,
    behaviour: Mutex<Behaviour>,
    hits: Mutex<Vec<Hit>>,
    active: AtomicUsize,
    max_active: AtomicUsize,
}

struct Server {
    base: String,
    shared: Arc<Shared>,
}

impl Server {
    async fn start() -> Server {
        let shared = Arc::new(Shared::default());
        let app = Router::new()
            .route("/{name}", get(serve))
            .route("/redirect/{name}", get(redirect))
            .with_state(shared.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Server { base, shared }
    }

    fn put(&self, name: &str, data: &[u8]) -> String {
        self.shared
            .files
            .lock()
            .unwrap()
            .insert(name.into(), Bytes::copy_from_slice(data));
        self.url(name)
    }

    fn url(&self, name: &str) -> String {
        format!("{}/{name}", self.base)
    }

    fn behave(&self, change: impl FnOnce(&mut Behaviour)) {
        change(&mut self.shared.behaviour.lock().unwrap());
    }

    fn hits(&self) -> Vec<Hit> {
        self.shared.hits.lock().unwrap().clone()
    }
}

/// Like Hugging Face and GitHub, which answer with a 302 to a CDN.
async fn redirect(UrlPath(name): UrlPath<String>) -> Response {
    (StatusCode::FOUND, [(header::LOCATION, format!("/{name}"))]).into_response()
}

/// Counts a response body as active until the body is dropped.
struct Active(Arc<Shared>);

impl Active {
    fn new(shared: Arc<Shared>) -> Active {
        let now = shared.active.fetch_add(1, Ordering::SeqCst) + 1;
        shared.max_active.fetch_max(now, Ordering::SeqCst);
        Active(shared)
    }
}

impl Drop for Active {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn serve(
    State(shared): State<Arc<Shared>>,
    UrlPath(name): UrlPath<String>,
    headers: HeaderMap,
) -> Response {
    let text = |h: header::HeaderName| {
        headers
            .get(h)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let range = text(header::RANGE);
    let user_agent = text(header::USER_AGENT);
    let data = shared.files.lock().unwrap().get(&name).cloned();
    let (ignore_range, cut, fail, chunk, delay) = {
        let mut b = shared.behaviour.lock().unwrap();
        let cut = match &mut b.cut {
            Some((n, bytes)) if *n > 0 => {
                *n -= 1;
                Some(*bytes)
            }
            _ => None,
        };
        let fail = match &mut b.fail {
            Some((n, status)) if *n > 0 => {
                *n -= 1;
                Some(*status)
            }
            _ => None,
        };
        (b.ignore_range, cut, fail, b.chunk, b.delay)
    };
    let start = range
        .as_deref()
        .filter(|_| !ignore_range)
        .and_then(|r| r.strip_prefix("bytes="))
        .and_then(|r| r.strip_suffix('-'))
        .and_then(|n| n.parse::<usize>().ok());
    let status = match (&data, fail, start) {
        (_, Some(status), _) => status,
        (None, _, _) => StatusCode::NOT_FOUND,
        (Some(d), _, Some(s)) if s >= d.len() => StatusCode::RANGE_NOT_SATISFIABLE,
        (Some(_), _, Some(_)) => StatusCode::PARTIAL_CONTENT,
        (Some(_), _, None) => StatusCode::OK,
    };
    shared.hits.lock().unwrap().push(Hit {
        range,
        user_agent,
        status: status.as_u16(),
    });
    let Some(data) = data.filter(|_| status.is_success()) else {
        return status.into_response();
    };
    let from = start
        .filter(|_| status == StatusCode::PARTIAL_CONTENT)
        .unwrap_or(0);
    let body = data.slice(from..);
    let mut response = Response::builder()
        .status(status)
        .header(header::CONTENT_LENGTH, body.len())
        .header(header::ACCEPT_RANGES, "bytes");
    if status == StatusCode::PARTIAL_CONTENT {
        response = response.header(
            header::CONTENT_RANGE,
            format!("bytes {from}-{}/{}", data.len() - 1, data.len()),
        );
    }
    let active = Active::new(shared.clone());
    let stream = futures::stream::unfold((0usize, active), move |(pos, active)| {
        let body = body.clone();
        async move {
            if pos >= body.len() {
                return None;
            }
            if cut.is_some_and(|limit| pos >= limit) {
                return Some((Err(std::io::Error::other("cut")), (usize::MAX, active)));
            }
            if let Some(d) = delay {
                tokio::time::sleep(d).await;
            }
            let mut end = (pos + chunk).min(body.len());
            if let Some(limit) = cut.filter(|limit| *limit > pos) {
                end = end.min(limit);
            }
            Some((Ok(body.slice(pos..end)), (end, active)))
        }
    });
    response.body(Body::from_stream(stream)).unwrap()
}

// ---------------------------------------------------------------------------
// Fixtures

/// Deterministic noise, so hashes mean something.
fn data(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(0x9e37_79b9);
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x >> 24) as u8
        })
        .collect()
}

fn sha(bytes: &[u8]) -> String {
    fsutil::hex(&Sha256::digest(bytes))
}

fn model(id: &str, file: &str, bytes: &[u8], url: String) -> Component {
    Component {
        id: id.into(),
        role: Role::LlmModel,
        label: id.into(),
        file: file.into(),
        url,
        size: bytes.len() as u64,
        sha256: sha(bytes),
        license: "MIT".into(),
        source: "test".into(),
        os: None,
        backend: None,
        unpack: None,
        strip: None,
        dir: None,
        exe: None,
    }
}

fn archive(
    id: &str,
    bytes: &[u8],
    url: String,
    kind: Unpack,
    exe: &str,
    strip: Option<u32>,
) -> Component {
    Component {
        role: Role::LlmEngine,
        os: Some(crate::catalog::current_os()),
        backend: Some(Backend::Cpu),
        unpack: Some(kind),
        strip,
        dir: Some(id.into()),
        exe: Some(exe.into()),
        ..model(id, &format!("{id}.archive"), bytes, url)
    }
}

fn quick() -> Tuning {
    Tuning {
        backoff: Duration::from_millis(10),
        retries: 3,
        read_timeout: Duration::from_secs(10),
        connect_timeout: Duration::from_secs(5),
        reuse_dirs: Vec::new(),
        progress_interval: Duration::from_millis(100),
        disk_margin: 0,
    }
}

fn selection(components: &[Component]) -> Selection {
    Selection {
        components: components.to_vec(),
        llm_backend: Backend::Cpu,
        image_backend: Backend::Cpu,
    }
}

#[derive(Clone, Default)]
struct Events(Arc<Mutex<Vec<Progress>>>);

impl Events {
    fn sink(&self) -> Sink {
        let events = self.0.clone();
        Arc::new(move |p| events.lock().unwrap().push(p))
    }

    fn of(&self, id: &str) -> Vec<Progress> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.id == id)
            .cloned()
            .collect()
    }

    /// The states one component went through, without repeats.
    fn states(&self, id: &str) -> Vec<DlState> {
        let mut states: Vec<_> = self.of(id).iter().map(|p| p.state).collect();
        states.dedup();
        states
    }

    fn last(&self, id: &str) -> Progress {
        self.of(id).pop().unwrap()
    }
}

struct Env {
    dir: tempfile::TempDir,
    paths: Paths,
    server: Server,
    events: Events,
}

impl Env {
    async fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path().join("root"));
        Env {
            dir,
            paths,
            server: Server::start().await,
            events: Events::default(),
        }
    }

    async fn install(&self, components: &[Component], tuning: Tuning) -> anyhow::Result<()> {
        self.install_opts(components, DownloadOptions::default(), tuning)
            .await
    }

    async fn install_opts(
        &self,
        components: &[Component],
        opts: DownloadOptions,
        tuning: Tuning,
    ) -> anyhow::Result<()> {
        install_with(
            &self.paths,
            components,
            opts,
            tuning,
            self.events.sink(),
            CancellationToken::new(),
        )
        .await
    }

    fn state(&self, c: &Component) -> InstallState {
        install::status_in(&self.paths, &selection(std::slice::from_ref(c)), &[])
            .remove(0)
            .state
    }

    fn part(&self, c: &Component) -> PathBuf {
        install::part_path(&self.paths, c)
    }

    fn model_file(&self, c: &Component) -> PathBuf {
        self.paths.models().join(&c.file)
    }
}

fn make_zip(entries: &[(&str, Option<u32>, &[u8])]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, mode, bytes) in entries {
        let mut options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        if let Some(mode) = mode {
            options = options.unix_permissions(*mode);
        }
        if name.ends_with('/') {
            writer.add_directory(*name, options).unwrap();
        } else {
            writer.start_file(*name, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
    }
    writer.finish().unwrap().into_inner()
}

enum TarItem<'a> {
    Dir(&'a str),
    File(&'a str, u32, &'a [u8]),
    Link(&'a str, &'a str),
    /// A regular file whose name goes into the header unchecked.
    Raw(&'a str, &'a [u8]),
}

fn make_tar_gz(items: &[TarItem]) -> Vec<u8> {
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut builder = tar::Builder::new(encoder);
    for item in items {
        let mut header = tar::Header::new_gnu();
        match item {
            TarItem::Dir(name) => {
                header.set_entry_type(tar::EntryType::Directory);
                header.set_mode(0o755);
                header.set_size(0);
                builder
                    .append_data(&mut header, name, std::io::empty())
                    .unwrap();
            }
            TarItem::File(name, mode, bytes) => {
                header.set_entry_type(tar::EntryType::Regular);
                header.set_mode(*mode);
                header.set_size(bytes.len() as u64);
                builder.append_data(&mut header, name, *bytes).unwrap();
            }
            TarItem::Link(name, target) => {
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_mode(0o777);
                header.set_size(0);
                builder.append_link(&mut header, name, target).unwrap();
            }
            TarItem::Raw(name, bytes) => {
                let mut header = tar::Header::new_old();
                header.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
                header.set_entry_type(tar::EntryType::Regular);
                header.set_mode(0o644);
                header.set_size(bytes.len() as u64);
                header.set_cksum();
                builder.append(&header, *bytes).unwrap();
            }
        }
    }
    builder.into_inner().unwrap().finish().unwrap()
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

// ---------------------------------------------------------------------------
// Downloads

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn downloads_verifies_and_records_a_model() {
    let env = Env::new().await;
    let bytes = data(300_000, 1);
    let c = model("m", "m.gguf", &bytes, env.server.put("m.gguf", &bytes));
    assert_eq!(env.state(&c), InstallState::Missing);

    env.install(std::slice::from_ref(&c), quick())
        .await
        .unwrap();

    assert_eq!(fs::read(env.model_file(&c)).unwrap(), bytes);
    assert_eq!(env.state(&c), InstallState::Installed);
    assert!(install::ready(
        &env.paths,
        &selection(std::slice::from_ref(&c))
    ));
    assert!(!env.part(&c).exists());
    assert!(!env.paths.downloads().join("m.gguf.lock").exists());
    let hits = env.server.hits();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].range, None);
    assert_eq!(hits[0].user_agent.as_deref(), Some(http::USER_AGENT));
    assert_eq!(
        env.events.states("m"),
        [
            DlState::Queued,
            DlState::Downloading,
            DlState::Verifying,
            DlState::Done
        ]
    );
    let done = env.events.last("m");
    assert_eq!(
        (done.bytes, done.total, done.error),
        (300_000, 300_000, None)
    );
    let record = fs::read_to_string(env.paths.models().join(".verified.json")).unwrap();
    assert!(record.contains(&c.sha256), "{record}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn installed_components_are_skipped() {
    let env = Env::new().await;
    let bytes = data(10_000, 2);
    let c = model("m", "m.gguf", &bytes, env.server.put("m.gguf", &bytes));
    env.install(std::slice::from_ref(&c), quick())
        .await
        .unwrap();
    fs::write(env.part(&c), b"leftover").unwrap();

    env.install(std::slice::from_ref(&c), quick())
        .await
        .unwrap();

    assert_eq!(env.server.hits().len(), 1, "no second request");
    assert!(!env.part(&c).exists(), "a leftover part is cleaned up");
    let last = env.events.last("m");
    assert_eq!((last.state, last.bytes), (DlState::Done, 10_000));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_component_listed_twice_is_installed_once() {
    let env = Env::new().await;
    let bytes = data(10_000, 23);
    let c = model("m", "m.gguf", &bytes, env.server.put("m.gguf", &bytes));
    env.install(&[c.clone(), c.clone()], quick()).await.unwrap();
    assert_eq!(env.server.hits().len(), 1);
    assert_eq!(env.state(&c), InstallState::Installed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumes_a_part_file_with_a_range_request() {
    let env = Env::new().await;
    let bytes = data(400_000, 3);
    let c = model("m", "m.gguf", &bytes, env.server.put("m.gguf", &bytes));
    env.paths.ensure_dirs().unwrap();
    fs::write(env.part(&c), &bytes[..150_000]).unwrap();
    assert_eq!(env.state(&c), InstallState::Partial { bytes: 150_000 });

    env.install(std::slice::from_ref(&c), quick())
        .await
        .unwrap();

    let hits = env.server.hits();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].range.as_deref(), Some("bytes=150000-"));
    assert_eq!(hits[0].status, 206);
    assert_eq!(fs::read(env.model_file(&c)).unwrap(), bytes);
    assert_eq!(
        env.events.states("m"),
        [
            DlState::Queued,
            DlState::Verifying,
            DlState::Downloading,
            DlState::Verifying,
            DlState::Done
        ]
    );
    let first = env
        .events
        .of("m")
        .into_iter()
        .find(|p| p.state == DlState::Downloading)
        .unwrap();
    assert_eq!(
        first.bytes, 150_000,
        "downloading starts at the resume point"
    );
    assert_eq!(
        env.events.of("m")[0].bytes,
        150_000,
        "queued shows the part"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumes_through_a_redirect() {
    let env = Env::new().await;
    let bytes = data(120_000, 24);
    env.server.put("m.gguf", &bytes);
    let c = model("m", "m.gguf", &bytes, env.server.url("redirect/m.gguf"));
    env.paths.ensure_dirs().unwrap();
    fs::write(env.part(&c), &bytes[..50_000]).unwrap();

    env.install(std::slice::from_ref(&c), quick())
        .await
        .unwrap();

    let hits = env.server.hits();
    assert_eq!(hits.len(), 1);
    assert_eq!(
        hits[0].range.as_deref(),
        Some("bytes=50000-"),
        "Range survives the redirect"
    );
    assert_eq!(hits[0].status, 206);
    assert_eq!(fs::read(env.model_file(&c)).unwrap(), bytes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_ignoring_range_restarts_the_file() {
    let env = Env::new().await;
    env.server.behave(|b| b.ignore_range = true);
    let bytes = data(200_000, 4);
    let c = model("m", "m.gguf", &bytes, env.server.put("m.gguf", &bytes));
    env.paths.ensure_dirs().unwrap();
    // A part that is not even a prefix of the file: the restart must discard it.
    fs::write(env.part(&c), data(80_000, 99)).unwrap();

    env.install(std::slice::from_ref(&c), quick())
        .await
        .unwrap();

    let hits = env.server.hits();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].range.as_deref(), Some("bytes=80000-"));
    assert_eq!(hits[0].status, 200);
    assert_eq!(fs::read(env.model_file(&c)).unwrap(), bytes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hash_mismatch_deletes_the_part_and_fails() {
    let env = Env::new().await;
    let bytes = data(50_000, 5);
    let mut c = model("m", "m.gguf", &bytes, env.server.put("m.gguf", &bytes));
    c.sha256 = sha(b"another file");

    let err = env
        .install(std::slice::from_ref(&c), quick())
        .await
        .unwrap_err();

    assert!(err.to_string().contains("SHA-256 mismatch"), "{err}");
    assert!(!env.part(&c).exists());
    assert!(!env.model_file(&c).exists());
    let last = env.events.last("m");
    assert_eq!(last.state, DlState::Failed);
    assert!(last.error.unwrap().contains("SHA-256 mismatch"));
    assert_eq!(env.state(&c), InstallState::Missing);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_keeps_the_part_and_the_next_install_resumes() {
    let env = Env::new().await;
    let bytes = data(1 << 20, 6);
    let c = model("m", "m.gguf", &bytes, env.server.put("m.gguf", &bytes));
    env.server
        .behave(|b| b.delay = Some(Duration::from_millis(10)));
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let record = env.events.sink();
    let sink: Sink = Arc::new(move |p: Progress| {
        let enough = p.state == DlState::Downloading && p.bytes >= 64 * 1024;
        record(p);
        if enough {
            trigger.cancel();
        }
    });

    let err = install_with(
        &env.paths,
        std::slice::from_ref(&c),
        DownloadOptions::default(),
        quick(),
        sink,
        cancel,
    )
    .await
    .unwrap_err();

    assert!(err.to_string().contains("canceled"), "{err}");
    assert_eq!(env.events.last("m").state, DlState::Canceled);
    let kept = fs::metadata(env.part(&c)).unwrap().len();
    assert!(kept >= 64 * 1024 && kept < c.size, "kept {kept} bytes");
    assert!(!env.model_file(&c).exists());
    assert_eq!(env.state(&c), InstallState::Partial { bytes: kept });

    env.server.behave(|b| b.delay = None);
    env.install(std::slice::from_ref(&c), quick())
        .await
        .unwrap();

    let last = env.server.hits().pop().unwrap();
    assert_eq!(last.range, Some(format!("bytes={kept}-")));
    assert_eq!(last.status, 206);
    assert_eq!(fs::read(env.model_file(&c)).unwrap(), bytes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_broken_connection_is_retried_from_where_it_stopped() {
    let env = Env::new().await;
    // The pause lets each chunk reach the client before the cut; without it
    // hyper drops its unsent buffer along with the connection.
    env.server.behave(|b| {
        b.cut = Some((1, 100_000));
        b.delay = Some(Duration::from_millis(2));
    });
    let bytes = data(300_000, 7);
    let c = model("m", "m.gguf", &bytes, env.server.put("m.gguf", &bytes));

    env.install(std::slice::from_ref(&c), quick())
        .await
        .unwrap();

    let hits = env.server.hits();
    assert_eq!(hits.len(), 2, "{hits:?}");
    assert_eq!(hits[1].status, 206, "{hits:?}");
    let resumed_at: u64 = hits[1].range.as_deref().unwrap()["bytes=".len()..]
        .trim_end_matches('-')
        .parse()
        .unwrap();
    assert!(
        resumed_at > 0 && resumed_at <= 100_000,
        "resumed at {resumed_at}"
    );
    assert_eq!(fs::read(env.model_file(&c)).unwrap(), bytes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_errors_are_retried_three_times() {
    let env = Env::new().await;
    let bytes = data(20_000, 8);
    let c = model("m", "m.gguf", &bytes, env.server.put("m.gguf", &bytes));

    env.server
        .behave(|b| b.fail = Some((3, StatusCode::SERVICE_UNAVAILABLE)));
    env.install(std::slice::from_ref(&c), quick())
        .await
        .unwrap();
    assert_eq!(env.server.hits().len(), 4, "three failures, then success");

    let other = model("n", "n.gguf", &bytes, env.server.put("n.gguf", &bytes));
    env.server
        .behave(|b| b.fail = Some((10, StatusCode::BAD_GATEWAY)));
    let err = env
        .install(std::slice::from_ref(&other), quick())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("HTTP 502"), "{err}");
    assert_eq!(
        env.server.hits().len(),
        4 + 4,
        "one attempt plus three retries"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_file_fails_without_retries() {
    let env = Env::new().await;
    let c = model("m", "m.gguf", b"whatever", env.server.url("nothing-here"));
    let err = env
        .install(std::slice::from_ref(&c), quick())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("m: HTTP 404"), "{err}");
    assert_eq!(env.server.hits().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_size_on_the_server_fails() {
    let env = Env::new().await;
    let bytes = data(1000, 9);
    let mut c = model("m", "m.gguf", &bytes, env.server.put("m.gguf", &bytes));
    c.size = 999;
    let err = env
        .install(std::slice::from_ref(&c), quick())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("has 1000 bytes, not 999"), "{err}");
    assert_eq!(env.server.hits().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrency_is_limited() {
    let env = Env::new().await;
    env.server
        .behave(|b| b.delay = Some(Duration::from_millis(5)));
    let components: Vec<_> = (0..4)
        .map(|i| {
            let bytes = data(256 * 1024, 10 + i);
            let name = format!("m{i}.gguf");
            model(
                &format!("m{i}"),
                &name,
                &bytes,
                env.server.put(&name, &bytes),
            )
        })
        .collect();
    let opts = DownloadOptions {
        reuse: false,
        concurrency: 2,
    };
    env.install_opts(&components, opts, quick()).await.unwrap();
    assert_eq!(env.server.shared.max_active.load(Ordering::SeqCst), 2);
    for c in &components {
        assert_eq!(env.state(c), InstallState::Installed);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn progress_is_throttled_and_measures_speed() {
    let env = Env::new().await;
    env.server
        .behave(|b| b.delay = Some(Duration::from_millis(10)));
    let bytes = data(1 << 20, 20);
    let c = model("m", "m.gguf", &bytes, env.server.put("m.gguf", &bytes));
    let started = Instant::now();
    env.install(std::slice::from_ref(&c), quick())
        .await
        .unwrap();
    let elapsed_ms = started.elapsed().as_millis();
    let downloading: Vec<_> = env
        .events
        .of("m")
        .into_iter()
        .filter(|p| p.state == DlState::Downloading)
        .collect();
    assert!(
        downloading.len() as u128 <= elapsed_ms / 100 + 2,
        "{} events in {elapsed_ms} ms",
        downloading.len()
    );
    assert!(downloading.len() >= 3, "{} events", downloading.len());
    assert!(downloading.iter().any(|p| p.speed_bps > 0));
    assert!(downloading.windows(2).all(|w| w[0].bytes <= w[1].bytes));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn too_little_disk_space_fails_before_downloading() {
    let env = Env::new().await;
    let bytes = data(1000, 21);
    let c = model("m", "m.gguf", &bytes, env.server.put("m.gguf", &bytes));
    let mut tuning = quick();
    tuning.disk_margin = u64::MAX / 2;
    let err = env
        .install(std::slice::from_ref(&c), tuning)
        .await
        .unwrap_err();
    assert!(
        err.to_string().starts_with("Not enough disk space"),
        "{err}"
    );
    assert!(env.server.hits().is_empty());
    let last = env.events.last("m");
    assert_eq!(last.state, DlState::Failed);
    assert!(last.error.unwrap().contains("Not enough disk space"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_locked_part_file_is_busy() {
    let env = Env::new().await;
    let bytes = data(1000, 22);
    let c = model("m", "m.gguf", &bytes, env.server.put("m.gguf", &bytes));
    env.paths.ensure_dirs().unwrap();
    let held = files::PartLock::acquire(&env.paths, &c).unwrap();
    let err = env
        .install(std::slice::from_ref(&c), quick())
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("another download of this file"),
        "{err}"
    );
    assert!(env.server.hits().is_empty());
    drop(held);
}

// ---------------------------------------------------------------------------
// Files that are already on disk

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_size_unverified_model_is_verified_without_downloading() {
    let env = Env::new().await;
    let bytes = data(70_000, 30);
    let c = model("m", "m.gguf", &bytes, env.server.url("m.gguf"));
    env.paths.ensure_dirs().unwrap();
    fs::write(env.model_file(&c), &bytes).unwrap();
    assert_eq!(env.state(&c), InstallState::Partial { bytes: 70_000 });
    assert_eq!(
        bytes_needed(&env.paths, &selection(std::slice::from_ref(&c))),
        0
    );

    env.install(std::slice::from_ref(&c), quick())
        .await
        .unwrap();

    assert!(env.server.hits().is_empty());
    assert_eq!(env.state(&c), InstallState::Installed);
    assert_eq!(
        env.events.states("m"),
        [DlState::Queued, DlState::Verifying, DlState::Done]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_size_wrong_model_is_downloaded_again() {
    let env = Env::new().await;
    let bytes = data(70_000, 31);
    let c = model("m", "m.gguf", &bytes, env.server.put("m.gguf", &bytes));
    env.paths.ensure_dirs().unwrap();
    fs::write(env.model_file(&c), data(70_000, 32)).unwrap();

    env.install(std::slice::from_ref(&c), quick())
        .await
        .unwrap();

    assert_eq!(env.server.hits().len(), 1);
    assert_eq!(fs::read(env.model_file(&c)).unwrap(), bytes);
    assert_eq!(env.state(&c), InstallState::Installed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_complete_part_is_verified_without_a_request() {
    let env = Env::new().await;
    let bytes = data(70_000, 33);
    let c = model("m", "m.gguf", &bytes, env.server.url("m.gguf"));
    env.paths.ensure_dirs().unwrap();
    fs::write(env.part(&c), &bytes).unwrap();
    assert_eq!(env.state(&c), InstallState::Partial { bytes: 70_000 });

    env.install(std::slice::from_ref(&c), quick())
        .await
        .unwrap();

    assert!(env.server.hits().is_empty());
    assert_eq!(fs::read(env.model_file(&c)).unwrap(), bytes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reuses_an_lm_studio_file_through_a_hard_link() {
    let env = Env::new().await;
    let bytes = data(200_000, 40);
    let lm = env.dir.path().join("lmstudio").join("models");
    let repo = lm.join("unsloth").join("Qwen3.8-27B-GGUF");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("m.gguf"), &bytes).unwrap();
    // Not on the server: any download attempt would fail.
    let c = model("m", "m.gguf", &bytes, env.server.url("m.gguf"));
    let mut tuning = quick();
    tuning.reuse_dirs = vec![lm.clone()];
    let state = install::status_in(
        &env.paths,
        &selection(std::slice::from_ref(&c)),
        &tuning.reuse_dirs,
    )
    .remove(0)
    .state;
    assert!(matches!(state, InstallState::Reusable { .. }), "{state:?}");

    env.install(std::slice::from_ref(&c), tuning).await.unwrap();

    assert!(env.server.hits().is_empty());
    assert_eq!(fs::read(env.model_file(&c)).unwrap(), bytes);
    assert_eq!(env.state(&c), InstallState::Installed);
    assert_eq!(
        env.events.states("m"),
        [DlState::Queued, DlState::Reusing, DlState::Done]
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let ours = fs::metadata(env.model_file(&c)).unwrap();
        let theirs = fs::metadata(repo.join("m.gguf")).unwrap();
        assert_eq!(ours.ino(), theirs.ino(), "a hard link, not a copy");
        assert_eq!(ours.nlink(), 2);
    }
}

/// A file that other accounts can change is copied, never linked: a link
/// would let them change the verified model afterwards.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reuse_candidate_others_can_change_is_copied() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let env = Env::new().await;
    let bytes = data(120_000, 44);
    let lm = env.dir.path().join("shared").join("models");
    fs::create_dir_all(&lm).unwrap();
    let theirs = lm.join("m.gguf");
    fs::write(&theirs, &bytes).unwrap();
    fs::set_permissions(&theirs, fs::Permissions::from_mode(0o666)).unwrap();
    let c = model("m", "m.gguf", &bytes, env.server.url("m.gguf"));
    let mut tuning = quick();
    tuning.reuse_dirs = vec![lm.clone()];

    env.install(std::slice::from_ref(&c), tuning).await.unwrap();

    assert!(env.server.hits().is_empty(), "reused, not downloaded");
    assert_eq!(fs::read(env.model_file(&c)).unwrap(), bytes);
    assert_eq!(env.state(&c), InstallState::Installed);
    let ours = fs::metadata(env.model_file(&c)).unwrap();
    assert_ne!(ours.ino(), fs::metadata(&theirs).unwrap().ino(), "a copy");
    assert_eq!(ours.nlink(), 1);
    assert_eq!(
        env.events.states("m"),
        [DlState::Queued, DlState::Reusing, DlState::Done]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reuse_candidate_that_does_not_match_is_downloaded_instead() {
    let env = Env::new().await;
    let bytes = data(60_000, 41);
    let impostor = data(60_000, 42);
    let lm = env.dir.path().join("lm");
    fs::create_dir_all(lm.join("pub")).unwrap();
    fs::write(lm.join("pub").join("m.gguf"), &impostor).unwrap();
    let c = model("m", "m.gguf", &bytes, env.server.put("m.gguf", &bytes));
    let mut tuning = quick();
    tuning.reuse_dirs = vec![lm.clone()];

    env.install(std::slice::from_ref(&c), tuning).await.unwrap();

    assert_eq!(env.server.hits().len(), 1);
    assert_eq!(fs::read(env.model_file(&c)).unwrap(), bytes);
    assert_eq!(
        fs::read(lm.join("pub").join("m.gguf")).unwrap(),
        impostor,
        "untouched"
    );
    let states = env.events.states("m");
    assert_eq!(
        states[..3],
        [DlState::Queued, DlState::Reusing, DlState::Downloading]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reuse_off_downloads_even_with_a_candidate() {
    let env = Env::new().await;
    let bytes = data(60_000, 43);
    let lm = env.dir.path().join("lm");
    fs::create_dir_all(&lm).unwrap();
    fs::write(lm.join("m.gguf"), &bytes).unwrap();
    let c = model("m", "m.gguf", &bytes, env.server.put("m.gguf", &bytes));
    let mut tuning = quick();
    tuning.reuse_dirs = vec![lm];
    let opts = DownloadOptions {
        reuse: false,
        concurrency: 3,
    };
    env.install_opts(std::slice::from_ref(&c), opts, tuning)
        .await
        .unwrap();
    assert_eq!(env.server.hits().len(), 1);
    assert!(!env.events.states("m").contains(&DlState::Reusing));
}

// ---------------------------------------------------------------------------
// Archives

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unpacks_a_zip_with_strip_and_unix_modes() {
    let env = Env::new().await;
    let lib = data(5000, 50);
    let zip = make_zip(&[
        ("llama-b1/", None, b""),
        ("llama-b1/bin/llama-server", None, b"#!/bin/sh\necho hi\n"),
        ("llama-b1/bin/helper.sh", Some(0o750), b"#!/bin/sh\n"),
        ("llama-b1/lib/libggml.so", Some(0o600), &lib),
        ("llama-b1/README.md", None, b"hello"),
    ]);
    let c = archive(
        "llama-zip",
        &zip,
        env.server.put("llama.zip", &zip),
        Unpack::Zip,
        "llama-server",
        Some(1),
    );

    env.install(std::slice::from_ref(&c), quick())
        .await
        .unwrap();

    let dir = env.paths.engines().join("llama-zip");
    let exe = dir.join("bin").join("llama-server");
    assert_eq!(fs::read(&exe).unwrap(), b"#!/bin/sh\necho hi\n");
    assert_eq!(fs::read(dir.join("lib").join("libggml.so")).unwrap(), lib);
    assert_eq!(
        fs::read_to_string(dir.join(".ok-llama-zip")).unwrap(),
        c.sha256
    );
    assert!(!dir.join("llama-b1").exists(), "the top folder is stripped");
    assert!(!env.part(&c).exists(), "the archive is deleted");
    let sel = selection(std::slice::from_ref(&c));
    assert_eq!(
        install::engine_exe(&env.paths, &sel, Role::LlmEngine),
        Some(exe.clone())
    );
    assert_eq!(env.state(&c), InstallState::Installed);
    assert!(env.events.states("llama-zip").contains(&DlState::Unpacking));
    #[cfg(unix)]
    {
        assert_eq!(
            mode(&dir.join("bin").join("helper.sh")),
            0o750,
            "mode from the zip"
        );
        assert_eq!(
            mode(&dir.join("lib").join("libggml.so")),
            0o600,
            "mode from the zip"
        );
        assert_ne!(mode(&exe) & 0o100, 0, "the engine is made executable");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unpacks_a_tar_gz_with_strip_modes_and_links() {
    let env = Env::new().await;
    let lib = data(5000, 51);
    let tgz = make_tar_gz(&[
        TarItem::Dir("llama-b1/"),
        TarItem::File("llama-b1/llama-server", 0o755, b"\x7fELF"),
        TarItem::File("llama-b1/libggml-base.so.0.9.4", 0o644, &lib),
        TarItem::Link("llama-b1/libggml-base.so.0", "libggml-base.so.0.9.4"),
        TarItem::File("llama-b1/sub/notes.txt", 0o600, b"notes"),
    ]);
    let c = archive(
        "llama-tar",
        &tgz,
        env.server.put("llama.tgz", &tgz),
        Unpack::TarGz,
        "llama-server",
        Some(1),
    );

    env.install(std::slice::from_ref(&c), quick())
        .await
        .unwrap();

    let dir = env.paths.engines().join("llama-tar");
    assert_eq!(fs::read(dir.join("llama-server")).unwrap(), b"\x7fELF");
    assert_eq!(
        fs::read(dir.join("libggml-base.so.0")).unwrap(),
        lib,
        "the link resolves"
    );
    assert_eq!(
        fs::read(dir.join("sub").join("notes.txt")).unwrap(),
        b"notes"
    );
    assert_eq!(
        fs::read_to_string(dir.join(".ok-llama-tar")).unwrap(),
        c.sha256
    );
    assert!(!env.part(&c).exists());
    assert_eq!(env.state(&c), InstallState::Installed);
    #[cfg(unix)]
    {
        assert_eq!(mode(&dir.join("llama-server")), 0o755);
        assert_eq!(mode(&dir.join("sub").join("notes.txt")), 0o600);
        let link = dir.join("libggml-base.so.0");
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read_link(&link).unwrap(),
            PathBuf::from("libggml-base.so.0.9.4")
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_engine_and_its_runtime_share_a_folder() {
    let env = Env::new().await;
    let engine_zip = make_zip(&[("llama-server", Some(0o755), b"exe")]);
    let runtime_zip = make_zip(&[("cudart64_13.dll", None, b"dll")]);
    let engine = archive(
        "eng",
        &engine_zip,
        env.server.put("eng.zip", &engine_zip),
        Unpack::Zip,
        "llama-server",
        None,
    );
    let runtime = Component {
        id: "eng-rt".into(),
        role: Role::LlmRuntime,
        exe: None,
        ..archive(
            "eng",
            &runtime_zip,
            env.server.put("rt.zip", &runtime_zip),
            Unpack::Zip,
            "x",
            None,
        )
    };
    let runtime = Component {
        file: "rt.archive".into(),
        ..runtime
    };

    env.install(&[engine.clone(), runtime.clone()], quick())
        .await
        .unwrap();

    let dir = env.paths.engines().join("eng");
    assert!(dir.join("llama-server").is_file() && dir.join("cudart64_13.dll").is_file());
    assert!(dir.join(".ok-eng").is_file() && dir.join(".ok-eng-rt").is_file());
    assert!(install::ready(&env.paths, &selection(&[engine, runtime])));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zip_slip_is_rejected() {
    let env = Env::new().await;
    let zip = make_zip(&[("ok.txt", None, b"ok"), ("../evil.txt", None, b"evil")]);
    let c = archive(
        "slip",
        &zip,
        env.server.put("slip.zip", &zip),
        Unpack::Zip,
        "ok.txt",
        None,
    );

    let err = env
        .install(std::slice::from_ref(&c), quick())
        .await
        .unwrap_err();

    assert!(
        err.to_string()
            .contains("unsafe path in archive: ../evil.txt"),
        "{err}"
    );
    assert!(!env.paths.engines().join("evil.txt").exists());
    assert!(!env.paths.root.join("evil.txt").exists());
    assert!(!env.paths.engines().join("slip").join(".ok-slip").exists());
    assert!(!env.part(&c).exists(), "an unusable archive is not kept");
    assert_eq!(env.state(&c), InstallState::Missing);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tar_slip_is_rejected() {
    let env = Env::new().await;
    let tgz = make_tar_gz(&[
        TarItem::File("top/ok.txt", 0o644, b"ok"),
        TarItem::Raw("top/../../evil.txt", b"evil"),
    ]);
    let c = archive(
        "slip",
        &tgz,
        env.server.put("slip.tgz", &tgz),
        Unpack::TarGz,
        "ok.txt",
        Some(1),
    );

    let err = env
        .install(std::slice::from_ref(&c), quick())
        .await
        .unwrap_err();

    assert!(
        err.to_string()
            .contains("unsafe path in archive: top/../../evil.txt"),
        "{err}"
    );
    assert!(!env.paths.engines().join("evil.txt").exists());
    assert!(!env.paths.root.join("evil.txt").exists());
    assert!(!env.paths.engines().join("slip").join(".ok-slip").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_archive_without_its_engine_fails() {
    let env = Env::new().await;
    let zip = make_zip(&[("readme.txt", None, b"no engine here")]);
    let c = archive(
        "noexe",
        &zip,
        env.server.put("noexe.zip", &zip),
        Unpack::Zip,
        "llama-server",
        None,
    );
    let err = env
        .install(std::slice::from_ref(&c), quick())
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("llama-server is not in the archive"),
        "{err}"
    );
    assert!(!env.paths.engines().join("noexe").join(".ok-noexe").exists());
    assert!(!env.part(&c).exists(), "an unusable archive is not kept");
}

/// The app spawns `install` on a multi-threaded runtime.
#[test]
fn the_install_future_is_send() {
    fn assert_send<T: Send>(_: &T) {}
    let paths = Paths::at("unused");
    let future = install(
        &paths,
        &[],
        DownloadOptions::default(),
        |_| {},
        CancellationToken::new(),
    );
    assert_send(&future);
}
