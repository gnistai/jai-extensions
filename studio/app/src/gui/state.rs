//! State shared by the Tauri commands, the tray and the window: the `Core`
//! (replaced when engine settings change), the server with the GPU lock, the
//! download and the encode jobs, the quit flags, and the engine status of
//! another JAI process whose server this app uses.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use jai_core::{Config, Paths};
use jai_engines::SchedulerStatus;
use jai_js::ServerRef;
use jai_server::gpu_lock::{self, GpuLock};
use jai_server::{discovery, Core, ServerHandle};
use serde_json::Value;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::settings;

/// How long another process's engine status is reused: its `GET /v1/status`
/// also runs nvidia-smi, and the UI polls every second or two.
const REMOTE_STATUS_EVERY: Duration = Duration::from_secs(3);

/// Managed with `tauri::Manager::manage`.
pub struct AppState(Arc<Shared>);

impl AppState {
    pub fn new(shared: Arc<Shared>) -> AppState {
        AppState(shared)
    }

    /// A handle to move into tasks.
    pub fn shared(&self) -> Arc<Shared> {
        self.0.clone()
    }
}

impl Deref for AppState {
    type Target = Shared;

    fn deref(&self) -> &Shared {
        &self.0
    }
}

/// Locks a std mutex, whether or not a panic poisoned it.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct Shared {
    pub paths: Paths,
    core: watch::Sender<Arc<Core>>,
    server: tokio::sync::Mutex<ServerSlot>,
    /// Cancels the running download.
    pub download: Mutex<Option<CancellationToken>>,
    /// Cancels each running encode job, by job id.
    pub jobs: Mutex<HashMap<String, CancellationToken>>,
    pub next_job: AtomicU64,
    /// For another JAI process's server: no proxy.
    pub http: reqwest::Client,
    /// That process's engines, and when they were read.
    remote: tokio::sync::Mutex<Option<(Instant, Option<RemoteEngines>)>>,
    /// The tray icon exists (on Linux it needs libayatana-appindicator3).
    pub has_tray: AtomicBool,
    /// The "still running in the tray" notification was shown.
    pub told_about_tray: AtomicBool,
    pub quitting: AtomicBool,
    pub shutdown_done: AtomicBool,
    /// Serializes `settings_set`, `regenerate_token` and the start of encode
    /// jobs, so a restart never begins under a job that is just starting.
    pub settings_lock: tokio::sync::Mutex<()>,
}

/// The engines of another JAI process (such as `jai serve`) whose server
/// this app uses because it has none of its own.
#[derive(Clone, Debug)]
pub struct RemoteEngines {
    pub pid: u32,
    pub engines: SchedulerStatus,
    pub installed: bool,
}

/// The HTTP server, the GPU lock it needs, and why it is not running.
#[derive(Default)]
struct ServerSlot {
    handle: Option<ServerHandle>,
    lock: Option<GpuLock>,
    error: Option<String>,
}

impl ServerSlot {
    fn fail(&mut self, message: String) {
        if self.error.as_deref() != Some(message.as_str()) {
            tracing::warn!("the server is not running: {message}");
        }
        self.error = Some(message);
    }
}

impl Shared {
    pub fn new(paths: Paths, core: Arc<Core>, http: reqwest::Client) -> Arc<Shared> {
        Arc::new(Shared {
            paths,
            core: watch::Sender::new(core),
            server: tokio::sync::Mutex::default(),
            download: Mutex::default(),
            jobs: Mutex::default(),
            next_job: AtomicU64::new(0),
            http,
            remote: tokio::sync::Mutex::default(),
            has_tray: AtomicBool::new(false),
            told_about_tray: AtomicBool::new(false),
            quitting: AtomicBool::new(false),
            shutdown_done: AtomicBool::new(false),
            settings_lock: tokio::sync::Mutex::default(),
        })
    }

    /// The current core.
    pub fn core(&self) -> Arc<Core> {
        self.core.borrow().clone()
    }

    /// Changes when the core is replaced.
    pub fn subscribe_core(&self) -> watch::Receiver<Arc<Core>> {
        self.core.subscribe()
    }

    /// The current settings.
    pub fn config(&self) -> Config {
        let core = self.core();
        let cfg = core
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        cfg
    }

    /// The bearer token.
    pub fn token(&self) -> String {
        let core = self.core();
        let token = core.token.read().unwrap_or_else(|e| e.into_inner()).clone();
        token
    }

    /// Replaces the token everywhere it is used.
    pub fn set_token(&self, token: String) {
        let core = self.core();
        *core.token.write().unwrap_or_else(|e| e.into_inner()) = token;
    }

    /// Replaces the settings of the current core.
    pub fn set_config(&self, cfg: Config) {
        let core = self.core();
        *core.config.write().unwrap_or_else(|e| e.into_inner()) = cfg;
    }

    /// Where the server listens, when it runs.
    pub async fn server_addr(&self) -> Option<SocketAddr> {
        self.server.lock().await.handle.as_ref().map(|h| h.addr)
    }

    /// Why the server is not running.
    pub async fn server_error(&self) -> Option<String> {
        self.server.lock().await.error.clone()
    }

    /// Takes the GPU lock when needed and starts the server. Returns whether
    /// it runs; records why when it does not. Without a server the GPU lock is
    /// released, so `jai serve` or a CLI command can use the GPU.
    pub async fn ensure_server(&self) -> bool {
        let mut slot = self.server.lock().await;
        if slot.handle.is_some() {
            return true;
        }
        if self.quitting.load(Ordering::SeqCst) {
            return false;
        }
        if slot.lock.is_none() {
            match gpu_lock::try_acquire(&self.paths) {
                Ok(Some(lock)) => slot.lock = Some(lock),
                Ok(None) => {
                    slot.fail(
                        "the GPU is busy: another JAI process (such as a `jai` command) is using it. \
                         JAI Studio starts its own server when that process ends"
                            .into(),
                    );
                    return false;
                }
                Err(e) => {
                    slot.fail(format!("{e:#}"));
                    return false;
                }
            }
        }
        let core = self.core();
        let cfg = self.config();
        if let Err(e) = settings::validate(&cfg) {
            slot.lock = None;
            slot.fail(format!("the server settings: {e}"));
            return false;
        }
        match jai_server::start(core).await {
            Ok(handle) => {
                tracing::info!("server listening on {}", settings::addr_url(handle.addr));
                slot.handle = Some(handle);
                slot.error = None;
                true
            }
            Err(e) => {
                slot.lock = None;
                slot.fail(format!("cannot start the server: {e:#}"));
                false
            }
        }
    }

    /// Stops the server and starts it again with the current settings.
    pub async fn restart_server(&self) -> anyhow::Result<()> {
        {
            let mut slot = self.server.lock().await;
            if let Some(handle) = slot.handle.take() {
                handle.stop().await;
                discovery::remove(&self.paths);
            }
        }
        if self.ensure_server().await {
            Ok(())
        } else {
            Err(anyhow!(self
                .server_error()
                .await
                .unwrap_or_else(|| "the server did not start".into())))
        }
    }

    /// Builds a new core with these settings: stops the server, shuts the
    /// old engines down, and starts the server on the new core.
    pub async fn replace_core(&self, cfg: Config) -> anyhow::Result<()> {
        // Core::new may run nvidia-smi: keep it off the async threads.
        let paths = self.paths.clone();
        let new = tokio::task::spawn_blocking(move || Core::new(paths, cfg)).await??;
        let old = self.core.send_replace(new);
        let had_server = {
            let mut slot = self.server.lock().await;
            match slot.handle.take() {
                Some(handle) => {
                    handle.stop().await;
                    true
                }
                None => false,
            }
        };
        old.scheduler.shutdown().await;
        if had_server && !self.ensure_server().await {
            return Err(anyhow!(self
                .server_error()
                .await
                .unwrap_or_else(|| "the server did not start".into())));
        }
        Ok(())
    }

    /// Why restarting the server or the engines now would break work in
    /// progress, if it would (`settings::busy_reason`).
    pub fn busy(&self) -> Option<String> {
        let encodes = lock(&self.jobs).len();
        let s = self.core().scheduler.status();
        settings::busy_reason(encodes, s.active + s.waiting_llm + s.waiting_image)
    }

    /// The engines of another JAI process whose server this app uses, when
    /// it has no server of its own; read at most every few seconds.
    pub async fn remote_engines(&self) -> Option<RemoteEngines> {
        if self.server_addr().await.is_some() {
            return None;
        }
        let mut cache = self.remote.lock().await;
        if let Some((at, engines)) = cache.as_ref() {
            if at.elapsed() < REMOTE_STATUS_EVERY {
                return engines.clone();
            }
        }
        let engines = read_remote(&self.paths, &self.http).await;
        *cache = Some((Instant::now(), engines.clone()));
        engines
    }

    /// The server renders and encodes go to: this app's own, else one that
    /// another JAI process runs.
    pub async fn server_ref(&self) -> anyhow::Result<ServerRef> {
        if let Some(addr) = self.server_addr().await {
            return Ok(ServerRef {
                url: settings::addr_url(addr),
                token: self.token(),
            });
        }
        if let Some((info, token)) = discovery::find(&self.paths).await {
            return Ok(ServerRef {
                url: settings::client_url(&info.host, info.port),
                token,
            });
        }
        Err(anyhow!(
            self.server_error()
                .await
                .unwrap_or_else(|| "the model server is not running".into())
        ))
    }

    /// Quit: cancels the download and the encode jobs, shuts the engines
    /// down, stops the server, removes `server.json` and releases the GPU
    /// lock.
    pub async fn shutdown(&self) {
        if let Some(t) = lock(&self.download).take() {
            t.cancel();
        }
        for t in lock(&self.jobs).values() {
            t.cancel();
        }
        self.core().scheduler.shutdown().await;
        let mut slot = self.server.lock().await;
        if let Some(handle) = slot.handle.take() {
            handle.stop().await;
            discovery::remove(&self.paths);
        }
        slot.lock = None;
        tracing::info!("shut down");
    }
}

/// The engine status of the shared server another process runs, from its
/// `GET /v1/status`.
async fn read_remote(paths: &Paths, http: &reqwest::Client) -> Option<RemoteEngines> {
    let (info, token) = discovery::find(paths).await?;
    if info.pid == std::process::id() {
        return None;
    }
    let client = crate::cli::Client {
        server: ServerRef {
            url: settings::client_url(&info.host, info.port),
            token,
        },
        http: http.clone(),
    };
    let v = client.status().await.ok()?;
    Some(RemoteEngines {
        pid: info.pid,
        engines: serde_json::from_value(v.get("engines")?.clone()).ok()?,
        installed: v.get("installed").and_then(Value::as_bool).unwrap_or(false),
    })
}
