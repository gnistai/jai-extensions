//! JAI Studio engines: the llama-server and sd-server commands, supervision of
//! their processes, and the scheduler that keeps one engine on the GPU at a
//! time. See `studio/DESIGN.md` section 6.

use std::fmt;

use serde::{Deserialize, Serialize};

pub mod launch;
pub mod launcher;
pub mod process;
pub mod scheduler;

pub use launch::{check_extra_args, engine_addr, llama_spec, sd_spec, LaunchSpec};
pub use launcher::{
    InvalidSettings, LaunchAttempt, Launcher, NoRoomForMtp, NotInstalled, ProcessLauncher,
    RunningEngine, StartError,
};
pub use process::EngineProcess;
pub use scheduler::{
    EngineError, Lease, Scheduler, SchedulerConfig, SchedulerStatus, DEFAULT_START_TIMEOUT,
};

/// The LLM (llama-server) or the image engine (sd-server).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineKind {
    Llm,
    Image,
}

/// State of the loaded engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineState {
    Stopped,
    Starting,
    Ready,
    Stopping,
    Failed,
}

impl fmt::Display for EngineKind {
    /// The serde name: `llm` or `image`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            EngineKind::Llm => "llm",
            EngineKind::Image => "image",
        })
    }
}
