//! JAI Studio's embedded JavaScript host. Each call runs `dist/studio/host.mjs`
//! in a fresh QuickJS runtime (rquickjs) on its own thread, with the web APIs
//! the bundle needs. See `studio/DESIGN.md` section 8.

use anyhow::anyhow;
use tokio_util::sync::CancellationToken;

pub mod imageops;

mod fetch;
mod host;
mod json;
mod modules;
mod runtime;
mod types;
mod web;

#[cfg(test)]
mod tests;

pub use types::{
    Diag, DocInput, EncodeBest, EncodeOptions, EncodeOutput, EncodeSink, EncodeTarget,
    RenderOptions, RenderOutput, ServerRef, ValidateResult,
};

use runtime::{Entry, Job, Output};

const WRONG_KIND: &str = "the JavaScript host returned the wrong kind of result";

/// `dist/studio/host.mjs`, embedded at build time (`build.rs` checks that it
/// exists).
pub const HOST_JS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../dist/studio/host.mjs"
));

/// A job for one of the public calls: JavaScript that runs `MAX_BUSY` (30 s)
/// without a pause is stopped and the call fails (DESIGN.md section 8).
fn job(entry: Entry) -> Job {
    let mut job = Job::new(entry);
    job.max_busy = Some(runtime::MAX_BUSY);
    job
}

/// Parses and validates a document, like `jai validate`. It blocks the calling
/// thread while the JavaScript runs: milliseconds for a real document, and at
/// most 30 s for one crafted to be slow, which then fails. From async code,
/// run it with `spawn_blocking`.
pub fn validate(input: DocInput) -> anyhow::Result<ValidateResult> {
    match runtime::run_blocking(job(Entry::Validate(input)))? {
        Output::Validate(result) => Ok(result),
        _ => Err(anyhow!(WRONG_KIND)),
    }
}

/// Renders a document with the local server's image model. Dropping the
/// future stops the render.
pub async fn render(input: DocInput, opts: RenderOptions) -> anyhow::Result<RenderOutput> {
    let mut job = job(Entry::Render(input, opts.clone()));
    job.server = Some(opts.server);
    match runtime::run(job).await? {
        Output::Render(output) => Ok(output),
        _ => Err(anyhow!(WRONG_KIND)),
    }
}

/// Encodes an image into a document with the local server's models. `cancel`
/// asks the bundle to stop (through `__jaiAbort`); dropping the future stops
/// the encode at once.
pub async fn encode(
    opts: EncodeOptions,
    sink: EncodeSink,
    cancel: CancellationToken,
) -> anyhow::Result<EncodeOutput> {
    let mut job = job(Entry::Encode(opts.clone()));
    job.server = Some(opts.server);
    job.sink = Some(sink);
    job.cancel = Some(cancel);
    match runtime::run(job).await? {
        Output::Encode(output) => Ok(output),
        _ => Err(anyhow!(WRONG_KIND)),
    }
}
