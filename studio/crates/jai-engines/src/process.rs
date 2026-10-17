//! Supervision of one engine process (DESIGN.md section 6, "Process
//! supervision").

use std::net::SocketAddrV4;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tokio::time::{timeout, Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::launch::{command_line, LaunchSpec};
use crate::launcher::{RunningEngine, StartError};

mod output;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;
#[cfg(unix)]
use unix as sys;
#[cfg(windows)]
use windows as sys;

#[cfg(test)]
mod tests;

use output::LogSink;

/// How often `wait_ready` asks the health path.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Longest single health request.
const HEALTH_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
/// After the process ends, how long to wait for its last output.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// A started engine: the child, its Job object on Windows, a 1000-line log
/// ring, its address and API key.
pub struct EngineProcess {
    /// The program's file name, for messages.
    name: String,
    child: Child,
    /// Set once the exit has been observed.
    status: Option<ExitStatus>,
    addr: SocketAddrV4,
    api_key: Option<String>,
    health_path: &'static str,
    log: Arc<LogSink>,
    /// The stdout and stderr readers.
    readers: Vec<JoinHandle<()>>,
    /// Closing it kills whatever is still in the job.
    #[cfg(windows)]
    _job: Option<windows::Job>,
}

impl EngineProcess {
    /// Spawns the engine with piped output and no console window. On Windows
    /// it joins a kill-on-close Job object; on Linux it gets its own process
    /// group and dies with the thread that started it (`PR_SET_PDEATHSIG`).
    /// Must run inside a Tokio runtime.
    pub async fn start(spec: LaunchSpec) -> anyhow::Result<EngineProcess> {
        let name = program_name(&spec.program);
        let log = Arc::new(LogSink::open(spec.kind, &spec.log_file));
        log.note(&format!("starting {}", command_line(&spec)));
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args)
            .current_dir(&spec.cwd)
            .envs(spec.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        sys::configure(&mut cmd);
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) => {
                log.note(&format!("could not start: {err}"));
                return Err(err)
                    .with_context(|| format!("could not start {}", spec.program.display()));
            }
        };
        #[cfg(windows)]
        let job = windows::contain(&child, &name);
        let mut readers = Vec::with_capacity(2);
        if let Some(stdout) = child.stdout.take() {
            readers.push(tokio::spawn(output::pump(stdout, Arc::clone(&log))));
        }
        if let Some(stderr) = child.stderr.take() {
            readers.push(tokio::spawn(output::pump(stderr, Arc::clone(&log))));
        }
        info!(engine = %spec.kind, pid = ?child.id(), addr = %spec.addr, "started {name}");
        Ok(EngineProcess {
            name,
            child,
            status: None,
            addr: spec.addr,
            api_key: spec.api_key,
            health_path: spec.health_path,
            log,
            readers,
            #[cfg(windows)]
            _job: job,
        })
    }

    /// Polls the health path every 250 ms until it answers 200, then checks
    /// that the answer came from this engine: that the child itself listens
    /// on its address. Fails early, with the log tail, if the child exits or
    /// another program answered; fails with the log tail after `timeout`.
    /// Every failure is a [`StartError`]. Redirects are not followed.
    pub async fn wait_ready(&mut self, timeout: Duration) -> anyhow::Result<()> {
        let url = format!("{}{}", self.base_url(), self.health_path);
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(HEALTH_REQUEST_TIMEOUT)
            .build()
            .context("could not build the health-check client")?;
        // None: a timeout too large to represent, so no deadline.
        let deadline = Instant::now().checked_add(timeout);
        let mut poll = tokio::time::interval(POLL_INTERVAL);
        poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            poll.tick().await;
            if self.exited() {
                self.drain_output().await;
                let how = self
                    .status
                    .map(|s| format!(" ({})", exit_text(&s)))
                    .unwrap_or_default();
                return Err(
                    self.start_error(format!("{} exited before it was ready{how}", self.name))
                );
            }
            if deadline.is_some_and(|at| Instant::now() >= at) {
                return Err(self.start_error(format!(
                    "{} did not become ready within {}",
                    self.name,
                    seconds(timeout)
                )));
            }
            match client.get(&url).send().await {
                Ok(response) if response.status() == reqwest::StatusCode::OK => {
                    return self.check_listener()
                }
                Ok(response) => debug!(status = %response.status(), "{} is loading", self.name),
                Err(err) => debug!("{} is not answering yet: {err}", self.name),
            }
        }
    }

    /// The address is chosen before the engine starts, and sd-server only
    /// binds it once its model has loaded, so another account's program can
    /// take it first and answer the health check. Requests and the API key
    /// must never go to such a program: the child itself must own the
    /// listening socket.
    fn check_listener(&mut self) -> anyhow::Result<()> {
        let owned = match self.child.id() {
            Some(pid) => sys::listens_on(pid, self.addr),
            None => Ok(false),
        };
        match owned {
            Ok(true) => Ok(()),
            Ok(false) => Err(self.start_error(format!(
                "another program answered on {} instead of {}, so JAI Studio did not use it",
                self.addr, self.name
            ))),
            Err(err) => Err(self.start_error(format!(
                "cannot check that {} itself answered on {}: {err}",
                self.name, self.addr
            ))),
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn api_key(&self) -> Option<String> {
        self.api_key.clone()
    }

    /// Whether the process has ended.
    pub fn exited(&mut self) -> bool {
        if self.status.is_some() {
            return true;
        }
        match self.child.try_wait() {
            Ok(Some(status)) => {
                self.status = Some(status);
                true
            }
            Ok(None) => false,
            Err(err) => {
                warn!("lost track of {}: {err}", self.name);
                true
            }
        }
    }

    /// The last `n` lines of output, oldest first.
    pub fn log_tail(&self, n: usize) -> Vec<String> {
        self.log.tail(n)
    }

    /// TerminateProcess on Windows; SIGTERM, then SIGKILL after 3 s on Linux.
    /// Returns once the process is gone (VRAM is free then).
    pub async fn stop(mut self) {
        if !self.exited() {
            self.status = sys::terminate(&mut self.child, &self.name).await;
        }
        self.drain_output().await;
        let status = match self.status {
            Some(status) => exit_text(&status),
            None => "it did not exit".into(),
        };
        self.log.note(&format!("stopped ({status})"));
        info!("stopped {} ({status})", self.name);
    }

    /// Waits briefly for the readers to reach the end of the output.
    async fn drain_output(&mut self) {
        let readers = std::mem::take(&mut self.readers);
        if !readers.is_empty() {
            let _ = timeout(DRAIN_TIMEOUT, futures::future::join_all(readers)).await;
        }
    }

    /// A [`StartError`] with the log tail, its message ending with the last
    /// line of output, which usually says what went wrong.
    fn start_error(&self, what: String) -> anyhow::Error {
        let log = self.log.tail(output::RING_LINES);
        let message = match log.iter().rev().map(|l| l.trim()).find(|l| !l.is_empty()) {
            Some(last) => format!("{what}: {last}"),
            None => what,
        };
        self.log.note(&message);
        StartError { message, log }.into()
    }
}

/// Starts the engine and waits until it is ready, or until `cancel` fires.
/// On any failure, the cancel included, the engine is stopped before this
/// returns, so no engine is left running and its VRAM is free.
pub(crate) async fn start_ready(
    spec: LaunchSpec,
    ready_timeout: Duration,
    cancel: &CancellationToken,
) -> anyhow::Result<EngineProcess> {
    if cancel.is_cancelled() {
        anyhow::bail!("the start of {} was cancelled", program_name(&spec.program));
    }
    let mut process = EngineProcess::start(spec).await?;
    let ready = tokio::select! {
        ready = process.wait_ready(ready_timeout) => ready,
        () = cancel.cancelled() => {
            process.log.note("start cancelled");
            Err(anyhow::anyhow!("the start of {} was cancelled", process.name))
        }
    };
    match ready {
        Ok(()) => Ok(process),
        Err(err) => {
            process.stop().await;
            Err(err)
        }
    }
}

impl Drop for EngineProcess {
    /// An engine dropped without `stop` is killed. `kill_on_drop` kills the
    /// process itself; this also reaches the rest of its group on Linux, and
    /// on Windows closing the Job object does.
    fn drop(&mut self) {
        if self.status.is_none() {
            if let Some(pid) = self.child.id() {
                debug!(pid, "killing {} on drop", self.name);
                #[cfg(unix)]
                unix::signal_group(pid, libc::SIGKILL);
            }
        }
    }
}

#[async_trait::async_trait]
impl RunningEngine for EngineProcess {
    fn base_url(&self) -> String {
        EngineProcess::base_url(self)
    }

    fn api_key(&self) -> Option<String> {
        EngineProcess::api_key(self)
    }

    fn exited(&mut self) -> bool {
        EngineProcess::exited(self)
    }

    fn log_tail(&self, n: usize) -> Vec<String> {
        EngineProcess::log_tail(self, n)
    }

    async fn stop(self: Box<Self>) {
        EngineProcess::stop(*self).await
    }
}

/// `llama-server` for `…/llama-server.exe`.
fn program_name(program: &Path) -> String {
    program
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.display().to_string())
}

/// "4 s", or "1.5 s" when there is a fraction.
fn seconds(d: Duration) -> String {
    if d.subsec_millis() == 0 {
        format!("{} s", d.as_secs())
    } else {
        format!("{:.1} s", d.as_secs_f64())
    }
}

/// How the process ended, with a hint for the Windows failures users can fix.
fn exit_text(status: &ExitStatus) -> String {
    if let Some(code) = status.code().map(|c| c as u32) {
        if let Some(hint) = ntstatus_hint(code) {
            return format!("exit code {code:#010x}: {hint}");
        }
    }
    status.to_string()
}

/// Windows NTSTATUS exit codes worth explaining. They cannot occur on Linux,
/// where exit codes are 0 to 255.
fn ntstatus_hint(code: u32) -> Option<&'static str> {
    match code {
        0xC000_0135 => Some("a DLL is missing; is the Microsoft Visual C++ runtime installed?"),
        0xC000_0139 => {
            Some("a DLL entry point is missing; the Microsoft Visual C++ runtime may be too old")
        }
        0xC000_001D => {
            Some("illegal instruction; this CPU lacks an instruction set the engine needs")
        }
        0xC000_0005 => Some("access violation"),
        0xC000_0409 => Some("the engine aborted"),
        _ => None,
    }
}
