//! The CLI subcommands (DESIGN.md section 10, "CLI (clap)").
//!
//! Client commands (generate, chat, render, encode) use a shared server (JAI
//! Studio's, the tray's or `jai serve`) found with
//! `jai_server::discovery::find`. Without one they take the GPU lock and start
//! a temporary server in their own process, which stops with the command.
//! That server is private: it listens on a random loopback port with a token
//! of its own, so its `server.json` entry fails the `/health` proof for every
//! other process and nobody else comes to depend on it. When another process
//! holds the GPU lock without a shared server (another command's temporary
//! server, or a server still starting), a command waits for the lock; Ctrl+C
//! stops waiting.

use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use jai_core::Paths;
use jai_engines::SchedulerStatus;
use jai_js::{DocInput, ServerRef};
use jai_server::gpu_lock::{self, GpuLock};
use jai_server::{discovery, Core, ServerHandle};
use rand::TryRngCore as _;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::output::{clip, engines_summary, format_duration};
use crate::settings;
use crate::term::Live;

pub mod chat;
pub mod encode;
pub mod generate;
pub mod models;
pub mod render;
pub mod serve;
pub mod status;
pub mod token;
pub mod validate;

/// How often a client command looks again for a shared server or the GPU
/// lock while another process holds the lock.
const LOCK_POLL: Duration = Duration::from_millis(500);

/// Largest document file the CLI reads (a `.jai` file is a few kilobytes).
const MAX_DOC_BYTES: u64 = 64 << 20;

/// A usage error: the command line is wrong. Exits with 2.
#[derive(Debug)]
pub struct UsageError(pub String);

impl fmt::Display for UsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UsageError {}

/// A [`UsageError`] as an `anyhow::Error`.
pub fn usage(message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(UsageError(message.into()))
}

/// 2 for usage errors, 1 for everything else.
pub fn exit_code(e: &anyhow::Error) -> u8 {
    if e.downcast_ref::<UsageError>().is_some() {
        2
    } else {
        1
    }
}

/// The data root, with its folders.
pub fn paths() -> anyhow::Result<Paths> {
    let paths = Paths::resolve().context("cannot use the JAI data folder")?;
    paths
        .ensure_dirs()
        .with_context(|| format!("cannot create the folders in {}", paths.root.display()))?;
    Ok(paths)
}

/// An HTTP client for the local server: no proxy (a system proxy must never
/// see local traffic) and a product-only User-Agent.
pub fn local_http() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .user_agent(concat!("jai/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .context("cannot create the HTTP client")
}

/// The server a command talks to.
#[derive(Clone)]
pub struct Client {
    pub server: ServerRef,
    pub http: reqwest::Client,
}

impl Client {
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.server.url.trim_end_matches('/'))
    }

    pub fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.http
            .get(self.url(path))
            .bearer_auth(&self.server.token)
    }

    pub fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.http
            .post(self.url(path))
            .bearer_auth(&self.server.token)
    }

    /// `GET /v1/status`.
    pub async fn status(&self) -> anyhow::Result<Value> {
        let resp = self
            .get("/v1/status")
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .context("cannot reach the JAI server")?;
        Ok(check(resp).await?.json().await?)
    }
}

/// The server for one client command: a shared one, or a temporary private
/// one in this process that owns the GPU lock until [`Conn::close`].
pub struct Conn {
    client: Client,
    local: Option<LocalServer>,
}

struct LocalServer {
    core: Arc<Core>,
    handle: ServerHandle,
    _lock: GpuLock,
}

/// What a command shows while another process holds the GPU lock.
const WAITING: &str = "Waiting for the GPU: another JAI process is using it";
/// How to avoid that wait, shown once.
const WAITING_HINT: &str = "(another `jai` command, or a server that is starting). JAI Studio, \
                            its tray or `jai serve` share the models between commands. Ctrl+C stops waiting.";

impl Conn {
    /// Finds a shared server, else takes the GPU lock and starts a private
    /// one in this process. While another process holds the lock without a
    /// shared server, waits and looks again every half second.
    pub async fn open(paths: &Paths, live: &Live) -> anyhow::Result<Conn> {
        let http = local_http()?;
        let started = Instant::now();
        let mut waited = false;
        loop {
            if let Some((info, token)) = discovery::find(paths).await {
                live.clear();
                return Ok(Conn::remote(&info, token, http));
            }
            if let Some(lock) = gpu_lock::try_acquire(paths)? {
                live.clear();
                return Conn::private(paths, live, lock, http).await;
            }
            if !waited {
                live.err(&format!("{WAITING} {WAITING_HINT}"));
                waited = true;
            }
            live.status(&format!(
                "{WAITING}… {}",
                format_duration(started.elapsed().as_secs())
            ));
            tokio::time::sleep(LOCK_POLL).await;
        }
    }

    /// Starts this command's private server: loopback only, on a free port,
    /// with a token of its own, and without a `server.json` entry. The token
    /// is what keeps it private: another process holds the token file's
    /// token, so even while the entry exists the server's `/health` proof
    /// does not match and `discovery::find` passes it over.
    async fn private(
        paths: &Paths,
        live: &Live,
        lock: GpuLock,
        http: reqwest::Client,
    ) -> anyhow::Result<Conn> {
        let cfg = private_config(paths)?;
        live.err("No JAI Studio server is running: starting the models for this command.");
        let core = Core::new(paths.clone(), cfg).context("cannot set up the model server")?;
        Conn::start_private(paths, core, lock, http).await
    }

    /// Serves `core` privately (see [`Conn::private`]) under `lock`.
    async fn start_private(
        paths: &Paths,
        core: Arc<Core>,
        lock: GpuLock,
        http: reqwest::Client,
    ) -> anyhow::Result<Conn> {
        let token = private_token()?;
        *core.token.write().unwrap_or_else(|e| e.into_inner()) = token.clone();
        let handle = jai_server::start(core.clone())
            .await
            .context("cannot start the model server")?;
        // Nobody else can use it, so nobody needs to find it: without the
        // entry, other processes do not even try its `/health` proof. (This
        // process holds the GPU lock, so no shared server wrote the file.)
        discovery::remove(paths);
        Ok(Conn {
            client: Client {
                server: ServerRef {
                    url: settings::addr_url(handle.addr),
                    token,
                },
                http,
            },
            local: Some(LocalServer {
                core,
                handle,
                _lock: lock,
            }),
        })
    }

    fn remote(info: &discovery::ServerInfo, token: String, http: reqwest::Client) -> Conn {
        Conn {
            client: Client {
                server: ServerRef {
                    url: settings::client_url(&info.host, info.port),
                    token,
                },
                http,
            },
            local: None,
        }
    }

    pub fn client(&self) -> Client {
        self.client.clone()
    }

    /// Stops a temporary server: the engines, then the HTTP server (which
    /// removes its `server.json` entry), then the GPU lock.
    pub async fn close(self) {
        if let Some(local) = self.local {
            local.core.scheduler.shutdown().await;
            local.handle.stop().await;
        }
    }
}

/// The settings of a command's private server: `config.json`'s, but on
/// loopback only and on a free port.
fn private_config(paths: &Paths) -> anyhow::Result<jai_core::Config> {
    let mut cfg = settings::load_config(paths);
    cfg.server.host = "127.0.0.1".into();
    cfg.server.allow_lan = false;
    settings::validate(&cfg).map_err(|e| anyhow!("the settings in config.json: {e}"))?;
    cfg.server.port = 0;
    Ok(cfg)
}

/// A token for a private server: `jai_` and 32 random bytes in base64url,
/// like the token file's (DESIGN.md section 2), kept only in memory.
pub fn private_token() -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|e| anyhow!("the system has no random numbers to make a token: {e}"))?;
    Ok(format!("jai_{}", URL_SAFE_NO_PAD.encode(bytes)))
}

/// Runs a client command against a server (see [`Conn::open`]) and stops a
/// temporary server afterwards. On Ctrl+C a `graceful` command gets its token
/// cancelled and is awaited; any other command is dropped. A second Ctrl+C
/// exits at once.
pub async fn with_server<T, F, Fut>(
    paths: &Paths,
    live: &Arc<Live>,
    graceful: bool,
    f: F,
) -> anyhow::Result<T>
where
    F: FnOnce(Client, CancellationToken) -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    let conn = Conn::open(paths, live).await?;
    let cancel = CancellationToken::new();
    // The command's future ends with this block, so an interrupted request
    // has closed its connection before a temporary server stops.
    let result = {
        let fut = f(conn.client(), cancel.clone());
        tokio::pin!(fut);
        tokio::select! {
            r = &mut fut => r,
            () = interrupted() => {
                if graceful {
                    live.err("Canceling… press Ctrl+C again to stop at once.");
                    cancel.cancel();
                    fut.await
                } else {
                    live.clear();
                    Err(anyhow!("interrupted"))
                }
            }
        }
    };
    live.clear();
    conn.close().await;
    result
}

/// Resolves on the first Ctrl+C and arms the second, which exits at once with
/// 1 (the engines die with the process). Never resolves when no handler can be
/// installed.
pub async fn interrupted() {
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
    tokio::spawn(async {
        if tokio::signal::ctrl_c().await.is_ok() {
            errln!("\njai: stopped");
            std::process::exit(1);
        }
    });
}

/// Passes a success response through; turns an error response into its
/// message.
pub async fn check(resp: reqwest::Response) -> anyhow::Result<reqwest::Response> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    Err(anyhow!(error_message(status, &body)))
}

/// The message of an OpenAI-shaped error body, else the status and the start
/// of the body.
pub fn error_message(status: u16, body: &str) -> String {
    let parsed = serde_json::from_str::<Value>(body).ok();
    if let Some(m) = parsed
        .as_ref()
        .and_then(|v| v.get("error")?.get("message")?.as_str())
    {
        return m.to_string();
    }
    let body = clip(body.trim(), 300);
    if body.is_empty() {
        format!("the server answered HTTP {status}")
    } else {
        format!("the server answered HTTP {status}: {body}")
    }
}

/// How often [`engine_progress`] asks the server for the engine states;
/// `GET /v1/status` also probes the GPU, so not every second.
const STATUS_EVERY: u64 = 5;

/// Shows on the status line what the engines are doing while a request
/// waits: the elapsed time every second, and the engine states from `GET
/// /v1/status` every few seconds. Stops when dropped.
pub fn engine_progress(live: &Arc<Live>, client: &Client, what: &str) -> ProgressGuard {
    if !live.is_tty() {
        return ProgressGuard(None);
    }
    let guard_live = live.clone();
    let (live, client, what) = (live.clone(), client.clone(), what.to_string());
    let task = tokio::spawn(async move {
        let started = Instant::now();
        let mut detail = String::new();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut n = 0u64;
        loop {
            tick.tick().await;
            if n.is_multiple_of(STATUS_EVERY) {
                detail = client
                    .status()
                    .await
                    .ok()
                    .and_then(|v| {
                        serde_json::from_value::<SchedulerStatus>(v.get("engines")?.clone()).ok()
                    })
                    .map(|s| format!(" · {}", engines_summary(&s)))
                    .unwrap_or_default();
            }
            live.status(&format!(
                "{what}… {}{detail}",
                format_duration(started.elapsed().as_secs())
            ));
            n += 1;
        }
    });
    ProgressGuard(Some((task, guard_live)))
}

/// Stops [`engine_progress`] and clears its line.
pub struct ProgressGuard(Option<(tokio::task::JoinHandle<()>, Arc<Live>)>);

impl Drop for ProgressGuard {
    fn drop(&mut self) {
        if let Some((task, live)) = self.0.take() {
            task.abort();
            live.clear();
        }
    }
}

/// A document argument: a file when one exists at that path, else JAI text.
/// A missing file that looks like a file name is an error, not a prompt.
pub fn doc_input(arg: &str) -> anyhow::Result<(DocInput, Option<PathBuf>)> {
    let path = Path::new(arg);
    if path.is_file() {
        let bytes = read_limited(path, MAX_DOC_BYTES)?;
        return Ok((DocInput::Bytes(bytes), Some(path.to_path_buf())));
    }
    if looks_like_file(arg) {
        return Err(usage(format!("{arg}: no such file")));
    }
    Ok((DocInput::Text(arg.to_string()), None))
}

/// One line ending in a document file extension.
pub fn looks_like_file(arg: &str) -> bool {
    let a = arg.trim().to_ascii_lowercase();
    !a.contains('\n') && [".jai", ".json", ".txt"].iter().any(|ext| a.ends_with(ext))
}

/// Reads a file of at most `max` bytes.
pub fn read_limited(path: &Path, max: u64) -> anyhow::Result<Vec<u8>> {
    let meta =
        std::fs::metadata(path).with_context(|| format!("cannot read {}", path.display()))?;
    if meta.len() > max {
        anyhow::bail!(
            "{} is too large ({}; at most {})",
            path.display(),
            crate::output::format_bytes(meta.len()),
            crate::output::format_bytes(max)
        );
    }
    std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Core like `Core::new`'s, for the CPU builds, without detecting the
    /// GPU; its engines are never started here.
    fn test_core(paths: &Paths, cfg: jai_core::Config) -> Arc<Core> {
        use jai_core::{catalog, Backend};
        use jai_engines::{ProcessLauncher, Scheduler, SchedulerConfig};
        let selection = catalog::select(catalog::catalog(), catalog::current_os(), Backend::Cpu);
        let launcher = Arc::new(ProcessLauncher::new(
            paths.clone(),
            cfg.clone(),
            selection.clone(),
        ));
        let scheduler = Scheduler::new(launcher, SchedulerConfig::from_config(&cfg));
        Arc::new(Core {
            paths: paths.clone(),
            token: Arc::new(std::sync::RwLock::new(
                jai_core::token::load_or_create_token(paths).unwrap(),
            )),
            config: Arc::new(std::sync::RwLock::new(cfg)),
            selection,
            scheduler,
            http: local_http().unwrap(),
        })
    }

    #[tokio::test]
    async fn temporary_servers_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path().join("jai"));
        paths.ensure_dirs().unwrap();
        let cfg = private_config(&paths).unwrap();
        assert_eq!(
            (
                cfg.server.host.as_str(),
                cfg.server.port,
                cfg.server.allow_lan
            ),
            ("127.0.0.1", 0, false)
        );
        let lock = gpu_lock::try_acquire(&paths).unwrap().unwrap();
        let conn = Conn::start_private(&paths, test_core(&paths, cfg), lock, local_http().unwrap())
            .await
            .unwrap();
        let client = conn.client();
        let port: u16 = client
            .server
            .url
            .rsplit(':')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert_ne!(port, 5240);
        // It answers its own command...
        let resp = client.get("/v1/models").send().await.unwrap();
        assert_eq!(resp.status(), 200);
        // ...but nobody finds it: no entry, and an entry would fail the proof.
        assert!(!paths.server_file().exists());
        assert!(discovery::find(&paths).await.is_none());
        discovery::write(
            &paths,
            &discovery::ServerInfo {
                version: "0.1.0".into(),
                pid: std::process::id(),
                host: "127.0.0.1".into(),
                port,
                started: 0,
            },
        )
        .unwrap();
        assert!(discovery::find(&paths).await.is_none());
        // The token file's token does not get in.
        let file_token = jai_core::token::load_or_create_token(&paths).unwrap();
        assert_ne!(file_token, client.server.token);
        let resp = local_http()
            .unwrap()
            .get(client.url("/v1/models"))
            .bearer_auth(&file_token)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
        // The GPU lock is the command's until it closes the server.
        assert!(gpu_lock::try_acquire(&paths).unwrap().is_none());
        conn.close().await;
        assert!(gpu_lock::try_acquire(&paths).unwrap().is_some());
        assert!(!paths.server_file().exists());
    }

    #[test]
    fn private_tokens() {
        let a = private_token().unwrap();
        let b = private_token().unwrap();
        assert_ne!(a, b);
        assert!(a.starts_with("jai_"), "{a}");
        // 32 bytes are 43 base64url characters without padding.
        assert_eq!(a.len(), 4 + 43, "{a}");
        assert!(a[4..]
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'));
    }

    #[test]
    fn exit_codes() {
        assert_eq!(exit_code(&usage("bad flag")), 2);
        assert_eq!(exit_code(&anyhow!("network down")), 1);
        let wrapped = usage("bad").context("while parsing");
        assert_eq!(exit_code(&wrapped), 2);
    }

    #[test]
    fn error_messages() {
        let body = r#"{"error":{"message":"Download the models first: open JAI Studio or run `jai models download`.","type":"invalid_request_error","code":"models_missing"}}"#;
        assert_eq!(
            error_message(409, body),
            "Download the models first: open JAI Studio or run `jai models download`."
        );
        assert_eq!(error_message(502, ""), "the server answered HTTP 502");
        assert_eq!(
            error_message(500, "  boom \n"),
            "the server answered HTTP 500: boom"
        );
    }

    #[test]
    fn document_arguments() {
        assert!(looks_like_file("castle.jai"));
        assert!(looks_like_file("C:\\images\\Castle.JAI"));
        assert!(looks_like_file("doc.json"));
        assert!(!looks_like_file("prompt=a castle at dusk"));
        assert!(!looks_like_file("a castle\nstyle=photo.jai"));
        let e = doc_input("definitely-missing-file.jai").unwrap_err();
        assert_eq!(exit_code(&e), 2);
        match doc_input("prompt=a castle").unwrap() {
            (DocInput::Text(t), None) => assert_eq!(t, "prompt=a castle"),
            other => panic!("{other:?}"),
        }
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x.jai");
        std::fs::write(&file, b"a castle").unwrap();
        match doc_input(file.to_str().unwrap()).unwrap() {
            (DocInput::Bytes(b), Some(p)) => {
                assert_eq!(b, b"a castle");
                assert_eq!(p, file);
            }
            other => panic!("{other:?}"),
        }
        assert!(read_limited(&file, 3).is_err());
    }
}
