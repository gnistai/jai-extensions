//! The scheduler's actor: one task that owns the waiters and the loaded engine
//! and applies the policy in DESIGN.md section 6. It never awaits a start or
//! a stop inline; both are futures it polls next to its messages, so status,
//! queue counts and cancellations stay current while an engine loads.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::{EngineError, Lease, SchedulerConfig, SchedulerStatus};
use crate::launcher::{
    InvalidSettings, LaunchAttempt, Launcher, NoRoomForMtp, NotInstalled, RunningEngine, StartError,
};
use crate::{EngineKind, EngineState};

/// How often a ready engine is checked for a crash.
const CRASH_POLL: Duration = Duration::from_secs(1);
/// How long a launcher gets, after its start is cancelled, to stop what it
/// started (SIGTERM, 3 s, SIGKILL and a wait take up to 15 s). Then the start
/// future is dropped, which kills the engine without waiting.
pub(super) const ABANDON_GRACE: Duration = Duration::from_secs(20);
/// Log lines in `EngineError::StartFailed`.
const FAILURE_LOG_LINES: usize = 40;
/// Log lines kept from each engine's last run.
const SAVED_LOG_LINES: usize = 1000;
/// The soonest the actor wakes for a deadline. Every deadline is acted on when
/// it passes; the floor only keeps a mistake from becoming a busy loop.
const MIN_WAKE: Duration = Duration::from_millis(1);

type Reply = oneshot::Sender<Result<Lease, EngineError>>;
type StartFuture = Pin<Box<dyn Future<Output = anyhow::Result<Box<dyn RunningEngine>>> + Send>>;
type StopFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Messages from the handle, the acquire futures and the leases.
pub(super) enum Msg {
    Acquire {
        id: u64,
        kind: EngineKind,
        reply: Reply,
    },
    /// An acquire future was dropped while it waited.
    Cancel {
        id: u64,
    },
    /// A lease on the engine of this generation ended.
    Release {
        generation: u64,
    },
    Unload {
        done: oneshot::Sender<()>,
    },
    Shutdown {
        done: oneshot::Sender<()>,
    },
    /// A new idle-unload time, counted from when the engine became idle.
    SetIdleUnload {
        idle: Option<Duration>,
    },
}

/// Sends `Release` when its lease is dropped.
pub(super) struct ReleaseGuard {
    tx: Option<mpsc::UnboundedSender<Msg>>,
    generation: u64,
}

impl ReleaseGuard {
    fn disarm(&mut self) {
        self.tx = None;
    }
}

impl Drop for ReleaseGuard {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Msg::Release {
                generation: self.generation,
            });
        }
    }
}

/// What the handle reads without asking the actor: the loaded engine, for live
/// logs, and the logs of each engine's last run.
#[derive(Default)]
pub(super) struct Shared {
    loaded: Mutex<Option<Loaded>>,
    saved: Mutex<[Vec<String>; 2]>,
}

struct Loaded {
    kind: EngineKind,
    engine: Box<dyn RunningEngine>,
}

impl Shared {
    pub(super) fn logs(&self, kind: EngineKind, n: usize) -> Vec<String> {
        if let Some(loaded) = lock(&self.loaded).as_ref() {
            if loaded.kind == kind {
                return loaded.engine.log_tail(n);
            }
        }
        tail(&lock(&self.saved)[slot(kind)], n)
    }

    fn save_logs(&self, kind: EngineKind, mut lines: Vec<String>) {
        let excess = lines.len().saturating_sub(SAVED_LOG_LINES);
        lines.drain(..excess);
        lock(&self.saved)[slot(kind)] = lines;
    }

    fn put(&self, kind: EngineKind, engine: Box<dyn RunningEngine>) {
        *lock(&self.loaded) = Some(Loaded { kind, engine });
    }

    fn take(&self) -> Option<Box<dyn RunningEngine>> {
        lock(&self.loaded).take().map(|loaded| loaded.engine)
    }

    /// Whether the loaded engine has exited (true if there is none).
    fn exited(&self) -> bool {
        lock(&self.loaded)
            .as_mut()
            .is_none_or(|loaded| loaded.engine.exited())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn slot(kind: EngineKind) -> usize {
    match kind {
        EngineKind::Llm => 0,
        EngineKind::Image => 1,
    }
}

fn other(kind: EngineKind) -> EngineKind {
    match kind {
        EngineKind::Llm => EngineKind::Image,
        EngineKind::Image => EngineKind::Llm,
    }
}

fn tail(lines: &[String], n: usize) -> Vec<String> {
    lines[lines.len().saturating_sub(n)..].to_vec()
}

pub(super) fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

struct Waiter {
    id: u64,
    kind: EngineKind,
    since: Instant,
    reply: Reply,
}

enum Phase {
    /// Nothing loaded. `failed` names the engine whose start failed or that
    /// crashed.
    Idle { failed: Option<EngineKind> },
    Starting {
        kind: EngineKind,
        mtp: bool,
        fut: StartFuture,
        /// Cancels the launch (shutdown); the future then ends once the
        /// launcher has stopped what it started.
        cancel: CancellationToken,
        /// Set when the start was cancelled: past it, the future is dropped.
        abandon_at: Option<Instant>,
    },
    Ready {
        kind: EngineKind,
        base_url: String,
        api_key: Option<String>,
        ready_at: Instant,
    },
    /// Then start `next` if it still has waiters. A crashed engine is stopped
    /// to clean up while the state already reads Failed.
    Stopping {
        kind: EngineKind,
        fut: StopFuture,
        next: Option<EngineKind>,
        crashed: bool,
    },
}

enum Event {
    Started(anyhow::Result<Box<dyn RunningEngine>>),
    Stopped,
}

pub(super) struct Actor {
    launcher: Arc<dyn Launcher>,
    cfg: SchedulerConfig,
    rx: mpsc::UnboundedReceiver<Msg>,
    rx_open: bool,
    /// For new leases; weak, so the actor ends once every handle is gone.
    tx: mpsc::WeakUnboundedSender<Msg>,
    status: watch::Sender<SchedulerStatus>,
    shared: Arc<Shared>,
    phase: Phase,
    /// Arrival order, both kinds.
    waiters: VecDeque<Waiter>,
    /// Leases on the loaded engine.
    active: usize,
    /// Numbers the engines that became ready; a lease carries its engine's.
    generation: u64,
    swap_pending: bool,
    unload_pending: bool,
    unload_done: Vec<oneshot::Sender<()>>,
    shutting_down: bool,
    shutdown_done: Vec<oneshot::Sender<()>>,
    mtp: bool,
    /// MTP for the LLM that runs or starts now: off for one start when VRAM
    /// is short (NoRoomForMtp), while `mtp` stays on for the next.
    llm_mtp: bool,
    last_error: Option<(EngineKind, String)>,
    swaps: u64,
    idle_since: Option<Instant>,
    next_crash_check: Instant,
    shown: (Option<EngineKind>, EngineState),
    since_ms: u64,
}

impl Actor {
    pub(super) fn new(
        launcher: Arc<dyn Launcher>,
        cfg: SchedulerConfig,
        rx: mpsc::UnboundedReceiver<Msg>,
        tx: mpsc::WeakUnboundedSender<Msg>,
        status: watch::Sender<SchedulerStatus>,
        shared: Arc<Shared>,
    ) -> Actor {
        let since_ms = status.borrow().since_ms;
        Actor {
            launcher,
            mtp: cfg.mtp,
            llm_mtp: cfg.mtp,
            cfg,
            rx,
            rx_open: true,
            tx,
            status,
            shared,
            phase: Phase::Idle { failed: None },
            waiters: VecDeque::new(),
            active: 0,
            generation: 0,
            swap_pending: false,
            unload_pending: false,
            unload_done: Vec::new(),
            shutting_down: false,
            shutdown_done: Vec::new(),
            last_error: None,
            swaps: 0,
            idle_since: None,
            next_crash_check: Instant::now(),
            shown: (None, EngineState::Stopped),
            since_ms,
        }
    }

    pub(super) async fn run(mut self) {
        loop {
            self.pump();
            self.publish();
            if self.shutting_down && matches!(self.phase, Phase::Idle { .. }) {
                break;
            }
            let deadline = self.next_deadline();
            tokio::select! {
                msg = self.rx.recv(), if self.rx_open => match msg {
                    Some(msg) => self.on_msg(msg),
                    None => {
                        // Every handle, lease and acquire future is gone.
                        self.rx_open = false;
                        self.shutting_down = true;
                    }
                },
                event = phase_event(&mut self.phase) => self.on_event(event),
                () = wake_at(deadline) => self.on_timer(),
            }
        }
        self.phase = Phase::Idle { failed: None };
        self.finish_unload();
        for done in self.shutdown_done.drain(..) {
            let _ = done.send(());
        }
        self.publish();
        info!("scheduler stopped");
    }

    fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Acquire { id, kind, reply } => {
                if self.shutting_down {
                    let _ = reply.send(Err(EngineError::ShuttingDown));
                } else {
                    self.waiters.push_back(Waiter {
                        id,
                        kind,
                        since: Instant::now(),
                        reply,
                    });
                }
            }
            Msg::Cancel { id } => self.waiters.retain(|w| w.id != id),
            Msg::Release { generation } => {
                if generation == self.generation && matches!(self.phase, Phase::Ready { .. }) {
                    self.active = self.active.saturating_sub(1);
                }
            }
            Msg::Unload { done } => {
                if matches!(self.phase, Phase::Idle { .. }) {
                    let _ = done.send(());
                } else {
                    info!("unloading once active requests end");
                    self.unload_pending = true;
                    self.unload_done.push(done);
                }
            }
            Msg::Shutdown { done } => {
                if !self.shutting_down {
                    info!("scheduler shutting down");
                }
                self.shutting_down = true;
                self.shutdown_done.push(done);
            }
            Msg::SetIdleUnload { idle } => self.cfg.idle_unload = idle,
        }
    }

    fn on_event(&mut self, event: Event) {
        match event {
            Event::Started(result) => {
                let Phase::Starting { kind, mtp, .. } = self.phase else {
                    return;
                };
                match result {
                    // During a shutdown it is stopped at once (pump_shutdown).
                    Ok(engine) => self.on_ready(kind, engine),
                    Err(err) if self.shutting_down => {
                        debug!(engine = %kind, "the cancelled start ended: {err:#}");
                        self.phase = Phase::Idle { failed: None };
                    }
                    Err(err) => self.on_start_failed(kind, mtp, err),
                }
            }
            Event::Stopped => {
                let Phase::Stopping {
                    kind,
                    next,
                    crashed,
                    ..
                } = self.phase
                else {
                    return;
                };
                info!(engine = %kind, "engine stopped");
                self.finish_unload();
                self.phase = Phase::Idle {
                    failed: crashed.then_some(kind),
                };
                if let Some(next) = next {
                    if !self.shutting_down && self.count(next) > 0 {
                        self.begin_start(next);
                    }
                }
            }
        }
    }

    fn on_timer(&mut self) {
        let now = Instant::now();
        if matches!(self.phase, Phase::Ready { .. }) && now >= self.next_crash_check {
            self.next_crash_check = now + CRASH_POLL;
            if self.shared.exited() {
                self.on_crash();
            }
        }
        if let Phase::Starting {
            kind,
            abandon_at: Some(at),
            ..
        } = self.phase
        {
            if now >= at {
                warn!(engine = %kind, "the cancelled start did not end in {} s; killing the engine", ABANDON_GRACE.as_secs());
                // Dropping the start future kills the half-started engine.
                self.phase = Phase::Idle { failed: None };
            }
        }
    }

    /// Applies the policy after every event.
    fn pump(&mut self) {
        self.waiters.retain(|w| !w.reply.is_closed());
        if self.shutting_down {
            self.pump_shutdown();
            return;
        }
        match &self.phase {
            Phase::Idle { .. } => {
                self.finish_unload();
                // Rule 3: first start, for the oldest waiter.
                if let Some(kind) = self.waiters.front().map(|w| w.kind) {
                    self.begin_start(kind);
                }
            }
            Phase::Ready { kind, ready_at, .. } => {
                let (kind, ready_at) = (*kind, *ready_at);
                self.pump_ready(kind, ready_at);
            }
            Phase::Starting { .. } | Phase::Stopping { .. } => {}
        }
    }

    fn pump_ready(&mut self, kind: EngineKind, ready_at: Instant) {
        let now = Instant::now();
        // Rule 7: unload once active leases end; waiters start it again.
        if self.unload_pending {
            self.idle_since = None;
            if self.active == 0 {
                self.begin_stop(None);
            }
            return;
        }
        // Rule 2: swap pending; stop granting K, swap once K is idle.
        self.mark_swap(kind, ready_at, now);
        if self.swap_pending {
            self.idle_since = None;
            if self.active == 0 {
                self.swaps += 1;
                self.begin_stop(Some(other(kind)));
            }
            return;
        }
        // Rule 1: grant the oldest waiters of K up to max(K).
        let max = self.max_leases(kind);
        if self.active < max && self.count(kind) > 0 && self.shared.exited() {
            // Rule 5 again: do not hand out an engine that has just died.
            self.on_crash();
            return;
        }
        while self.active < max {
            let Some(pos) = self.waiters.iter().position(|w| w.kind == kind) else {
                break;
            };
            if let Some(waiter) = self.waiters.remove(pos) {
                self.grant(waiter);
            }
        }
        // Granting may have taken the last waiter of K.
        self.mark_swap(kind, ready_at, now);
        // Rule 6: idle unload.
        if self.active == 0 && self.waiters.is_empty() {
            let since = *self.idle_since.get_or_insert(now);
            let expired = self
                .cfg
                .idle_unload
                .is_some_and(|idle| now.duration_since(since) >= idle);
            if expired {
                info!(engine = %kind, "unloading the idle engine");
                self.begin_stop(None);
            }
        } else {
            self.idle_since = None;
        }
    }

    /// Marks a swap pending when the other kind waits and either nothing of
    /// this kind waits or the other kind's oldest waiter has waited
    /// `max_batch_wait` (counted from when this engine became ready). Clears
    /// it if the other kind's waiters have all gone.
    fn mark_swap(&mut self, kind: EngineKind, ready_at: Instant, now: Instant) {
        let other = other(kind);
        let Some(oldest) = self.oldest(other) else {
            self.swap_pending = false;
            return;
        };
        if self.swap_pending {
            return;
        }
        let starving = now.duration_since(oldest.since.max(ready_at)) >= self.cfg.max_batch_wait;
        if starving || self.count(kind) == 0 {
            info!(from = %kind, to = %other, starving, "swap pending");
            self.swap_pending = true;
        }
    }

    fn grant(&mut self, waiter: Waiter) {
        let Phase::Ready {
            kind,
            base_url,
            api_key,
            ..
        } = &self.phase
        else {
            return;
        };
        let Some(tx) = self.tx.upgrade() else {
            return;
        };
        let lease = Lease {
            kind: *kind,
            base_url: base_url.clone(),
            api_key: api_key.clone(),
            _release: ReleaseGuard {
                tx: Some(tx),
                generation: self.generation,
            },
        };
        self.active += 1;
        if let Err(Ok(mut lease)) = waiter.reply.send(Ok(lease)) {
            // The acquire future went away just now.
            lease._release.disarm();
            self.active -= 1;
        }
    }

    fn begin_start(&mut self, kind: EngineKind) {
        self.begin_start_with(kind, kind == EngineKind::Llm && self.mtp);
    }

    fn begin_start_with(&mut self, kind: EngineKind, mtp: bool) {
        if kind == EngineKind::Llm {
            self.llm_mtp = mtp;
        }
        let cancel = CancellationToken::new();
        let attempt = LaunchAttempt {
            mtp,
            cancel: cancel.clone(),
        };
        info!(engine = %kind, mtp, "starting engine");
        let fut: StartFuture = Box::pin(start_within(
            Arc::clone(&self.launcher),
            kind,
            attempt,
            self.cfg.start_timeout,
        ));
        self.phase = Phase::Starting {
            kind,
            mtp,
            fut,
            cancel,
            abandon_at: None,
        };
    }

    fn on_ready(&mut self, kind: EngineKind, engine: Box<dyn RunningEngine>) {
        let now = Instant::now();
        self.generation += 1;
        let base_url = engine.base_url();
        let api_key = engine.api_key();
        self.shared.put(kind, engine);
        self.active = 0;
        self.swap_pending = false;
        self.idle_since = None;
        self.next_crash_check = now + CRASH_POLL;
        if self.last_error.as_ref().is_some_and(|(k, _)| *k == kind) {
            self.last_error = None;
        }
        info!(engine = %kind, %base_url, "engine ready");
        self.phase = Phase::Ready {
            kind,
            base_url,
            api_key,
            ready_at: now,
        };
    }

    /// Rule 4: the LLM retries once without MTP (and keeps MTP off);
    /// otherwise the waiters of this kind fail.
    fn on_start_failed(&mut self, kind: EngineKind, mtp: bool, err: anyhow::Error) {
        let missing = err.downcast_ref::<NotInstalled>().map(|e| e.0.clone());
        let settings = err.downcast_ref::<InvalidSettings>().is_some();
        let (message, log) = match err.downcast_ref::<StartError>() {
            Some(e) => (e.message.clone(), e.log.clone()),
            None => (format!("{err:#}"), Vec::new()),
        };
        self.shared.save_logs(kind, log.clone());
        if err.downcast_ref::<NoRoomForMtp>().is_some()
            && kind == EngineKind::Llm
            && mtp
            && !self.shutting_down
        {
            info!("{message}; starting the llm engine without MTP this time");
            self.begin_start_with(kind, false);
            return;
        }
        let retry = missing.is_none() && !settings;
        if retry && kind == EngineKind::Llm && mtp && !self.shutting_down {
            warn!("the llm engine failed to start with MTP ({message}); retrying without MTP for this session");
            self.mtp = false;
            self.begin_start(kind);
            return;
        }
        let error = match missing {
            Some(what) => EngineError::NotInstalled(what),
            None => EngineError::StartFailed {
                kind,
                message,
                log: tail(&log, FAILURE_LOG_LINES),
            },
        };
        warn!("{error}");
        let (failed, kept): (VecDeque<Waiter>, VecDeque<Waiter>) =
            self.waiters.drain(..).partition(|w| w.kind == kind);
        self.waiters = kept;
        for waiter in failed {
            let _ = waiter.reply.send(Err(error.clone()));
        }
        self.last_error = Some((kind, error.to_string()));
        self.phase = Phase::Idle { failed: Some(kind) };
    }

    /// Rule 5: a ready engine exited. Its leases are void; the next acquire
    /// restarts it.
    fn on_crash(&mut self) {
        let Phase::Ready { kind, .. } = self.phase else {
            return;
        };
        let engine = self.shared.take();
        let log = engine
            .as_ref()
            .map(|e| e.log_tail(SAVED_LOG_LINES))
            .unwrap_or_default();
        let message = match log.iter().rev().map(|l| l.trim()).find(|l| !l.is_empty()) {
            Some(last) => format!("the {kind} engine exited unexpectedly: {last}"),
            None => format!("the {kind} engine exited unexpectedly"),
        };
        warn!("{message}");
        self.shared.save_logs(kind, log);
        self.last_error = Some((kind, message));
        self.active = 0;
        self.swap_pending = false;
        self.idle_since = None;
        let fut: StopFuture = match engine {
            Some(engine) => engine.stop(),
            None => Box::pin(std::future::ready(())),
        };
        self.phase = Phase::Stopping {
            kind,
            fut,
            next: None,
            crashed: true,
        };
    }

    fn begin_stop(&mut self, next: Option<EngineKind>) {
        let Phase::Ready { kind, .. } = self.phase else {
            return;
        };
        let fut: StopFuture = match self.shared.take() {
            Some(engine) => {
                self.shared
                    .save_logs(kind, engine.log_tail(SAVED_LOG_LINES));
                engine.stop()
            }
            None => Box::pin(std::future::ready(())),
        };
        info!(engine = %kind, next = ?next, "stopping engine");
        self.active = 0;
        self.swap_pending = false;
        self.idle_since = None;
        self.phase = Phase::Stopping {
            kind,
            fut,
            next,
            crashed: false,
        };
    }

    /// Fails every waiter, cancels a start and stops a ready engine without
    /// waiting for its leases. A cancelled start is waited for, so the engine
    /// is gone (and its VRAM free) when the shutdown completes; one that has
    /// not ended after [`ABANDON_GRACE`] is dropped (see `on_timer`).
    fn pump_shutdown(&mut self) {
        for waiter in self.waiters.drain(..) {
            let _ = waiter.reply.send(Err(EngineError::ShuttingDown));
        }
        match &mut self.phase {
            Phase::Starting {
                kind,
                cancel,
                abandon_at,
                ..
            } => {
                if abandon_at.is_none() {
                    debug!(engine = %kind, "cancelling the start");
                    cancel.cancel();
                    *abandon_at = Some(Instant::now() + ABANDON_GRACE);
                }
            }
            Phase::Ready { .. } => self.begin_stop(None),
            Phase::Idle { .. } | Phase::Stopping { .. } => {}
        }
    }

    fn finish_unload(&mut self) {
        self.unload_pending = false;
        for done in self.unload_done.drain(..) {
            let _ = done.send(());
        }
    }

    /// The next time-based check: crash poll, idle unload, anti-starvation,
    /// or the end of a cancelled start's grace.
    fn next_deadline(&self) -> Option<Instant> {
        if let Phase::Starting {
            abandon_at: Some(at),
            ..
        } = self.phase
        {
            return Some(at.max(Instant::now() + MIN_WAKE));
        }
        let Phase::Ready { kind, ready_at, .. } = &self.phase else {
            return None;
        };
        let mut deadline = self.next_crash_check;
        if let (Some(since), Some(idle)) = (self.idle_since, self.cfg.idle_unload) {
            if let Some(at) = since.checked_add(idle) {
                deadline = deadline.min(at);
            }
        }
        if !self.swap_pending && !self.unload_pending && self.count(*kind) > 0 {
            if let Some(oldest) = self.oldest(other(*kind)) {
                if let Some(at) = oldest
                    .since
                    .max(*ready_at)
                    .checked_add(self.cfg.max_batch_wait)
                {
                    deadline = deadline.min(at);
                }
            }
        }
        Some(deadline.max(Instant::now() + MIN_WAKE))
    }

    fn publish(&mut self) {
        let (loaded, state) = match &self.phase {
            Phase::Idle { failed: Some(kind) } => (Some(*kind), EngineState::Failed),
            Phase::Idle { failed: None } => (None, EngineState::Stopped),
            Phase::Starting {
                kind,
                abandon_at: Some(_),
                ..
            } => (Some(*kind), EngineState::Stopping),
            Phase::Starting { kind, .. } => (Some(*kind), EngineState::Starting),
            Phase::Ready { kind, .. } => (Some(*kind), EngineState::Ready),
            Phase::Stopping {
                kind,
                crashed: true,
                ..
            } => (Some(*kind), EngineState::Failed),
            Phase::Stopping { kind, .. } => (Some(*kind), EngineState::Stopping),
        };
        if (loaded, state) != self.shown {
            self.shown = (loaded, state);
            self.since_ms = unix_ms();
        }
        let status = SchedulerStatus {
            loaded,
            state,
            active: self.active,
            waiting_llm: self.count(EngineKind::Llm),
            waiting_image: self.count(EngineKind::Image),
            mtp: self.llm_mtp,
            last_error: self.last_error.as_ref().map(|(_, message)| message.clone()),
            swaps: self.swaps,
            since_ms: self.since_ms,
        };
        if *self.status.borrow() != status {
            self.status.send_replace(status);
        }
    }

    fn count(&self, kind: EngineKind) -> usize {
        self.waiters.iter().filter(|w| w.kind == kind).count()
    }

    /// Waiters are kept in arrival order, so the first of a kind is the oldest.
    fn oldest(&self, kind: EngineKind) -> Option<&Waiter> {
        self.waiters.iter().find(|w| w.kind == kind)
    }

    fn max_leases(&self, kind: EngineKind) -> usize {
        match kind {
            EngineKind::Llm => self.cfg.max_llm,
            EngineKind::Image => self.cfg.max_image,
        }
        .max(1)
    }
}

/// Runs a launch for at most `limit`. At the limit it cancels the attempt and
/// waits up to [`ABANDON_GRACE`] for the launcher to stop what it started (an
/// engine that became ready just then is stopped here), so a retry never
/// overlaps the old process. Then it reports the timeout.
async fn start_within(
    launcher: Arc<dyn Launcher>,
    kind: EngineKind,
    attempt: LaunchAttempt,
    limit: Duration,
) -> anyhow::Result<Box<dyn RunningEngine>> {
    let cancel = attempt.cancel.clone();
    let mut launch = launcher.launch(kind, attempt);
    if let Ok(result) = tokio::time::timeout(limit, &mut launch).await {
        return result;
    }
    cancel.cancel();
    match tokio::time::timeout(ABANDON_GRACE, launch).await {
        Ok(Ok(engine)) => engine.stop().await,
        Ok(Err(_)) => {}
        Err(_) => {
            warn!(engine = %kind, "the timed-out start did not end in {} s; killing the engine", ABANDON_GRACE.as_secs())
        }
    }
    Err(anyhow::anyhow!("not ready after {} s", limit.as_secs()))
}

/// Completes when the phase's start or stop does; never in Idle or Ready.
async fn phase_event(phase: &mut Phase) -> Event {
    match phase {
        Phase::Starting { fut, .. } => Event::Started(fut.await),
        Phase::Stopping { fut, .. } => {
            fut.await;
            Event::Stopped
        }
        Phase::Idle { .. } | Phase::Ready { .. } => std::future::pending().await,
    }
}

async fn wake_at(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}
