//! Unit tests that run JavaScript inside the host: the web globals, fetch
//! against a local mock, the runtime's cancellation paths, and the spec's
//! conformance suite.

mod cancel;
mod conformance;
mod fetch;
mod web;

use std::path::{Path, PathBuf};

use crate::runtime::{run, run_blocking, Entry, Job, Output};
use crate::ServerRef;

/// A canonical path without Windows' verbatim prefix (`\\?\C:\...`), which
/// the conformance bundle's path shim does not parse.
pub(crate) fn plain(path: &Path) -> PathBuf {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    match canonical.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        Some(rest) => PathBuf::from(rest),
        None => canonical,
    }
}

/// jai-tools, the root of the TypeScript project: `$JAI_TOOLS_DIR` (for test
/// binaries run elsewhere, such as on Windows from WSL), else this checkout.
pub(crate) fn tools_dir() -> PathBuf {
    let dir = std::env::var_os("JAI_TOOLS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.."));
    plain(&dir)
}

/// A job that runs `source` as an ES module; its default export is the result.
pub(crate) fn module_job(source: &str) -> Job {
    Job::new(Entry::Module {
        name: "test.mjs".into(),
        source: source.into(),
    })
}

fn json(output: anyhow::Result<Output>) -> anyhow::Result<serde_json::Value> {
    match output? {
        Output::Json(value) => Ok(value),
        _ => anyhow::bail!("not a module result"),
    }
}

/// Runs a module job on its own JavaScript thread and waits for its result.
pub(crate) fn run_job(job: Job) -> anyhow::Result<serde_json::Value> {
    json(run_blocking(job))
}

/// Runs a module job through the async entry point.
pub(crate) async fn run_job_async(job: Job) -> anyhow::Result<serde_json::Value> {
    json(run(job).await)
}

/// Runs `source` as a module with no server.
pub(crate) fn run_js(source: &str) -> serde_json::Value {
    match run_job(module_job(source)) {
        Ok(value) => value,
        Err(e) => panic!("JavaScript failed: {e:#}"),
    }
}

/// Runs `source` as a module allowed to fetch from `server`.
pub(crate) async fn run_js_with_server(source: &str, server: &str) -> serde_json::Value {
    let mut job = module_job(source);
    job.server = Some(ServerRef {
        url: server.into(),
        token: "test-token".into(),
    });
    match run_job_async(job).await {
        Ok(value) => value,
        Err(e) => panic!("JavaScript failed: {e:#}"),
    }
}
