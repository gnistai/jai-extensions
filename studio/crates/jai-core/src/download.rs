//! Downloading, verifying and unpacking components (DESIGN.md section 5, "How
//! install works").
//!
//! Each component goes through these steps, at most `concurrency` at a time:
//! 1. Installed components are skipped and reported as Done.
//! 2. A full-size model already in `models/` but not verified (status reports
//!    it as Partial) is hashed in place (Verifying) and recorded if it
//!    matches; otherwise it is deleted.
//! 3. With `reuse`, a matching file found on this PC is hashed (Reusing), then
//!    hard-linked into `models/`, or copied when linking fails.
//! 4. Otherwise the file is downloaded to `downloads/<file>.part`. An existing
//!    part is hashed first (Verifying), then resumed with
//!    `Range: bytes=<len>-`: a 206 response appends, a 200 starts over. Data
//!    is hashed as it arrives. Failed attempts are retried three times with
//!    exponential backoff; an attempt that received data resets that count.
//! 5. The size and SHA-256 are checked. On a mismatch the part file is deleted
//!    and the component fails.
//! 6. Models are renamed into `models/<file>` and recorded in
//!    `models/.verified.json`. Archives are unpacked into `engines/<dir>/`,
//!    marked with `.ok-<id>` holding the sha256, and then deleted.
//!
//! Before any of this, the free disk space must cover the remaining bytes plus
//! 1 GiB. Canceling keeps part files, so the next install resumes them. While
//! a component installs, `downloads/<file>.lock` is locked, so two installs
//! (even in two processes) never write the same part file.

mod files;
mod http;
mod report;
mod unpack;

#[cfg(test)]
mod tests;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::catalog::{Component, Selection, Unpack};
use crate::install::{self, InstallState};
use crate::paths::Paths;
use crate::verified::{self, Verified};
use crate::{fsutil, reuse};

use report::{Reporter, Sink};

/// Progress of one component.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub id: String,
    pub state: DlState,
    pub bytes: u64,
    pub total: u64,
    pub speed_bps: u64,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DlState {
    Queued,
    Reusing,
    Downloading,
    Verifying,
    Unpacking,
    Done,
    Failed,
    Canceled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DownloadOptions {
    /// Reuse matching files found on this PC (`reuse::find_reusable`).
    pub reuse: bool,
    /// Components downloaded at once (3).
    pub concurrency: usize,
}

impl Default for DownloadOptions {
    fn default() -> Self {
        DownloadOptions {
            reuse: true,
            concurrency: 3,
        }
    }
}

/// Installs the components that are not installed yet. `progress` gets every
/// state change of every component and, while bytes move, at most about ten
/// updates a second per component. Fails when any component fails, with each
/// failure in the message, or when `cancel` fires.
pub async fn install(
    paths: &Paths,
    components: &[Component],
    opts: DownloadOptions,
    progress: impl Fn(Progress) + Send + Sync + 'static,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    install_with(
        paths,
        components,
        opts,
        Tuning::default(),
        Arc::new(progress),
        cancel,
    )
    .await
}

/// Free space on the disk that holds the data root, for the current user.
pub fn disk_free(paths: &Paths) -> Option<u64> {
    let existing = fsutil::nearest_existing(&paths.root)?;
    fs4::available_space(existing).ok()
}

/// Bytes the install still has to write for the selection: the rest of each
/// download, plus copies of reusable files that are on another disk (files on
/// the same disk are hard-linked). Installed files and full-size files that
/// only need verifying count as 0.
pub fn bytes_needed(paths: &Paths, sel: &Selection) -> u64 {
    install::status(paths, sel)
        .iter()
        .map(|s| install::bytes_to_write(paths, &s.component, &s.state))
        .sum()
}

/// Knobs fixed in production, shortened by tests.
pub(crate) struct Tuning {
    /// Delay before the first retry; it doubles for each further one.
    pub backoff: Duration,
    /// Retries after consecutive failed attempts.
    pub retries: u32,
    /// Longest wait for response headers or the next piece of the body.
    pub read_timeout: Duration,
    pub connect_timeout: Duration,
    /// Folders searched for reusable model files.
    pub reuse_dirs: Vec<PathBuf>,
    /// Least time between two byte-count progress events of one component.
    pub progress_interval: Duration,
    /// Free space required on top of the remaining bytes.
    pub disk_margin: u64,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning {
            backoff: Duration::from_secs(1),
            retries: 3,
            read_timeout: Duration::from_secs(60),
            connect_timeout: Duration::from_secs(30),
            reuse_dirs: reuse::default_dirs(),
            progress_interval: Duration::from_millis(100),
            disk_margin: 1 << 30,
        }
    }
}

/// Why one component failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum DlError {
    #[error("canceled")]
    Canceled,
    #[error("HTTP {0}")]
    Status(reqwest::StatusCode),
    #[error("{}", error_chain(.0))]
    Network(reqwest::Error),
    #[error("no data for {} seconds", .0.as_secs())]
    Stalled(Duration),
    #[error("the download ended at byte {got} of {expected}")]
    Incomplete { got: u64, expected: u64 },
    #[error(
        "the server answered a request for bytes from {expected} with Content-Range {header:?}"
    )]
    BadRange { expected: u64, header: String },
    #[error("the server rejected the range request")]
    RangeRejected,
    #[error("the file on the server has {got} bytes, not {expected}")]
    WrongSize { got: u64, expected: u64 },
    #[error("the server sent more than {expected} bytes")]
    TooLong { expected: u64 },
    #[error("SHA-256 mismatch: expected {expected}, got {actual}")]
    HashMismatch { expected: String, actual: String },
    #[error("unsafe path in archive: {0}")]
    UnsafePath(String),
    #[error("invalid archive: {0}")]
    BadArchive(String),
    #[error("{0}")]
    Invalid(String),
    #[error("another download of this file is in progress")]
    Busy,
    #[error("{}: {error}", .path.display())]
    Io { path: PathBuf, error: io::Error },
}

impl DlError {
    /// Worth another attempt: network trouble, server errors, throttling and
    /// confused range answers. Not: missing files, wrong sizes or hashes,
    /// disk errors.
    fn retryable(&self) -> bool {
        match self {
            DlError::Status(s) => {
                s.is_server_error()
                    || *s == reqwest::StatusCode::REQUEST_TIMEOUT
                    || *s == reqwest::StatusCode::TOO_MANY_REQUESTS
            }
            DlError::Network(e) => !e.is_builder(),
            DlError::Stalled(_)
            | DlError::Incomplete { .. }
            | DlError::BadRange { .. }
            | DlError::RangeRejected => true,
            _ => false,
        }
    }
}

/// An error and its sources, as one line.
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let line = cause.to_string();
        if !text.contains(&line) {
            text.push_str(": ");
            text.push_str(&line);
        }
        source = cause.source();
    }
    text
}

/// Attaches the path to an I/O error.
pub(crate) trait IoAt<T> {
    fn at(self, path: &Path) -> Result<T, DlError>;
}

impl<T> IoAt<T> for io::Result<T> {
    fn at(self, path: &Path) -> Result<T, DlError> {
        self.map_err(|error| DlError::Io {
            path: path.to_path_buf(),
            error,
        })
    }
}

/// What every step of one install run shares.
struct Ctx<'a> {
    paths: &'a Paths,
    tuning: &'a Tuning,
    client: reqwest::Client,
    sink: Sink,
    cancel: CancellationToken,
    /// `tuning.reuse_dirs`, or nothing when reuse is off.
    reuse_dirs: &'a [PathBuf],
}

/// [`install`] with explicit knobs.
pub(crate) async fn install_with(
    paths: &Paths,
    components: &[Component],
    opts: DownloadOptions,
    tuning: Tuning,
    sink: Sink,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    paths
        .ensure_dirs()
        .with_context(|| format!("cannot create the folders in {}", paths.root.display()))?;
    let reuse_dirs: &[PathBuf] = if opts.reuse { &tuning.reuse_dirs } else { &[] };

    let verified = Verified::load(paths);
    let mut todo: Vec<&Component> = Vec::new();
    let mut needed = 0u64;
    for (i, c) in components.iter().enumerate() {
        if components[..i].iter().any(|seen| seen.id == c.id) {
            continue;
        }
        let state = install::state_of(paths, c, &verified, reuse_dirs);
        if state == InstallState::Installed {
            files::remove_stale_part(paths, c);
            Reporter::new(c, &sink, tuning.progress_interval).finish(DlState::Done, None);
        } else {
            needed = needed.saturating_add(install::bytes_to_write(paths, c, &state));
            todo.push(c);
        }
    }
    if todo.is_empty() {
        return Ok(());
    }

    if let Some(free) = disk_free(paths) {
        if free < needed.saturating_add(tuning.disk_margin) {
            let message = format!(
                "Not enough disk space in {}: the download needs {} plus {} of headroom, but only {} is free.",
                paths.root.display(),
                gigabytes(needed),
                gigabytes(tuning.disk_margin),
                gigabytes(free)
            );
            for c in &todo {
                Reporter::new(c, &sink, tuning.progress_interval)
                    .finish(DlState::Failed, Some(message.clone()));
            }
            anyhow::bail!(message);
        }
    }

    for c in &todo {
        let bytes = files::part_len(paths, c);
        Reporter::new(c, &sink, tuning.progress_interval).set(DlState::Queued, bytes);
    }
    let ctx = Ctx {
        paths,
        tuning: &tuning,
        client: http::client(&tuning)?,
        sink: sink.clone(),
        cancel: cancel.clone(),
        reuse_dirs,
    };
    // The futures are made in a plain loop: a closure here would hide from
    // the compiler that this future is Send.
    let mut jobs = Vec::with_capacity(todo.len());
    for c in todo {
        jobs.push(run_tagged(&ctx, c));
    }
    let results: Vec<(&Component, Result<(), DlError>)> = futures::stream::iter(jobs)
        .buffer_unordered(opts.concurrency.max(1))
        .collect()
        .await;

    if results
        .iter()
        .any(|(_, r)| matches!(r, Err(DlError::Canceled)))
    {
        anyhow::bail!("The download was canceled.");
    }
    let failed: Vec<String> = results
        .iter()
        .filter_map(|(c, r)| r.as_ref().err().map(|e| format!("{}: {e}", c.id)))
        .collect();
    if !failed.is_empty() {
        anyhow::bail!(
            "{} of {} downloads failed: {}",
            failed.len(),
            results.len(),
            failed.join("; ")
        );
    }
    Ok(())
}

/// [`run_one`], paired with its component.
async fn run_tagged<'c>(ctx: &Ctx<'_>, c: &'c Component) -> (&'c Component, Result<(), DlError>) {
    (c, run_one(ctx, c).await)
}

/// Installs one component and reports how it ended.
async fn run_one(ctx: &Ctx<'_>, c: &Component) -> Result<(), DlError> {
    let rep = Reporter::new(c, &ctx.sink, ctx.tuning.progress_interval);
    let result = async {
        files::check_names(c)?;
        let _lock = files::PartLock::acquire(ctx.paths, c)?;
        if c.unpack.is_some() {
            install_archive(ctx, c, &rep).await
        } else {
            install_file(ctx, c, &rep).await
        }
    }
    .await;
    match &result {
        Ok(()) => {
            info!(id = %c.id, "installed");
            rep.finish(DlState::Done, None);
        }
        Err(DlError::Canceled) => {
            info!(id = %c.id, "canceled");
            rep.finish(DlState::Canceled, None);
        }
        Err(e) => {
            warn!(id = %c.id, error = %e, "install failed");
            rep.finish(DlState::Failed, Some(e.to_string()));
        }
    }
    result
}

/// A model file: verify in place, reuse, or download; then record it.
async fn install_file(ctx: &Ctx<'_>, c: &Component, rep: &Reporter) -> Result<(), DlError> {
    let dest = ctx.paths.models().join(&c.file);
    if let Ok(meta) = std::fs::metadata(&dest) {
        if meta.is_file() && meta.len() == c.size {
            rep.set(DlState::Verifying, 0);
            let (hasher, _) = files::hash(dest.clone(), rep.clone(), ctx.cancel.clone()).await?;
            if files::digest_matches(hasher, &c.sha256) {
                files::remove_stale_part(ctx.paths, c);
                return verified::record(ctx.paths, &c.file, &c.sha256).at(&dest);
            }
            warn!(file = %dest.display(), "the model file does not match the catalog; replacing it");
        }
        files::remove_file(&dest)?;
        if let Err(e) = verified::forget(ctx.paths, &c.file) {
            warn!(error = %e, "cannot update the verification record");
        }
    }

    if let Some(candidate) = reusable(ctx, c) {
        match reuse_file(ctx, c, &candidate, &dest, rep).await {
            Ok(true) => {
                files::remove_stale_part(ctx.paths, c);
                return verified::record(ctx.paths, &c.file, &c.sha256).at(&dest);
            }
            Ok(false) => {
                warn!(id = %c.id, candidate = %candidate.display(), "the file found on this PC does not match; downloading instead")
            }
            Err(DlError::Canceled) => return Err(DlError::Canceled),
            Err(e) => {
                warn!(id = %c.id, error = %e, "cannot reuse the file found on this PC; downloading instead")
            }
        }
    }

    let part = install::part_path(ctx.paths, c);
    let digest = http::download(ctx, c, &part, rep).await?;
    verify(c, &digest, &part, rep)?;
    files::move_file(part, dest.clone()).await?;
    verified::record(ctx.paths, &c.file, &c.sha256).at(&dest)
}

/// A reuse candidate outside the data root, when reuse is on.
fn reusable(ctx: &Ctx<'_>, c: &Component) -> Option<PathBuf> {
    let candidate = reuse::find_reusable_in(c, ctx.reuse_dirs)?;
    let root = std::fs::canonicalize(&ctx.paths.root).unwrap_or_else(|_| ctx.paths.root.clone());
    (!candidate.starts_with(&root)).then_some(candidate)
}

/// Reuses a candidate when its SHA-256 matches; false when it does not.
///
/// A candidate that [`install::reuse_links`] allows is hashed, then
/// hard-linked to `dest`. Anything else (another disk, or a file that other
/// accounts could change after it was hashed) is copied through the part
/// file and hashed as it is copied, so what is verified is what is kept.
/// A failed link falls back to the copy.
async fn reuse_file(
    ctx: &Ctx<'_>,
    c: &Component,
    candidate: &Path,
    dest: &Path,
    rep: &Reporter,
) -> Result<bool, DlError> {
    info!(id = %c.id, candidate = %candidate.display(), "checking a matching file found on this PC");
    rep.set(DlState::Reusing, 0);
    if install::reuse_links(ctx.paths, candidate) {
        let (hasher, len) =
            files::hash(candidate.to_path_buf(), rep.clone(), ctx.cancel.clone()).await?;
        if len != c.size || !files::digest_matches(hasher, &c.sha256) {
            return Ok(false);
        }
        match std::fs::hard_link(candidate, dest) {
            Ok(()) => {
                info!(id = %c.id, "hard-linked the file found on this PC");
                return Ok(true);
            }
            Err(e) => {
                info!(id = %c.id, error = %e, "cannot hard-link the file found on this PC; copying it")
            }
        }
        rep.set(DlState::Reusing, 0);
    } else {
        info!(id = %c.id, "copying the file found on this PC: it is on another disk, or other accounts can change it");
    }
    let part = install::part_path(ctx.paths, c);
    let digest = files::copy_hashing(
        candidate.to_path_buf(),
        part.clone(),
        rep.clone(),
        ctx.cancel.clone(),
    )
    .await?;
    if !digest.eq_ignore_ascii_case(&c.sha256) {
        files::remove_file(&part)?;
        return Ok(false);
    }
    files::move_file(part, dest.to_path_buf()).await?;
    Ok(true)
}

/// An engine or runtime archive: download, verify, unpack, mark, delete.
async fn install_archive(ctx: &Ctx<'_>, c: &Component, rep: &Reporter) -> Result<(), DlError> {
    let (Some(kind), Some(dir)) = (c.unpack, c.dir.as_deref()) else {
        return Err(DlError::Invalid(format!(
            "the catalog entry {} is an archive without a target folder",
            c.id
        )));
    };
    let part = install::part_path(ctx.paths, c);
    let digest = http::download(ctx, c, &part, rep).await?;
    verify(c, &digest, &part, rep)?;

    rep.set(DlState::Unpacking, c.size);
    let target = ctx.paths.engines().join(dir);
    match unpack_engine(ctx, c, kind, &part, &target).await {
        Ok(()) => {}
        Err(e @ (DlError::UnsafePath(_) | DlError::BadArchive(_))) => {
            // The verified archive itself is unusable: downloading it again
            // would give the same bytes, so free the space.
            files::remove_file(&part)?;
            return Err(e);
        }
        Err(e) => return Err(e),
    }
    let marker = target.join(format!(".ok-{}", c.id));
    fsutil::write_atomic(&marker, c.sha256.to_ascii_lowercase().as_bytes(), None).at(&marker)?;
    files::remove_file(&part)
}

/// Unpacks a verified archive and checks that the engine executable, if the
/// component has one, is there and executable.
async fn unpack_engine(
    ctx: &Ctx<'_>,
    c: &Component,
    kind: Unpack,
    part: &Path,
    target: &Path,
) -> Result<(), DlError> {
    let strip = c.strip.unwrap_or(0);
    unpack::run(
        part.to_path_buf(),
        target.to_path_buf(),
        kind,
        strip,
        ctx.cancel.clone(),
    )
    .await?;
    if let Some(exe) = c.exe.as_deref() {
        let found = install::find_file(target, exe, install::EXE_DEPTH)
            .ok_or_else(|| DlError::BadArchive(format!("{exe} is not in the archive")))?;
        files::ensure_executable(&found)?;
    }
    Ok(())
}

/// Checks the SHA-256 of a complete part file; deletes the part on a mismatch.
fn verify(c: &Component, digest: &str, part: &Path, rep: &Reporter) -> Result<(), DlError> {
    rep.set(DlState::Verifying, c.size);
    if digest.eq_ignore_ascii_case(&c.sha256) {
        return Ok(());
    }
    files::remove_file(part)?;
    Err(DlError::HashMismatch {
        expected: c.sha256.to_ascii_lowercase(),
        actual: digest.to_owned(),
    })
}

/// Bytes as decimal gigabytes with one decimal, as the UI shows them.
fn gigabytes(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}
