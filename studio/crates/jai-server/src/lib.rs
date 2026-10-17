//! JAI Studio server: the shared [`Core`], the HTTP API on 127.0.0.1 with its
//! request rules, discovery through `server.json`, and the GPU lock. See
//! `studio/DESIGN.md` section 7.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Context;
use jai_core::{Config, Paths, Selection};
use jai_engines::{ProcessLauncher, Scheduler, SchedulerConfig};

pub mod discovery;
pub mod gpu_lock;

mod auth;
mod body;
mod chat;
mod chat_image;
mod error;
mod images;
mod proof;
mod server;
mod status;

pub use server::{router, start, ServerHandle};

/// The server's version, reported by `/health`, `/v1/status` and `server.json`.
pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What the server, the CLI and the GUI share.
pub struct Core {
    pub paths: Paths,
    pub config: Arc<RwLock<Config>>,
    pub token: Arc<RwLock<String>>,
    pub selection: Selection,
    pub scheduler: Arc<Scheduler>,
    /// The client for the engines. They only listen on loopback, so it never
    /// uses a proxy.
    pub http: reqwest::Client,
}

impl Core {
    /// Loads the token, picks the backend and the catalog selection for this
    /// PC, and builds the `ProcessLauncher` and the `Scheduler`.
    pub fn new(paths: Paths, cfg: Config) -> anyhow::Result<Arc<Core>> {
        paths
            .ensure_dirs()
            .with_context(|| format!("could not create {}", paths.root.display()))?;
        let token = jai_core::token::load_or_create_token(&paths)?;
        let gpu = jai_core::gpu::detect();
        let backend = jai_core::gpu::effective_backend(cfg.backend, gpu.as_ref());
        let selection = jai_core::catalog::select(
            jai_core::catalog::catalog(),
            jai_core::catalog::current_os(),
            backend,
        );
        tracing::info!(
            gpu = gpu.as_ref().map(|g| g.name.as_str()).unwrap_or("none"),
            llm_backend = ?selection.llm_backend,
            image_backend = ?selection.image_backend,
            "engine selection"
        );
        let launcher = Arc::new(ProcessLauncher::new(
            paths.clone(),
            cfg.clone(),
            selection.clone(),
        ));
        let scheduler = Scheduler::new(launcher, SchedulerConfig::from_config(&cfg));
        Ok(Arc::new(Core {
            paths,
            config: Arc::new(RwLock::new(cfg)),
            token: Arc::new(RwLock::new(token)),
            selection,
            scheduler,
            http: engine_client()?,
        }))
    }
}

/// The HTTP client for the engines on their random loopback ports. It follows
/// no redirects: the engines send none, and a request with the key or a
/// user's prompt must not go anywhere else.
fn engine_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .pool_idle_timeout(Duration::from_secs(30))
        .user_agent(concat!("jai-studio/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("could not build the engine HTTP client")
}

/// Reads through a lock even if a panicking writer poisoned it: the values
/// behind it (config, token) are always whole.
pub(crate) fn read_lock<T, R>(lock: &RwLock<T>, f: impl FnOnce(&T) -> R) -> R {
    match lock.read() {
        Ok(guard) => f(&guard),
        Err(poisoned) => f(&poisoned.into_inner()),
    }
}
