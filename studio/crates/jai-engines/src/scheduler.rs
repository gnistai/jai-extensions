//! The scheduler: a single actor that keeps one engine loaded, batches
//! requests of the loaded kind and swaps when the other kind waits (DESIGN.md
//! section 6, "Scheduler policy").

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use jai_core::Config;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch};

use crate::launcher::Launcher;
use crate::{EngineKind, EngineState};

mod actor;
#[cfg(test)]
mod tests;

use actor::{Actor, Msg, ReleaseGuard, Shared};

/// The default `start_timeout`.
pub const DEFAULT_START_TIMEOUT: Duration = Duration::from_secs(240);

#[derive(Clone, Debug)]
pub struct SchedulerConfig {
    /// Concurrent LLM leases (1). Zero counts as one.
    pub max_llm: usize,
    /// Concurrent image leases (1). Zero counts as one.
    pub max_image: usize,
    /// After this long, a waiter of the other kind forces a swap (120 s).
    /// The wait counts from when the loaded engine became ready, so each
    /// engine gets at least this long before it is swapped out.
    pub max_batch_wait: Duration,
    /// Stop the engine after this long with nothing active or waiting.
    pub idle_unload: Option<Duration>,
    /// Longest an engine may take to become ready (240 s).
    pub start_timeout: Duration,
    /// Start the LLM with MTP; on a failed start it retries once without.
    pub mtp: bool,
}

impl Default for SchedulerConfig {
    /// One lease per kind, 120 s batches, idle unload after 15 minutes, a
    /// 240 s start timeout and MTP on, matching the default config.
    fn default() -> Self {
        SchedulerConfig {
            max_llm: 1,
            max_image: 1,
            max_batch_wait: Duration::from_secs(120),
            idle_unload: Some(Duration::from_secs(15 * 60)),
            start_timeout: DEFAULT_START_TIMEOUT,
            mtp: true,
        }
    }
}

impl SchedulerConfig {
    /// The defaults with `llm.parallel` LLM leases, `idleUnloadMinutes` (0
    /// turns idle unload off) and `llm.mtp` from the settings.
    pub fn from_config(cfg: &Config) -> Self {
        SchedulerConfig {
            max_llm: usize::try_from(cfg.llm.parallel).unwrap_or(1).max(1),
            idle_unload: (cfg.idle_unload_minutes > 0)
                .then(|| Duration::from_secs(u64::from(cfg.idle_unload_minutes) * 60)),
            mtp: cfg.llm.mtp,
            ..SchedulerConfig::default()
        }
    }
}

/// Handle to the scheduler actor.
pub struct Scheduler {
    tx: mpsc::UnboundedSender<Msg>,
    status: watch::Receiver<SchedulerStatus>,
    shared: Arc<Shared>,
    next_id: AtomicU64,
}

impl Scheduler {
    /// Spawns the actor; call it inside a Tokio runtime. When the scheduler
    /// and every lease are dropped, the actor stops the engine and ends.
    pub fn new(launcher: Arc<dyn Launcher>, cfg: SchedulerConfig) -> Arc<Scheduler> {
        let (tx, rx) = mpsc::unbounded_channel();
        let (status_tx, status) = watch::channel(SchedulerStatus::initial(cfg.mtp));
        let shared = Arc::new(Shared::default());
        let actor = Actor::new(
            launcher,
            cfg,
            rx,
            tx.downgrade(),
            status_tx,
            Arc::clone(&shared),
        );
        tokio::spawn(actor.run());
        Arc::new(Scheduler {
            tx,
            status,
            shared,
            next_id: AtomicU64::new(0),
        })
    }

    /// Waits for the engine of this kind. Cancel-safe: dropping the future
    /// leaves the queue.
    pub async fn acquire(self: &Arc<Self>, kind: EngineKind) -> Result<Lease, EngineError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply, answer) = oneshot::channel();
        self.tx
            .send(Msg::Acquire { id, kind, reply })
            .map_err(|_| EngineError::ShuttingDown)?;
        let mut cancel = CancelOnDrop {
            tx: &self.tx,
            id: Some(id),
        };
        let answer = answer.await;
        cancel.id = None;
        // A dropped reply means the actor has ended.
        answer.unwrap_or(Err(EngineError::ShuttingDown))
    }

    pub fn status(&self) -> SchedulerStatus {
        self.status.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<SchedulerStatus> {
        self.status.clone()
    }

    /// Stops the loaded engine once active leases end; waiters restart it.
    /// Returns when the engine has stopped (at once if none is loaded).
    pub async fn unload(&self) {
        let (done, finished) = oneshot::channel();
        if self.tx.send(Msg::Unload { done }).is_ok() {
            let _ = finished.await;
        }
    }

    /// Stops everything, fails waiters, and starts nothing afterwards.
    /// Returns when the engine has stopped.
    pub async fn shutdown(&self) {
        let (done, finished) = oneshot::channel();
        if self.tx.send(Msg::Shutdown { done }).is_ok() {
            let _ = finished.await;
        }
    }

    /// Changes the idle-unload time (`SchedulerConfig::idle_unload`) without
    /// a restart. It counts from when the engine became idle, so an engine
    /// idle for longer than the new time stops at once.
    pub fn set_idle_unload(&self, idle: Option<Duration>) {
        let _ = self.tx.send(Msg::SetIdleUnload { idle });
    }

    /// The last `n` log lines of the engine of this kind: live while it is
    /// loaded, otherwise from its last run (including a failed start).
    pub fn logs(&self, kind: EngineKind, n: usize) -> Vec<String> {
        self.shared.logs(kind, n)
    }
}

/// Tells the actor that an `acquire` future was dropped before its answer.
struct CancelOnDrop<'a> {
    tx: &'a mpsc::UnboundedSender<Msg>,
    id: Option<u64>,
}

impl Drop for CancelOnDrop<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            let _ = self.tx.send(Msg::Cancel { id });
        }
    }
}

/// Use of the loaded engine. Dropping it releases the slot through a private
/// release-on-drop guard.
#[non_exhaustive]
pub struct Lease {
    pub kind: EngineKind,
    pub base_url: String,
    pub api_key: Option<String>,
    _release: ReleaseGuard,
}

impl fmt::Debug for Lease {
    /// Without the API key.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Lease")
            .field("kind", &self.kind)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SchedulerStatus {
    /// The engine `state` is about: the one loaded, starting or stopping, or
    /// the one that failed. None when nothing is loaded.
    pub loaded: Option<EngineKind>,
    pub state: EngineState,
    /// Leases on the loaded engine.
    pub active: usize,
    pub waiting_llm: usize,
    pub waiting_image: usize,
    /// False once the LLM failed to start with MTP in this session.
    pub mtp: bool,
    /// Why the last start failed or the last engine crashed; cleared when that
    /// engine is ready again.
    pub last_error: Option<String>,
    /// Times one engine was stopped to start the other.
    pub swaps: u64,
    /// Unix time in milliseconds when `loaded` or `state` last changed.
    pub since_ms: u64,
}

impl SchedulerStatus {
    fn initial(mtp: bool) -> Self {
        SchedulerStatus {
            loaded: None,
            state: EngineState::Stopped,
            active: 0,
            waiting_llm: 0,
            waiting_image: 0,
            mtp,
            last_error: None,
            swaps: 0,
            since_ms: actor::unix_ms(),
        }
    }
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum EngineError {
    #[error("not installed: {0}")]
    NotInstalled(String),
    #[error("the {kind} engine failed to start: {message}")]
    StartFailed {
        kind: EngineKind,
        message: String,
        /// The last 40 log lines.
        log: Vec<String>,
    },
    #[error("JAI Studio is shutting down")]
    ShuttingDown,
}
