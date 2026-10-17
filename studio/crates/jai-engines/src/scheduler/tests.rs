//! Scheduler tests with a fake launcher and no processes. Time is paused, so
//! waits of minutes take no real time.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout, Instant};

use super::*;
use crate::launcher::{
    InvalidSettings, LaunchAttempt, NoRoomForMtp, NotInstalled, RunningEngine, StartError,
};

const LLM: EngineKind = EngineKind::Llm;
const IMAGE: EngineKind = EngineKind::Image;

/// What the fake launcher and its engines saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ev {
    Launch(EngineKind, bool),
    Ready(EngineKind, u64),
    Stop(EngineKind, u64),
    /// A launch future was dropped before it finished.
    Abort(EngineKind),
    /// A launch saw its cancel and finished stopping its engine.
    Cancelled(EngineKind),
}

#[derive(Default)]
struct Behavior {
    start_delay: Duration,
    /// Starts that fail, per kind (llm, image), counted down.
    failures: [usize; 2],
    /// LLM starts with MTP on fail.
    fail_with_mtp: bool,
    /// LLM starts with MTP on find too little VRAM, counted down.
    no_room_for_mtp: usize,
    not_installed: bool,
    invalid_settings: bool,
    /// Starts that never finish on their own, counted down.
    hangs: usize,
    /// Starts ignore their cancel.
    ignore_cancel: bool,
    /// How long a cancelled start takes to stop its engine.
    stop_delay: Duration,
}

#[derive(Default)]
struct FakeLauncher {
    behavior: Mutex<Behavior>,
    events: Arc<Mutex<Vec<Ev>>>,
    next_id: AtomicU64,
    crash_flags: Mutex<Vec<(EngineKind, Arc<AtomicBool>)>>,
}

impl FakeLauncher {
    fn set(&self, change: impl FnOnce(&mut Behavior)) {
        change(&mut self.behavior.lock().unwrap());
    }

    fn events(&self) -> Vec<Ev> {
        self.events.lock().unwrap().clone()
    }

    fn launches(&self) -> Vec<(EngineKind, bool)> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                Ev::Launch(kind, mtp) => Some((kind, mtp)),
                _ => None,
            })
            .collect()
    }

    /// Makes the newest engine of this kind report that it exited.
    fn crash_latest(&self, kind: EngineKind) {
        let flags = self.crash_flags.lock().unwrap();
        let (_, flag) = flags
            .iter()
            .rev()
            .find(|(k, _)| *k == kind)
            .expect("an engine");
        flag.store(true, Ordering::SeqCst);
    }
}

/// Records `Abort` if the launch future is dropped early.
struct AbortOnDrop {
    events: Arc<Mutex<Vec<Ev>>>,
    kind: EngineKind,
    armed: bool,
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.events.lock().unwrap().push(Ev::Abort(self.kind));
        }
    }
}

enum Outcome {
    Ready,
    Fail,
    FailMtp,
    NoRoom,
    NotInstalled,
    InvalidSettings,
}

#[async_trait::async_trait]
impl Launcher for FakeLauncher {
    async fn launch(
        &self,
        kind: EngineKind,
        attempt: LaunchAttempt,
    ) -> anyhow::Result<Box<dyn RunningEngine>> {
        self.events
            .lock()
            .unwrap()
            .push(Ev::Launch(kind, attempt.mtp));
        let (delay, hang, ignore_cancel, stop_delay, outcome) = {
            let mut b = self.behavior.lock().unwrap();
            let slot = usize::from(kind == IMAGE);
            let outcome = if b.not_installed {
                Outcome::NotInstalled
            } else if b.invalid_settings {
                Outcome::InvalidSettings
            } else if b.failures[slot] > 0 {
                b.failures[slot] -= 1;
                Outcome::Fail
            } else if b.fail_with_mtp && attempt.mtp {
                Outcome::FailMtp
            } else if b.no_room_for_mtp > 0 && attempt.mtp {
                b.no_room_for_mtp -= 1;
                Outcome::NoRoom
            } else {
                Outcome::Ready
            };
            let hang = b.hangs > 0;
            b.hangs = b.hangs.saturating_sub(1);
            (b.start_delay, hang, b.ignore_cancel, b.stop_delay, outcome)
        };
        let mut abort = AbortOnDrop {
            events: Arc::clone(&self.events),
            kind,
            armed: true,
        };
        let loading = async {
            if hang {
                std::future::pending::<()>().await;
            }
            sleep(delay).await;
        };
        let cancelled = tokio::select! {
            () = loading => false,
            () = attempt.cancel.cancelled(), if !ignore_cancel => true,
        };
        if cancelled {
            // Stopping the half-started engine takes a while.
            sleep(stop_delay).await;
            abort.armed = false;
            self.events.lock().unwrap().push(Ev::Cancelled(kind));
            return Err(anyhow::anyhow!("cancelled"));
        }
        abort.armed = false;
        match outcome {
            Outcome::Ready => {
                let id = self.next_id.fetch_add(1, Ordering::SeqCst);
                let crashed = Arc::new(AtomicBool::new(false));
                self.crash_flags
                    .lock()
                    .unwrap()
                    .push((kind, Arc::clone(&crashed)));
                self.events.lock().unwrap().push(Ev::Ready(kind, id));
                Ok(Box::new(FakeEngine {
                    kind,
                    id,
                    crashed,
                    events: Arc::clone(&self.events),
                }))
            }
            Outcome::Fail => Err(StartError {
                message: "fake start failure".into(),
                log: (1..=50).map(|i| format!("line {i}")).collect(),
            }
            .into()),
            Outcome::FailMtp => Err(StartError {
                message: "llama-server exited before it was ready: unknown argument --spec-type"
                    .into(),
                log: vec!["error: unknown argument --spec-type".into()],
            }
            .into()),
            Outcome::NoRoom => Err(NoRoomForMtp(
                "13000 MiB of VRAM free; the LLM with MTP needs about 13138 MiB".into(),
            )
            .into()),
            Outcome::NotInstalled => Err(NotInstalled("fake model missing".into()).into()),
            Outcome::InvalidSettings => {
                Err(InvalidSettings("llm.extraArgs may not contain --host".into()).into())
            }
        }
    }
}

struct FakeEngine {
    kind: EngineKind,
    id: u64,
    crashed: Arc<AtomicBool>,
    events: Arc<Mutex<Vec<Ev>>>,
}

#[async_trait::async_trait]
impl RunningEngine for FakeEngine {
    fn base_url(&self) -> String {
        format!("http://fake-{}-{}", self.kind, self.id)
    }

    fn api_key(&self) -> Option<String> {
        (self.kind == LLM).then(|| format!("key-{}", self.id))
    }

    fn exited(&mut self) -> bool {
        self.crashed.load(Ordering::SeqCst)
    }

    fn log_tail(&self, n: usize) -> Vec<String> {
        let lines: Vec<String> = (1..=3)
            .map(|i| format!("{} engine {} line {i}", self.kind, self.id))
            .collect();
        lines[lines.len().saturating_sub(n)..].to_vec()
    }

    async fn stop(self: Box<Self>) {
        self.events
            .lock()
            .unwrap()
            .push(Ev::Stop(self.kind, self.id));
    }
}

/// The defaults without idle unload.
fn config() -> SchedulerConfig {
    SchedulerConfig {
        idle_unload: None,
        ..SchedulerConfig::default()
    }
}

fn scheduler(cfg: SchedulerConfig) -> (Arc<FakeLauncher>, Arc<Scheduler>) {
    let fake = Arc::new(FakeLauncher::default());
    let sched = Scheduler::new(Arc::clone(&fake) as Arc<dyn Launcher>, cfg);
    (fake, sched)
}

/// Waits (in paused time) until the status matches.
async fn until(
    sched: &Scheduler,
    what: &str,
    pred: impl FnMut(&SchedulerStatus) -> bool,
) -> SchedulerStatus {
    let mut rx = sched.subscribe();
    let result = timeout(Duration::from_secs(3600), rx.wait_for(pred)).await;
    match result {
        Ok(Ok(status)) => status.clone(),
        _ => panic!("no {what}; status {:?}", sched.status()),
    }
}

/// Grants in order: kind, request number, time since the test began.
type Grants = Arc<Mutex<Vec<(EngineKind, usize, Duration)>>>;

/// A client that acquires, records the grant, holds the lease, then drops it.
fn request(
    sched: &Arc<Scheduler>,
    kind: EngineKind,
    hold: Duration,
    grants: &Grants,
    n: usize,
    t0: Instant,
) -> JoinHandle<Result<(), EngineError>> {
    let sched = Arc::clone(sched);
    let grants = Arc::clone(grants);
    tokio::spawn(async move {
        let lease = sched.acquire(kind).await?;
        grants.lock().unwrap().push((kind, n, t0.elapsed()));
        sleep(hold).await;
        drop(lease);
        Ok(())
    })
}

fn kinds(grants: &Grants) -> Vec<EngineKind> {
    grants.lock().unwrap().iter().map(|g| g.0).collect()
}

async fn join_all(tasks: &mut Vec<JoinHandle<Result<(), EngineError>>>) {
    for task in tasks.drain(..) {
        task.await.expect("client task").expect("lease");
    }
}

#[tokio::test(start_paused = true)]
async fn grants_up_to_max_per_kind() {
    let (fake, sched) = scheduler(SchedulerConfig {
        max_llm: 2,
        ..config()
    });
    let a = sched.acquire(LLM).await.unwrap();
    let b = sched.acquire(LLM).await.unwrap();
    assert_eq!(a.kind, LLM);
    assert_eq!(a.base_url, "http://fake-llm-0");
    assert_eq!(b.base_url, a.base_url);
    assert_eq!(a.api_key.as_deref(), Some("key-0"));
    let s = Arc::clone(&sched);
    let third = tokio::spawn(async move { s.acquire(LLM).await });
    let status = until(&sched, "third waiter", |s| s.waiting_llm == 1).await;
    assert_eq!(status.active, 2);
    assert_eq!(
        (status.loaded, status.state),
        (Some(LLM), EngineState::Ready)
    );
    drop(a);
    let c = third.await.unwrap().unwrap();
    let status = until(&sched, "third grant", |s| {
        s.waiting_llm == 0 && s.active == 2
    })
    .await;
    assert_eq!(status.swaps, 0);
    drop((b, c));
    until(&sched, "releases", |s| s.active == 0).await;
    assert_eq!(fake.launches(), [(LLM, true)]);
}

#[tokio::test(start_paused = true)]
async fn image_engine_grants_one_at_a_time_by_default() {
    let (_fake, sched) = scheduler(config());
    let first = sched.acquire(IMAGE).await.unwrap();
    assert_eq!(first.api_key, None);
    assert!(timeout(Duration::from_secs(5), sched.acquire(IMAGE))
        .await
        .is_err());
    drop(first);
    timeout(Duration::from_secs(1), sched.acquire(IMAGE))
        .await
        .expect("granted after the release")
        .unwrap();
}

/// The encoder sends a generation's renders at once, and each inspection
/// follows its render: one swap to the LLM, and one back for the next
/// generation's renders.
#[tokio::test(start_paused = true)]
async fn a_generation_costs_two_swaps() {
    let (fake, sched) = scheduler(config());
    let grants = Grants::default();
    let t0 = Instant::now();
    let render = Duration::from_secs(4);
    let inspect = Duration::from_secs(2);

    let mut tasks: Vec<_> = (0..8)
        .map(|n| request(&sched, IMAGE, render, &grants, n, t0))
        .collect();
    until(&sched, "first render", |s| {
        s.state == EngineState::Ready && s.active == 1
    })
    .await;
    tasks.extend((0..8).map(|n| request(&sched, LLM, inspect, &grants, n, t0)));
    let status = until(&sched, "inspections queued", |s| s.waiting_llm == 8).await;
    assert_eq!(status.waiting_image, 7);
    join_all(&mut tasks).await;
    assert_eq!(sched.status().swaps, 1);
    assert_eq!(kinds(&grants), [vec![IMAGE; 8], vec![LLM; 8]].concat());
    assert_eq!(fake.launches(), [(IMAGE, false), (LLM, true)]);

    // Generation 2: the renders arrive while the LLM is loaded.
    tasks.extend((0..8).map(|n| request(&sched, IMAGE, render, &grants, n, t0)));
    let status = until(&sched, "second generation's renders", |s| {
        s.loaded == Some(IMAGE) && s.state == EngineState::Ready && s.active == 1
    })
    .await;
    assert_eq!(status.swaps, 2);
    tasks.extend((0..8).map(|n| request(&sched, LLM, inspect, &grants, n, t0)));
    join_all(&mut tasks).await;
    assert_eq!(sched.status().swaps, 3);
    assert_eq!(
        kinds(&grants),
        [vec![IMAGE; 8], vec![LLM; 8], vec![IMAGE; 8], vec![LLM; 8]].concat()
    );
    assert_eq!(
        fake.launches(),
        [(IMAGE, false), (LLM, true), (IMAGE, false), (LLM, true)]
    );
    let stops: Vec<Ev> = fake
        .events()
        .into_iter()
        .filter(|e| matches!(e, Ev::Stop(..)))
        .collect();
    assert_eq!(
        stops,
        [Ev::Stop(IMAGE, 0), Ev::Stop(LLM, 1), Ev::Stop(IMAGE, 2)]
    );
}

/// Once the last queued render is granted with inspections waiting, the swap
/// is decided: a render that arrives a moment later waits for the next turn.
#[tokio::test(start_paused = true)]
async fn the_swap_is_marked_when_the_last_waiter_is_granted() {
    let (_fake, sched) = scheduler(config());
    let grants = Grants::default();
    let t0 = Instant::now();
    let hold = Duration::from_secs(3);
    let first = sched.acquire(IMAGE).await.unwrap();
    let mut tasks = vec![request(&sched, IMAGE, hold, &grants, 1, t0)];
    until(&sched, "a queued render", |s| s.waiting_image == 1).await;
    tasks.push(request(&sched, LLM, hold, &grants, 0, t0));
    until(&sched, "a queued inspection", |s| s.waiting_llm == 1).await;
    drop(first);
    until(&sched, "the second render", |s| {
        s.waiting_image == 0 && s.active == 1
    })
    .await;
    tasks.push(request(&sched, IMAGE, hold, &grants, 2, t0));
    join_all(&mut tasks).await;
    let order: Vec<(EngineKind, usize)> =
        grants.lock().unwrap().iter().map(|g| (g.0, g.1)).collect();
    assert_eq!(order, [(IMAGE, 1), (LLM, 0), (IMAGE, 2)]);
    assert_eq!(sched.status().swaps, 2);
}

#[tokio::test(start_paused = true)]
async fn a_starving_waiter_forces_a_swap() {
    let (fake, sched) = scheduler(config());
    let grants = Grants::default();
    let t0 = Instant::now();
    let mut tasks: Vec<_> = (0..30)
        .map(|n| request(&sched, IMAGE, Duration::from_secs(10), &grants, n, t0))
        .collect();
    until(&sched, "renders", |s| {
        s.state == EngineState::Ready && s.active == 1
    })
    .await;
    sleep(Duration::from_secs(5)).await;
    let asked = t0.elapsed();
    tasks.push(request(&sched, LLM, Duration::from_secs(1), &grants, 0, t0));
    join_all(&mut tasks).await;

    let grants = grants.lock().unwrap().clone();
    let pos = grants.iter().position(|g| g.0 == LLM).expect("llm grant");
    let waited = grants[pos].2 - asked;
    // Swap pending at 120 s; the render in progress then finishes.
    assert!(
        waited >= Duration::from_secs(120) && waited <= Duration::from_secs(131),
        "waited {waited:?}"
    );
    assert!(pos < 29, "renders were still queued at the swap");
    assert!(grants[pos + 1..].iter().all(|g| g.0 == IMAGE));
    assert_eq!(sched.status().swaps, 2);
    assert_eq!(
        fake.launches(),
        [(IMAGE, false), (LLM, true), (IMAGE, false)]
    );
}

#[tokio::test(start_paused = true)]
async fn short_vram_starts_the_llm_without_mtp_once_and_asks_again_next_time() {
    let (fake, sched) = scheduler(config());
    fake.set(|b| b.no_room_for_mtp = 1);
    let lease = sched.acquire(LLM).await.expect("starts without MTP");
    let status = sched.status();
    assert!(!status.mtp, "the running LLM has no MTP");
    assert_eq!(status.state, EngineState::Ready);
    assert_eq!(status.last_error, None, "short VRAM is not an error");
    assert_eq!(fake.launches(), [(LLM, true), (LLM, false)]);
    drop(lease);
    sched.unload().await;
    let _lease = sched.acquire(LLM).await.unwrap();
    assert_eq!(
        fake.launches(),
        [(LLM, true), (LLM, false), (LLM, true)],
        "MTP is tried again"
    );
    assert!(sched.status().mtp);
}

#[tokio::test(start_paused = true)]
async fn llm_falls_back_to_no_mtp_for_the_session() {
    let (fake, sched) = scheduler(config());
    fake.set(|b| b.fail_with_mtp = true);
    let lease = sched.acquire(LLM).await.expect("starts without MTP");
    let status = sched.status();
    assert!(!status.mtp);
    assert_eq!(status.state, EngineState::Ready);
    assert_eq!(status.last_error, None);
    assert_eq!(fake.launches(), [(LLM, true), (LLM, false)]);
    drop(lease);
    sched.unload().await;
    let _lease = sched.acquire(LLM).await.unwrap();
    assert_eq!(fake.launches(), [(LLM, true), (LLM, false), (LLM, false)]);
}

#[tokio::test(start_paused = true)]
async fn llm_retry_without_mtp_can_fail_too() {
    let (fake, sched) = scheduler(config());
    fake.set(|b| b.failures = [2, 0]);
    match sched.acquire(LLM).await {
        Err(EngineError::StartFailed { kind, message, .. }) => {
            assert_eq!(kind, LLM);
            assert_eq!(message, "fake start failure");
        }
        other => panic!("{other:?}"),
    }
    assert!(!sched.status().mtp);
    assert_eq!(fake.launches(), [(LLM, true), (LLM, false)]);
}

#[tokio::test(start_paused = true)]
async fn failed_start_fails_waiters_and_the_next_acquire_retries() {
    let (fake, sched) = scheduler(config());
    fake.set(|b| {
        b.failures = [0, 1];
        b.start_delay = Duration::from_secs(1);
    });
    let s1 = Arc::clone(&sched);
    let s2 = Arc::clone(&sched);
    let a = tokio::spawn(async move { s1.acquire(IMAGE).await });
    let b = tokio::spawn(async move { s2.acquire(IMAGE).await });
    for result in [a.await.unwrap(), b.await.unwrap()] {
        match result {
            Err(EngineError::StartFailed { kind, message, log }) => {
                assert_eq!(kind, IMAGE);
                assert_eq!(message, "fake start failure");
                assert_eq!(log.len(), 40);
                assert_eq!((log[0].as_str(), log[39].as_str()), ("line 11", "line 50"));
            }
            other => panic!("{other:?}"),
        }
    }
    let status = sched.status();
    assert_eq!(
        (status.loaded, status.state),
        (Some(IMAGE), EngineState::Failed)
    );
    assert_eq!(
        status.last_error.as_deref(),
        Some("the image engine failed to start: fake start failure")
    );
    assert_eq!(sched.logs(IMAGE, 2), ["line 49", "line 50"]);

    let _lease = sched
        .acquire(IMAGE)
        .await
        .expect("the next acquire retries");
    let status = sched.status();
    assert_eq!(status.state, EngineState::Ready);
    assert_eq!(status.last_error, None);
    assert_eq!(fake.launches(), [(IMAGE, false), (IMAGE, false)]);
}

#[tokio::test(start_paused = true)]
async fn missing_files_are_not_installed_and_keep_mtp() {
    let (fake, sched) = scheduler(config());
    fake.set(|b| b.not_installed = true);
    match sched.acquire(LLM).await {
        Err(EngineError::NotInstalled(what)) => assert_eq!(what, "fake model missing"),
        other => panic!("{other:?}"),
    }
    let status = sched.status();
    assert!(status.mtp, "no MTP retry for a missing file");
    assert_eq!(status.state, EngineState::Failed);
    assert_eq!(fake.launches(), [(LLM, true)]);
}

/// At the start timeout the launch is cancelled and its engine stopped
/// before the waiters hear of the failure.
#[tokio::test(start_paused = true)]
async fn a_start_that_hangs_times_out() {
    let (fake, sched) = scheduler(SchedulerConfig {
        start_timeout: Duration::from_secs(30),
        ..config()
    });
    fake.set(|b| {
        b.hangs = usize::MAX;
        b.stop_delay = Duration::from_secs(2);
    });
    let t = Instant::now();
    match sched.acquire(IMAGE).await {
        Err(EngineError::StartFailed { message, log, .. }) => {
            assert_eq!(message, "not ready after 30 s");
            assert!(log.is_empty());
        }
        other => panic!("{other:?}"),
    }
    let took = t.elapsed();
    assert!(
        took >= Duration::from_secs(32) && took < Duration::from_secs(33),
        "{took:?}"
    );
    assert_eq!(
        fake.events(),
        [Ev::Launch(IMAGE, false), Ev::Cancelled(IMAGE)]
    );
}

/// The MTP retry after a timed-out LLM start begins only once the first
/// engine has stopped, so two never hold the GPU at once.
#[tokio::test(start_paused = true)]
async fn a_timed_out_start_is_stopped_before_the_retry() {
    let (fake, sched) = scheduler(SchedulerConfig {
        start_timeout: Duration::from_secs(30),
        ..config()
    });
    fake.set(|b| {
        b.hangs = 1;
        b.stop_delay = Duration::from_secs(3);
    });
    let lease = sched.acquire(LLM).await.expect("the retry starts");
    assert_eq!(lease.base_url, "http://fake-llm-0");
    assert_eq!(
        fake.events(),
        [
            Ev::Launch(LLM, true),
            Ev::Cancelled(LLM),
            Ev::Launch(LLM, false),
            Ev::Ready(LLM, 0)
        ]
    );
    assert!(!sched.status().mtp);
}

/// A launcher that ignores the cancel is dropped after the grace period.
#[tokio::test(start_paused = true)]
async fn a_start_that_ignores_its_cancel_is_dropped_after_the_grace() {
    let (fake, sched) = scheduler(SchedulerConfig {
        start_timeout: Duration::from_secs(30),
        ..config()
    });
    fake.set(|b| {
        b.hangs = usize::MAX;
        b.ignore_cancel = true;
    });
    let t = Instant::now();
    assert!(matches!(
        sched.acquire(IMAGE).await,
        Err(EngineError::StartFailed { .. })
    ));
    assert!(t.elapsed() >= Duration::from_secs(30) + actor::ABANDON_GRACE);
    assert_eq!(fake.events(), [Ev::Launch(IMAGE, false), Ev::Abort(IMAGE)]);
}

/// Settings that cannot work fail the start without the MTP retry, which
/// would fail the same way and lose MTP for the session.
#[tokio::test(start_paused = true)]
async fn invalid_settings_fail_without_an_mtp_retry() {
    let (fake, sched) = scheduler(config());
    fake.set(|b| b.invalid_settings = true);
    match sched.acquire(LLM).await {
        Err(EngineError::StartFailed { kind, message, .. }) => {
            assert_eq!(kind, LLM);
            assert!(message.contains("--host"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    assert!(sched.status().mtp, "MTP stays on");
    assert_eq!(fake.launches(), [(LLM, true)]);
}

#[tokio::test(start_paused = true)]
async fn crash_then_restart_on_next_acquire() {
    let (fake, sched) = scheduler(config());
    let old = sched.acquire(LLM).await.unwrap();
    fake.crash_latest(LLM);
    let t = Instant::now();
    let status = until(&sched, "crash", |s| s.state == EngineState::Failed).await;
    assert!(t.elapsed() <= Duration::from_secs(1), "polled every second");
    assert_eq!((status.loaded, status.active), (Some(LLM), 0));
    assert_eq!(
        status.last_error.as_deref(),
        Some("the llm engine exited unexpectedly: llm engine 0 line 3")
    );

    let new = sched.acquire(LLM).await.unwrap();
    assert_eq!(new.base_url, "http://fake-llm-1");
    assert_eq!(
        fake.events(),
        [
            Ev::Launch(LLM, true),
            Ev::Ready(LLM, 0),
            Ev::Stop(LLM, 0),
            Ev::Launch(LLM, true),
            Ev::Ready(LLM, 1)
        ]
    );
    assert_eq!(sched.status().last_error, None);
    // A lease on the dead engine does not free a slot on the new one.
    drop(old);
    let s = Arc::clone(&sched);
    let waiter = tokio::spawn(async move { s.acquire(LLM).await });
    let status = until(&sched, "a waiter", |s| s.waiting_llm == 1).await;
    assert_eq!(status.active, 1);
    drop(new);
    waiter.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_dead_engine_is_not_handed_out() {
    let (fake, sched) = scheduler(config());
    drop(sched.acquire(IMAGE).await.unwrap());
    fake.crash_latest(IMAGE);
    // The acquire arrives before the next crash poll.
    let lease = sched.acquire(IMAGE).await.unwrap();
    assert_eq!(lease.base_url, "http://fake-image-1");
}

#[tokio::test(start_paused = true)]
async fn idle_engine_unloads() {
    let (fake, sched) = scheduler(SchedulerConfig {
        idle_unload: Some(Duration::from_secs(60)),
        ..config()
    });
    let lease = sched.acquire(IMAGE).await.unwrap();
    sleep(Duration::from_secs(300)).await;
    assert_eq!(sched.status().state, EngineState::Ready, "busy, not idle");
    drop(lease);
    let t = Instant::now();
    let status = until(&sched, "idle unload", |s| s.state == EngineState::Stopped).await;
    let idle = t.elapsed();
    assert!(
        idle >= Duration::from_secs(60) && idle < Duration::from_secs(62),
        "{idle:?}"
    );
    assert_eq!(status.loaded, None);
    assert_eq!(fake.events().last(), Some(&Ev::Stop(IMAGE, 0)));
    sched.acquire(IMAGE).await.expect("starts again");
    assert_eq!(fake.launches(), [(IMAGE, false), (IMAGE, false)]);
}

#[tokio::test(start_paused = true)]
async fn the_idle_unload_time_changes_without_a_restart() {
    let (fake, sched) = scheduler(config());
    drop(sched.acquire(IMAGE).await.unwrap());
    sleep(Duration::from_secs(600)).await;
    assert_eq!(sched.status().state, EngineState::Ready, "no idle unload");
    // Idle for 10 minutes already, so 5 minutes stops it at once.
    sched.set_idle_unload(Some(Duration::from_secs(300)));
    let t = Instant::now();
    until(&sched, "idle unload", |s| s.state == EngineState::Stopped).await;
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    // A longer time counts from when the engine became idle.
    sched.set_idle_unload(Some(Duration::from_secs(120)));
    drop(sched.acquire(IMAGE).await.unwrap());
    let t = Instant::now();
    until(&sched, "idle unload", |s| s.state == EngineState::Stopped).await;
    let idle = t.elapsed();
    assert!(
        idle >= Duration::from_secs(120) && idle < Duration::from_secs(122),
        "{idle:?}"
    );
    // Off again: it stays loaded.
    sched.set_idle_unload(None);
    drop(sched.acquire(IMAGE).await.unwrap());
    sleep(Duration::from_secs(3600)).await;
    assert_eq!(sched.status().state, EngineState::Ready);
    assert_eq!(fake.launches().len(), 3);
}

#[tokio::test(start_paused = true)]
async fn dropping_a_waiting_acquire_leaves_the_queue() {
    let (fake, sched) = scheduler(config());
    let lease = sched.acquire(IMAGE).await.unwrap();
    assert!(timeout(Duration::from_secs(5), sched.acquire(IMAGE))
        .await
        .is_err());
    let status = until(&sched, "the waiter to leave", |s| s.waiting_image == 0).await;
    assert_eq!(status.active, 1);
    // An LLM waiter marks a swap; when it leaves, the swap is called off.
    assert!(timeout(Duration::from_secs(5), sched.acquire(LLM))
        .await
        .is_err());
    until(&sched, "the llm waiter to leave", |s| s.waiting_llm == 0).await;
    drop(lease);
    until(&sched, "the release", |s| s.active == 0).await;
    let again = timeout(Duration::from_secs(1), sched.acquire(IMAGE))
        .await
        .expect("granted at once")
        .unwrap();
    assert_eq!(sched.status().swaps, 0);
    assert_eq!(fake.launches(), [(IMAGE, false)]);
    drop(again);
}

#[tokio::test(start_paused = true)]
async fn unload_waits_for_leases_and_waiters_restart_it() {
    let (fake, sched) = scheduler(config());
    timeout(Duration::from_millis(1), sched.unload())
        .await
        .expect("nothing loaded: at once");
    let lease = sched.acquire(LLM).await.unwrap();
    let s = Arc::clone(&sched);
    let unload = tokio::spawn(async move { s.unload().await });
    sleep(Duration::from_secs(10)).await;
    assert!(!unload.is_finished(), "waits for the lease");
    assert_eq!(sched.status().state, EngineState::Ready);
    let s = Arc::clone(&sched);
    let waiter = tokio::spawn(async move { s.acquire(LLM).await });
    let status = until(&sched, "a waiter", |s| s.waiting_llm == 1).await;
    assert_eq!(status.active, 1, "no grants while unloading");
    drop(lease);
    unload.await.unwrap();
    let next = waiter.await.unwrap().unwrap();
    assert_eq!(next.base_url, "http://fake-llm-1");
    assert_eq!(
        fake.events(),
        [
            Ev::Launch(LLM, true),
            Ev::Ready(LLM, 0),
            Ev::Stop(LLM, 0),
            Ev::Launch(LLM, true),
            Ev::Ready(LLM, 1)
        ]
    );
    assert_eq!(sched.status().swaps, 0);
}

#[tokio::test(start_paused = true)]
async fn unload_during_a_start_stops_the_engine_once_ready() {
    let (fake, sched) = scheduler(config());
    fake.set(|b| b.start_delay = Duration::from_secs(10));
    let s = Arc::clone(&sched);
    let waiter = tokio::spawn(async move { s.acquire(IMAGE).await });
    until(&sched, "the start", |s| s.state == EngineState::Starting).await;
    sched.unload().await;
    let lease = waiter.await.unwrap().unwrap();
    assert_eq!(
        lease.base_url, "http://fake-image-1",
        "the waiter restarted it"
    );
    assert_eq!(
        fake.events(),
        [
            Ev::Launch(IMAGE, false),
            Ev::Ready(IMAGE, 0),
            Ev::Stop(IMAGE, 0),
            Ev::Launch(IMAGE, false),
            Ev::Ready(IMAGE, 1)
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn shutdown_rejects_waiters_and_stops_the_engine() {
    let (fake, sched) = scheduler(config());
    let lease = sched.acquire(IMAGE).await.unwrap();
    let s1 = Arc::clone(&sched);
    let s2 = Arc::clone(&sched);
    let image = tokio::spawn(async move { s1.acquire(IMAGE).await });
    let llm = tokio::spawn(async move { s2.acquire(LLM).await });
    until(&sched, "two waiters", |s| {
        s.waiting_image == 1 && s.waiting_llm == 1
    })
    .await;
    sched.shutdown().await;
    assert!(matches!(
        image.await.unwrap(),
        Err(EngineError::ShuttingDown)
    ));
    assert!(matches!(llm.await.unwrap(), Err(EngineError::ShuttingDown)));
    assert_eq!(fake.events().last(), Some(&Ev::Stop(IMAGE, 0)));
    let status = sched.status();
    assert_eq!(
        (
            status.loaded,
            status.state,
            status.active,
            status.waiting_image,
            status.waiting_llm
        ),
        (None, EngineState::Stopped, 0, 0, 0)
    );
    assert!(matches!(
        sched.acquire(LLM).await,
        Err(EngineError::ShuttingDown)
    ));
    drop(lease);
    sched.unload().await;
    sched.shutdown().await;
    assert_eq!(fake.launches(), [(IMAGE, false)]);
}

/// Shutdown during a start cancels it and returns only once the launcher
/// has stopped its engine.
#[tokio::test(start_paused = true)]
async fn shutdown_cancels_a_start_and_waits_for_it() {
    let (fake, sched) = scheduler(config());
    fake.set(|b| {
        b.start_delay = Duration::from_secs(60);
        b.stop_delay = Duration::from_secs(3);
    });
    let s = Arc::clone(&sched);
    let waiter = tokio::spawn(async move { s.acquire(LLM).await });
    until(&sched, "the start", |s| s.state == EngineState::Starting).await;
    let t = Instant::now();
    let s = Arc::clone(&sched);
    let shutdown = tokio::spawn(async move { s.shutdown().await });
    let status = until(&sched, "stopping the start", |s| {
        s.state == EngineState::Stopping
    })
    .await;
    assert_eq!(status.loaded, Some(LLM));
    shutdown.await.unwrap();
    assert!(t.elapsed() >= Duration::from_secs(3), "waited for the stop");
    assert!(matches!(
        waiter.await.unwrap(),
        Err(EngineError::ShuttingDown)
    ));
    assert_eq!(fake.events(), [Ev::Launch(LLM, true), Ev::Cancelled(LLM)]);
    let status = sched.status();
    assert_eq!(
        (status.state, status.last_error),
        (EngineState::Stopped, None)
    );
}

/// A start that ignores its cancel holds up the shutdown for the grace
/// period at most; then it is dropped.
#[tokio::test(start_paused = true)]
async fn shutdown_drops_a_start_that_ignores_its_cancel() {
    let (fake, sched) = scheduler(config());
    fake.set(|b| {
        b.start_delay = Duration::from_secs(600);
        b.ignore_cancel = true;
    });
    let s = Arc::clone(&sched);
    let _waiter = tokio::spawn(async move { s.acquire(IMAGE).await });
    until(&sched, "the start", |s| s.state == EngineState::Starting).await;
    let t = Instant::now();
    sched.shutdown().await;
    let took = t.elapsed();
    assert!(
        took >= actor::ABANDON_GRACE && took < actor::ABANDON_GRACE + Duration::from_secs(1),
        "{took:?}"
    );
    assert_eq!(fake.events(), [Ev::Launch(IMAGE, false), Ev::Abort(IMAGE)]);
    assert_eq!(sched.status().state, EngineState::Stopped);
}

/// A start that finishes just as the shutdown cancels it is stopped too.
#[tokio::test(start_paused = true)]
async fn shutdown_stops_an_engine_that_became_ready_meanwhile() {
    let (fake, sched) = scheduler(config());
    fake.set(|b| {
        b.start_delay = Duration::from_secs(10);
        b.ignore_cancel = true;
    });
    let s = Arc::clone(&sched);
    let _waiter = tokio::spawn(async move { s.acquire(IMAGE).await });
    until(&sched, "the start", |s| s.state == EngineState::Starting).await;
    sched.shutdown().await;
    assert_eq!(
        fake.events(),
        [
            Ev::Launch(IMAGE, false),
            Ev::Ready(IMAGE, 0),
            Ev::Stop(IMAGE, 0)
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn dropping_the_scheduler_stops_the_engine() {
    let (fake, sched) = scheduler(config());
    drop(sched.acquire(IMAGE).await.unwrap());
    let mut rx = sched.subscribe();
    drop(sched);
    while rx.changed().await.is_ok() {}
    assert_eq!(fake.events().last(), Some(&Ev::Stop(IMAGE, 0)));
}

#[tokio::test(start_paused = true)]
async fn logs_are_live_while_loaded_and_kept_afterwards() {
    let (_fake, sched) = scheduler(config());
    assert!(sched.logs(LLM, 10).is_empty());
    let lease = sched.acquire(LLM).await.unwrap();
    assert_eq!(
        sched.logs(LLM, 2),
        ["llm engine 0 line 2", "llm engine 0 line 3"]
    );
    assert!(sched.logs(IMAGE, 10).is_empty());
    let debug = format!("{lease:?}");
    assert!(
        debug.contains("fake-llm-0") && !debug.contains("key-0"),
        "{debug}"
    );
    drop(lease);
    sched.unload().await;
    assert_eq!(sched.logs(LLM, 10).len(), 3);
}

#[test]
fn config_from_settings() {
    let mut cfg = Config::default();
    let s = SchedulerConfig::from_config(&cfg);
    assert_eq!((s.max_llm, s.max_image), (1, 1));
    assert_eq!(s.max_batch_wait, Duration::from_secs(120));
    assert_eq!(s.idle_unload, Some(Duration::from_secs(15 * 60)));
    assert_eq!(s.start_timeout, Duration::from_secs(240));
    assert!(s.mtp);
    cfg.llm.parallel = 3;
    cfg.idle_unload_minutes = 0;
    cfg.llm.mtp = false;
    let s = SchedulerConfig::from_config(&cfg);
    assert_eq!((s.max_llm, s.idle_unload, s.mtp), (3, None, false));
}

#[test]
fn status_is_camel_case_json() {
    let status = SchedulerStatus {
        loaded: Some(LLM),
        state: EngineState::Ready,
        active: 1,
        waiting_llm: 2,
        waiting_image: 0,
        mtp: true,
        last_error: None,
        swaps: 3,
        since_ms: 5,
    };
    assert_eq!(
        serde_json::to_value(status).unwrap(),
        serde_json::json!({"loaded": "llm", "state": "ready", "active": 1, "waitingLlm": 2,
            "waitingImage": 0, "mtp": true, "lastError": null, "swaps": 3, "sinceMs": 5})
    );
}
