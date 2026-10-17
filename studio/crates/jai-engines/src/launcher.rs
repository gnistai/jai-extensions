//! The launcher abstraction, so the scheduler can be tested with fake engines,
//! and the real launcher that runs llama-server and sd-server.

use std::time::Duration;

use anyhow::Context as _;
use jai_core::{Backend, Config, Paths, Selection};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::launch::{engine_addr, llama_spec, sd_spec};
use crate::process;
use crate::EngineKind;

/// Starts engines for the scheduler.
#[async_trait::async_trait]
pub trait Launcher: Send + Sync + 'static {
    /// Starts an engine and returns once it is ready to serve.
    ///
    /// When the scheduler gives up on the start (shutdown, or
    /// `SchedulerConfig::start_timeout`) it cancels `attempt.cancel`: the
    /// launcher should then stop what it started, wait until it is gone, and
    /// return. A launch that has not returned 20 s after the cancel is
    /// dropped, so dropping the future must still kill a half-started engine.
    ///
    /// Errors may wrap [`NotInstalled`], [`InvalidSettings`] or
    /// [`StartError`]; the scheduler reports those as
    /// `EngineError::NotInstalled`, and as `EngineError::StartFailed` (with
    /// the log lines of a `StartError`).
    async fn launch(
        &self,
        kind: EngineKind,
        attempt: LaunchAttempt,
    ) -> anyhow::Result<Box<dyn RunningEngine>>;
}

/// Options for one start attempt.
#[derive(Clone, Debug, Default)]
pub struct LaunchAttempt {
    /// Start the LLM with `--spec-type draft-mtp`.
    pub mtp: bool,
    /// Cancelled when the scheduler gives up on this start.
    pub cancel: CancellationToken,
}

/// An engine that is up, as the scheduler sees it.
#[async_trait::async_trait]
pub trait RunningEngine: Send + Sync {
    fn base_url(&self) -> String;
    fn api_key(&self) -> Option<String>;
    fn exited(&mut self) -> bool;
    fn log_tail(&self, n: usize) -> Vec<String>;
    async fn stop(self: Box<Self>);
}

/// A file an engine needs is missing. Returned inside `anyhow::Error`.
#[derive(Clone, Debug, thiserror::Error)]
#[error("{0}")]
pub struct NotInstalled(pub String);

/// The engine settings cannot be used, such as `extraArgs` that set the
/// engine's address. Returned inside `anyhow::Error`; starting again without
/// MTP would not help.
#[derive(Clone, Debug, thiserror::Error)]
#[error("{0}")]
pub struct InvalidSettings(pub String);

/// Not enough free VRAM to start the LLM with MTP. The scheduler starts it
/// without MTP this time and asks again at the next start, since the desktop's
/// own VRAM use changes. Returned inside `anyhow::Error`.
#[derive(Clone, Debug, thiserror::Error)]
#[error("{0}")]
pub struct NoRoomForMtp(pub String);

/// The LLM engine's VRAM in MiB, measured on an RTX 5070 Ti with Qwen3.8-27B
/// UD-IQ3_XXS, its F16 vision projector and flash attention (2026-10-17):
/// about 10.9 GB, plus the KV cache (ctx / 16 MiB at f16; q8_0 measured at
/// about two thirds of that, the MTP head's own cache staying f16), plus about
/// 0.9 GB for MTP. The margin is for the desktop, whose own use moves.
const LLM_BASE_MIB: u64 = 10_915;
const MTP_EXTRA_MIB: u64 = 870;
const VRAM_MARGIN_MIB: u64 = 500;

/// How the LLM fits in the GPU's free VRAM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LlmFit {
    /// MTP with the default f16 KV cache.
    Mtp,
    /// MTP fits only with a q8_0 KV cache.
    MtpQ8Cache,
    /// Without MTP.
    NoMtp,
    /// Not even without MTP: Windows would spill into shared system memory,
    /// which works but is much slower.
    Tight,
}

/// VRAM the LLM needs, margin included.
pub(crate) fn llm_need_mib(ctx: u32, mtp: bool, q8_cache: bool) -> u64 {
    let f16 = u64::from(ctx) / 16;
    let kv = if q8_cache { f16 * 2 / 3 } else { f16 };
    LLM_BASE_MIB + kv + if mtp { MTP_EXTRA_MIB } else { 0 } + VRAM_MARGIN_MIB
}

pub(crate) fn llm_fit(free_mib: u64, ctx: u32, want_mtp: bool) -> LlmFit {
    if want_mtp && free_mib >= llm_need_mib(ctx, true, false) {
        LlmFit::Mtp
    } else if want_mtp && free_mib >= llm_need_mib(ctx, true, true) {
        LlmFit::MtpQ8Cache
    } else if free_mib >= llm_need_mib(ctx, false, false) {
        LlmFit::NoMtp
    } else {
        LlmFit::Tight
    }
}

/// Free VRAM in MiB on the NVIDIA GPU, or None without one. Sampled up to three
/// times, 0.7 s apart, until it reaches `enough`, keeping the largest: VRAM an
/// engine frees when it stops can take a moment to show.
async fn free_vram_mib(enough: u64) -> Option<u64> {
    let mut best: Option<u64> = None;
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(700)).await;
        }
        let Ok(Some(gpu)) = tokio::task::spawn_blocking(jai_core::gpu::detect).await else {
            break;
        };
        let free = gpu.vram_total_mb.saturating_sub(gpu.vram_used_mb);
        best = Some(best.map_or(free, |b| b.max(free)));
        if free >= enough {
            break;
        }
    }
    best
}

/// The user's llama-server extraArgs already choose a KV cache type.
fn sets_cache_type(extra: &[String]) -> bool {
    extra.iter().any(|a| {
        let flag = a.split('=').next().unwrap_or(a);
        matches!(flag, "-ctk" | "-ctv" | "--cache-type-k" | "--cache-type-v")
    })
}

/// An engine that was started but did not become ready, with its last log
/// lines (up to 1000). Returned inside `anyhow::Error`.
#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub struct StartError {
    pub message: String,
    pub log: Vec<String>,
}

/// How long `ProcessLauncher` waits for readiness: a little under the
/// scheduler's default `start_timeout` (240 s), so a slow start ends with the
/// engine's log tail rather than the scheduler's bare timeout.
const READY_TIMEOUT: Duration = Duration::from_secs(235);

/// The real launcher: `launch::engine_addr`, `launch::llama_spec` or
/// `launch::sd_spec`, then `EngineProcess::start` and `wait_ready`, stopping
/// the engine if the start is cancelled. Holds the paths, a config snapshot
/// and the selection.
pub struct ProcessLauncher {
    paths: Paths,
    cfg: Config,
    sel: Selection,
    ready_timeout: Duration,
}

impl ProcessLauncher {
    pub fn new(paths: Paths, cfg: Config, sel: Selection) -> Self {
        ProcessLauncher {
            paths,
            cfg,
            sel,
            ready_timeout: READY_TIMEOUT,
        }
    }

    /// Waits this long for an engine to become ready (default 235 s). Keep it
    /// below the scheduler's `start_timeout`.
    pub fn with_ready_timeout(mut self, timeout: Duration) -> Self {
        self.ready_timeout = timeout;
        self
    }
}

#[async_trait::async_trait]
impl Launcher for ProcessLauncher {
    async fn launch(
        &self,
        kind: EngineKind,
        attempt: LaunchAttempt,
    ) -> anyhow::Result<Box<dyn RunningEngine>> {
        // The full model goes into VRAM (-ngl all). On CUDA, check it fits
        // first: rather than fail, the Windows driver spills what does not fit
        // into shared system memory, and the LLM gets many times slower.
        let mut q8_cache = false;
        if kind == EngineKind::Llm && self.sel.llm_backend == Backend::Cuda {
            let ctx = self.cfg.llm.ctx;
            let enough = llm_need_mib(ctx, attempt.mtp, false);
            if let Some(free) = free_vram_mib(enough).await {
                match llm_fit(free, ctx, attempt.mtp) {
                    LlmFit::Mtp => {}
                    LlmFit::MtpQ8Cache if !sets_cache_type(&self.cfg.llm.extra_args) => {
                        info!(free_mib = free, "VRAM is tight: MTP with a q8_0 KV cache");
                        q8_cache = true;
                    }
                    LlmFit::MtpQ8Cache => {}
                    LlmFit::NoMtp | LlmFit::Tight if attempt.mtp => {
                        return Err(NoRoomForMtp(format!(
                            "{free} MiB of VRAM free; the LLM with MTP needs about {} MiB",
                            llm_need_mib(ctx, true, true)
                        ))
                        .into());
                    }
                    LlmFit::NoMtp => {}
                    LlmFit::Tight => warn!(
                        free_mib = free,
                        need_mib = llm_need_mib(ctx, false, false),
                        "the LLM does not fit in free VRAM; Windows may spill it into shared memory, which is much slower"
                    ),
                }
            }
        }
        let addr = engine_addr().context("no free loopback port for the engine")?;
        let spec = match kind {
            EngineKind::Llm => {
                let mut spec = llama_spec(
                    &self.paths,
                    &self.cfg,
                    &self.sel,
                    addr,
                    &new_api_key(),
                    attempt.mtp,
                )?;
                if q8_cache {
                    spec.args
                        .extend(["-ctk", "q8_0", "-ctv", "q8_0"].map(String::from));
                }
                spec
            }
            EngineKind::Image => sd_spec(&self.paths, &self.cfg, &self.sel, addr)?,
        };
        debug!(engine = %kind, %addr, mtp = attempt.mtp, "launching");
        let process = process::start_ready(spec, self.ready_timeout, &attempt.cancel).await?;
        debug!(engine = %kind, %addr, "health check passed");
        Ok(Box::new(process))
    }
}

/// A fresh llama-server API key: 32 random bytes as hex.
fn new_api_key() -> String {
    let bytes: [u8; 32] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::launch::tests::{engine_path, fixture};

    #[test]
    fn the_llm_fit_follows_the_measured_vram() {
        // 20k context, as measured: 13.1 GB with MTP, 12.6 GB with a q8_0
        // cache, 12.2 GB without MTP, each plus the 0.5 GB margin.
        assert_eq!(llm_need_mib(20480, true, false), 13_565);
        assert_eq!(llm_need_mib(20480, true, true), 13_138);
        assert_eq!(llm_need_mib(20480, false, false), 12_695);
        assert_eq!(llm_fit(14_400, 20480, true), LlmFit::Mtp);
        assert_eq!(llm_fit(13_300, 20480, true), LlmFit::MtpQ8Cache);
        assert_eq!(llm_fit(13_000, 20480, true), LlmFit::NoMtp);
        assert_eq!(llm_fit(12_000, 20480, true), LlmFit::Tight);
        assert_eq!(
            llm_fit(14_400, 20480, false),
            LlmFit::NoMtp,
            "MTP off in settings stays off"
        );
        assert_eq!(llm_fit(12_000, 20480, false), LlmFit::Tight);
        // A larger context needs more.
        assert_eq!(llm_fit(14_400, 65536, true), LlmFit::Tight);
        assert_eq!(llm_fit(16_000, 65536, true), LlmFit::MtpQ8Cache);
        assert!(sets_cache_type(&["--cache-type-k=q4_0".into()]));
        assert!(sets_cache_type(&["-ctv".into(), "q8_0".into()]));
        assert!(!sets_cache_type(&["--threads".into(), "8".into()]));
    }

    #[test]
    fn api_keys_are_random_hex() {
        let a = new_api_key();
        let b = new_api_key();
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    /// The real launcher end to end, up to readiness: its llama-server is this
    /// test binary, which rejects llama-server's flags and exits at once.
    #[tokio::test]
    async fn an_engine_that_exits_fails_with_its_log() {
        let fx = fixture(true);
        let llama = if cfg!(windows) {
            "llama-win-cpu"
        } else {
            "llama-linux-cpu"
        };
        let exe = std::env::current_exe().expect("test binary");
        let engine = engine_path(&fx, llama);
        // On Unix a symlink, not a copy: a copy's write handle can leak into a
        // child that another test thread forks at that moment, and exec then
        // fails with ETXTBSY ("Text file busy").
        #[cfg(unix)]
        {
            fs::remove_file(&engine).expect("remove the placeholder");
            std::os::unix::fs::symlink(&exe, &engine).expect("link the test binary");
        }
        #[cfg(not(unix))]
        fs::copy(&exe, &engine).expect("copy the test binary");
        let launcher = ProcessLauncher::new(fx.paths.clone(), Config::default(), fx.sel.clone())
            .with_ready_timeout(Duration::from_secs(60));
        let attempt = LaunchAttempt {
            mtp: true,
            ..LaunchAttempt::default()
        };
        let err = launcher
            .launch(EngineKind::Llm, attempt)
            .await
            .err()
            .expect("it exits");
        let start = err
            .downcast_ref::<StartError>()
            .unwrap_or_else(|| panic!("a StartError, got: {err:#}"));
        assert!(
            start
                .message
                .starts_with("llama-server exited before it was ready ("),
            "{}",
            start.message
        );
        assert!(!start.log.is_empty());
        let log = fs::read_to_string(fx.paths.logs().join("llm.log")).expect("llm.log");
        assert!(
            !log.contains("--api-key"),
            "the key is not an argument: {log}"
        );
        assert!(log.contains(" --host 127."), "{log}");
        assert!(log.contains(" --spec-type draft-mtp"), "{log}");
        assert!(log.contains(" UTC stopped ("), "{log}");
    }

    /// A cancelled launch returns without an engine, and nothing is started.
    #[tokio::test]
    async fn a_cancelled_launch_starts_nothing() {
        let fx = fixture(true);
        let launcher = ProcessLauncher::new(fx.paths.clone(), Config::default(), fx.sel.clone());
        let attempt = LaunchAttempt::default();
        attempt.cancel.cancel();
        let err = launcher
            .launch(EngineKind::Image, attempt)
            .await
            .err()
            .expect("cancelled");
        assert!(format!("{err:#}").contains("cancelled"), "{err:#}");
        assert!(!fx.paths.logs().join("image.log").exists(), "not started");
    }
}
