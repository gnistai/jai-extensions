//! `jai models status | download | verify | path`.

use std::io::{IsTerminal, Read as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use jai_core::download::{self, DlState, DownloadOptions, Progress};
use jai_core::gpu::{self, GpuInfo};
use jai_core::install::{self, ComponentStatus, InstallState};
use jai_core::{catalog, Backend, Component, Paths, Selection};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use super::{interrupted, usage};
use crate::output::{display_path, format_bytes, format_duration, format_speed, percent};
use crate::term::{self, Live, Stream};
use crate::ModelsCommand;

/// Space kept free on top of the downloads (DESIGN.md section 5).
const SPARE_BYTES: u64 = 1 << 30;

pub async fn run(command: ModelsCommand) -> anyhow::Result<ExitCode> {
    let paths = super::paths()?;
    match command {
        ModelsCommand::Status => status(&paths).await,
        ModelsCommand::Download(args) => download(&paths, args).await,
        ModelsCommand::Verify => verify(&paths).await,
        ModelsCommand::Path => {
            outln!("{}", paths.models().display());
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// The selection for this PC and what is on disk.
pub struct Setup {
    pub gpu: Option<GpuInfo>,
    pub backend: Backend,
    pub selection: Selection,
    pub statuses: Vec<ComponentStatus>,
}

/// Detects the GPU (nvidia-smi), selects the components and reads their
/// install state, off the async threads.
pub async fn setup(paths: &Paths) -> anyhow::Result<Setup> {
    let paths = paths.clone();
    tokio::task::spawn_blocking(move || {
        let cfg = crate::settings::load_config(&paths);
        let gpu = gpu::detect();
        let backend = gpu::effective_backend(cfg.backend, gpu.as_ref());
        let selection = catalog::select(catalog::catalog(), catalog::current_os(), backend);
        let statuses = install::status(&paths, &selection);
        Setup {
            gpu,
            backend,
            selection,
            statuses,
        }
    })
    .await
    .context("the setup task failed")
}

/// One word for an install state.
pub fn state_word(state: &InstallState) -> &'static str {
    match state {
        InstallState::Installed => "installed",
        InstallState::Partial { .. } => "partial",
        InstallState::Missing => "missing",
        InstallState::Reusable { .. } => "on this PC",
    }
}

fn row_note(s: &ComponentStatus, reuse: bool) -> String {
    match &s.state {
        InstallState::Partial { bytes } => format!(
            "{} of {} downloaded",
            format_bytes(*bytes),
            format_bytes(s.component.size)
        ),
        InstallState::Reusable { path } if reuse => format!("will reuse {}", display_path(path)),
        InstallState::Reusable { path } => format!("found at {}", display_path(path)),
        _ => String::new(),
    }
}

/// Aligned rows: state, label, size and a note.
pub fn table(statuses: &[&ComponentStatus], reuse: bool) -> Vec<String> {
    let label_width = statuses
        .iter()
        .map(|s| s.component.label.chars().count())
        .max()
        .unwrap_or(0);
    statuses
        .iter()
        .map(|s| {
            let note = row_note(s, reuse);
            let line = format!(
                "  {:<10}  {:<label_width$}  {:>8}",
                state_word(&s.state),
                s.component.label,
                format_bytes(s.component.size),
            );
            if note.is_empty() {
                line
            } else {
                format!("{line}  ({note})")
            }
        })
        .collect()
}

/// Bytes still to download: missing components in full, partial ones the
/// rest, and files found on this PC nothing when they are reused.
pub fn plan_bytes(statuses: &[&ComponentStatus], reuse: bool) -> u64 {
    statuses
        .iter()
        .map(|s| match &s.state {
            InstallState::Installed => 0,
            InstallState::Partial { bytes } => s.component.size.saturating_sub(*bytes),
            InstallState::Missing => s.component.size,
            InstallState::Reusable { .. } => {
                if reuse {
                    0
                } else {
                    s.component.size
                }
            }
        })
        .sum()
}

/// One line per licence: the licence, then the components under it, in
/// catalog order.
pub fn licence_lines(statuses: &[&ComponentStatus]) -> Vec<String> {
    let mut groups: Vec<(&str, Vec<&str>)> = Vec::new();
    for s in statuses {
        let c = &s.component;
        match groups.iter_mut().find(|(l, _)| *l == c.license) {
            Some((_, labels)) => labels.push(&c.label),
            None => groups.push((&c.license, vec![&c.label])),
        }
    }
    groups
        .into_iter()
        .map(|(licence, labels)| format!("  {licence}: {}", labels.join(", ")))
        .collect()
}

/// The `--only` ids, each of which must be in the selection.
pub fn check_only(only: &[String], statuses: &[ComponentStatus]) -> anyhow::Result<Vec<String>> {
    let ids: Vec<String> = only
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let unknown: Vec<&str> = ids
        .iter()
        .filter(|id| !statuses.iter().any(|s| &s.component.id == *id))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        let known: Vec<&str> = statuses.iter().map(|s| s.component.id.as_str()).collect();
        return Err(usage(format!(
            "unknown component {}; this PC uses: {}",
            unknown.join(", "),
            known.join(", ")
        )));
    }
    Ok(ids)
}

fn gpu_line(setup: &Setup) -> String {
    let backend = serde_json::to_value(setup.backend)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    match &setup.gpu {
        Some(g) => format!("Backend: {backend} ({})", g.name),
        None => format!("Backend: {backend} (no NVIDIA GPU found)"),
    }
}

async fn status(paths: &Paths) -> anyhow::Result<ExitCode> {
    let setup = setup(paths).await?;
    outln!("Data folder: {}", paths.root.display());
    outln!("{}", gpu_line(&setup));
    outln!();
    let all: Vec<&ComponentStatus> = setup.statuses.iter().collect();
    for line in table(&all, true) {
        outln!("{line}");
    }
    outln!();
    outln!("Licences (each component keeps its own; catalog.json has the sources):");
    for line in licence_lines(&all) {
        outln!("{line}");
    }
    let total: u64 = setup.statuses.iter().map(|s| s.component.size).sum();
    let needed = plan_bytes(&all, true);
    let free = download::disk_free(paths)
        .map(format_bytes)
        .unwrap_or_else(|| "unknown".into());
    outln!();
    if needed == 0 && all.iter().all(|s| s.state == InstallState::Installed) {
        outln!(
            "Everything is installed ({}). Free disk: {free}.",
            format_bytes(total)
        );
    } else {
        outln!(
            "To download: {} of {}. Free disk: {free}.",
            format_bytes(needed),
            format_bytes(total)
        );
        outln!("Run `jai models download` to install the rest.");
    }
    Ok(ExitCode::SUCCESS)
}

/// Asks on the terminal; usage error without one.
async fn confirm(question: &str) -> anyhow::Result<bool> {
    if !std::io::stdin().is_terminal() {
        return Err(usage(
            "not asking without a terminal: pass --yes to download without confirmation",
        ));
    }
    term::write_raw(Stream::Out, &format!("{question} [y/N] "));
    let answer = tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).map(|_| line)
    })
    .await
    .context("the input task failed")??;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// An exclusive lock on `downloads/.lock`, so the GUI and the CLI never
/// download into the same `.part` files at once.
pub fn download_lock(paths: &Paths) -> anyhow::Result<std::fs::File> {
    let dir = paths.downloads();
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(".lock"))
        .with_context(|| format!("cannot open {}", dir.join(".lock").display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!(
            "another download is running (in JAI Studio or another `jai models download`)"
        ),
        Err(std::fs::TryLockError::Error(e)) => Err(e).context("cannot lock the downloads folder"),
    }
}

async fn download(paths: &Paths, args: crate::DownloadArgs) -> anyhow::Result<ExitCode> {
    let setup = setup(paths).await?;
    let only = check_only(&args.only, &setup.statuses)?;
    let todo: Vec<&ComponentStatus> = setup
        .statuses
        .iter()
        .filter(|s| s.state != InstallState::Installed)
        .filter(|s| only.is_empty() || only.contains(&s.component.id))
        .collect();
    if todo.is_empty() {
        outln!("Everything is installed.");
        return Ok(ExitCode::SUCCESS);
    }
    let reuse = !args.no_reuse;
    outln!("{}", gpu_line(&setup));
    outln!();
    for line in table(&todo, reuse) {
        outln!("{line}");
    }
    outln!();
    outln!("Licences (each component keeps its own; catalog.json has the sources):");
    for line in licence_lines(&todo) {
        outln!("{line}");
    }
    let needed = plan_bytes(&todo, reuse);
    let free = download::disk_free(paths);
    outln!();
    outln!(
        "Download {} into {}",
        format_bytes(needed),
        paths.root.display()
    );
    outln!(
        "Free disk: {}",
        free.map(format_bytes).unwrap_or_else(|| "unknown".into())
    );
    if let Some(free) = free {
        if free < needed.saturating_add(SPARE_BYTES) {
            anyhow::bail!(
                "not enough free disk space: {} needed plus 1 GiB to spare, {} free",
                format_bytes(needed),
                format_bytes(free)
            );
        }
    }
    if !args.yes && !confirm("Continue?").await? {
        outln!("Canceled.");
        return Ok(ExitCode::FAILURE);
    }
    let _lock = download_lock(paths)?;

    let components: Vec<Component> = todo.iter().map(|s| s.component.clone()).collect();
    let live = Live::new();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Progress>();
    let cancel = CancellationToken::new();
    let opts = DownloadOptions {
        reuse,
        concurrency: 3,
    };
    let install = download::install(
        paths,
        &components,
        opts,
        move |p| {
            let _ = tx.send(p);
        },
        cancel.clone(),
    );
    tokio::pin!(install);
    let ctrl_c = interrupted();
    tokio::pin!(ctrl_c);
    let mut view = DownloadView::new(&components);
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    let mut last_plain = Instant::now();
    let result = loop {
        tokio::select! {
            r = &mut install => break r,
            Some(p) = rx.recv() => {
                if let Some(line) = view.update(&p) {
                    live.out(&line);
                }
            }
            _ = tick.tick() => {
                if live.is_tty() {
                    live.status(&view.status_line());
                } else if last_plain.elapsed() >= Duration::from_secs(10) {
                    last_plain = Instant::now();
                    live.out(&view.status_line());
                }
            }
            () = &mut ctrl_c, if !cancel.is_cancelled() => {
                live.err("Canceling… press Ctrl+C again to stop at once.");
                cancel.cancel();
            }
        }
    };
    while let Ok(p) = rx.try_recv() {
        if let Some(line) = view.update(&p) {
            live.out(&line);
        }
    }
    live.clear();
    match result {
        Ok(()) => {
            outln!("Done: {} components installed.", components.len());
            Ok(ExitCode::SUCCESS)
        }
        Err(e) if cancel.is_cancelled() => {
            tracing::debug!("download canceled: {e:#}");
            errln!("Canceled. Partial downloads resume next time.");
            Ok(ExitCode::FAILURE)
        }
        Err(e) => Err(e.context("the download failed; run it again to resume")),
    }
}

/// What the download looks like, for the terminal.
pub struct DownloadView {
    items: Vec<Item>,
}

struct Item {
    id: String,
    label: String,
    total: u64,
    bytes: u64,
    speed: u64,
    state: DlState,
}

impl DownloadView {
    pub fn new(components: &[Component]) -> DownloadView {
        DownloadView {
            items: components
                .iter()
                .map(|c| Item {
                    id: c.id.clone(),
                    label: c.label.clone(),
                    total: c.size,
                    bytes: 0,
                    speed: 0,
                    state: DlState::Queued,
                })
                .collect(),
        }
    }

    /// Applies one update. Returns a line to print when the component moved
    /// to a state worth a line.
    pub fn update(&mut self, p: &Progress) -> Option<String> {
        let item = self.items.iter_mut().find(|i| i.id == p.id)?;
        let changed = item.state != p.state;
        item.state = p.state;
        item.bytes = p.bytes;
        item.speed = p.speed_bps;
        if p.total > 0 {
            item.total = p.total;
        }
        if !changed {
            return None;
        }
        let label = &item.label;
        Some(match p.state {
            DlState::Queued => return None,
            DlState::Reusing => format!("  checking   {label} (found on this PC)"),
            DlState::Downloading => format!("  download   {label} ({})", format_bytes(item.total)),
            DlState::Verifying => format!("  verifying  {label}"),
            DlState::Unpacking => format!("  unpacking  {label}"),
            DlState::Done => format!("  done       {label}"),
            DlState::Failed => format!(
                "  failed     {label}: {}",
                p.error.as_deref().unwrap_or("unknown error")
            ),
            DlState::Canceled => format!("  canceled   {label}"),
        })
    }

    /// "2 active · 12.3 GB of 30.3 GB (40%) · 85.2 MB/s · 3m 31s left".
    pub fn status_line(&self) -> String {
        let total: u64 = self.items.iter().map(|i| i.total).sum();
        let done: u64 = self
            .items
            .iter()
            .map(|i| match i.state {
                DlState::Done => i.total,
                _ => i.bytes.min(i.total),
            })
            .sum();
        let active = self
            .items
            .iter()
            .filter(|i| {
                matches!(
                    i.state,
                    DlState::Downloading
                        | DlState::Reusing
                        | DlState::Verifying
                        | DlState::Unpacking
                )
            })
            .count();
        let speed: u64 = self
            .items
            .iter()
            .filter(|i| i.state == DlState::Downloading)
            .map(|i| i.speed)
            .sum();
        let mut line = format!(
            "{active} active · {} of {} ({})",
            format_bytes(done),
            format_bytes(total),
            percent(done, total)
        );
        if speed > 0 {
            line.push_str(&format!(" · {}", format_speed(speed)));
            let left = total.saturating_sub(done) / speed;
            line.push_str(&format!(" · {} left", format_duration(left)));
        }
        line
    }
}

/// SHA-256 of a file as lowercase hex, adding to `progress` as it reads.
pub fn sha256_file(path: &Path, progress: &AtomicU64) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        progress.fetch_add(n as u64, Ordering::Relaxed);
    }
    Ok(hex(&hasher.finalize()))
}

/// Lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Hashes a model file with a progress line.
async fn hash_with_progress(
    live: &Arc<Live>,
    label: &str,
    path: PathBuf,
    size: u64,
) -> anyhow::Result<String> {
    let progress = Arc::new(AtomicU64::new(0));
    let task = {
        let progress = progress.clone();
        tokio::task::spawn_blocking(move || sha256_file(&path, &progress))
    };
    tokio::pin!(task);
    let started = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    loop {
        tokio::select! {
            r = &mut task => {
                live.clear();
                return Ok(r.context("the hashing task failed")??);
            }
            _ = tick.tick() => {
                let done = progress.load(Ordering::Relaxed);
                let secs = started.elapsed().as_secs_f64().max(0.001);
                live.status(&format!(
                    "Verifying {label}: {} · {}",
                    percent(done, size),
                    format_speed((done as f64 / secs) as u64)
                ));
            }
        }
    }
}

/// The marker an unpacked archive leaves, `engines/<dir>/.ok-<id>` holding
/// its SHA-256, and for an engine its executable, found the way the install
/// status and the launcher find it (`install::engine_exe`, which looks a few
/// folders deep), so `verify` and `status` agree.
fn archive_ok(paths: &Paths, sel: &Selection, c: &Component) -> Result<(), String> {
    let Some(dir) = c.dir.as_deref() else {
        return Err("the catalog names no folder".into());
    };
    let dir = paths.engines().join(dir);
    let marker = dir.join(format!(".ok-{}", c.id));
    let recorded =
        std::fs::read_to_string(&marker).map_err(|_| format!("{} is missing", marker.display()))?;
    if !recorded.trim().eq_ignore_ascii_case(&c.sha256) {
        return Err(format!("{} names another build", marker.display()));
    }
    if let Some(exe) = c.exe.as_deref() {
        if c.role.is_engine() && install::engine_exe(paths, sel, c.role).is_none() {
            return Err(format!("{exe} is missing from {}", dir.display()));
        }
    }
    Ok(())
}

/// A model whose bytes no longer match the catalog is removed, so that
/// `jai models download` (which skips installed files) fetches it again.
/// Removing a hard link leaves the file it shares with LM Studio in place.
fn remove_damaged(path: &Path) -> String {
    match std::fs::remove_file(path) {
        Ok(()) => "SHA-256 mismatch: removed the damaged file".to_string(),
        Err(e) => format!(
            "SHA-256 mismatch, and it cannot be removed ({e}): delete {}",
            path.display()
        ),
    }
}

async fn verify(paths: &Paths) -> anyhow::Result<ExitCode> {
    let setup = setup(paths).await?;
    let live = Live::new();
    let mut problems = 0;
    for s in &setup.statuses {
        let c = &s.component;
        let verdict = match (&s.state, c.unpack) {
            (InstallState::Installed, None) => {
                let path = paths.models().join(&c.file);
                let actual = hash_with_progress(&live, &c.label, path.clone(), c.size).await?;
                if actual.eq_ignore_ascii_case(&c.sha256) {
                    Ok(())
                } else {
                    Err(remove_damaged(&path))
                }
            }
            (InstallState::Installed, Some(_)) => archive_ok(paths, &setup.selection, c),
            (state, _) => Err(format!("not installed ({})", state_word(state))),
        };
        match verdict {
            Ok(()) => live.out(&format!("  ok         {}", c.label)),
            Err(why) => {
                problems += 1;
                live.out(&format!("  problem    {}: {why}", c.label));
            }
        }
    }
    if problems == 0 {
        outln!("All {} components are intact.", setup.statuses.len());
        Ok(ExitCode::SUCCESS)
    } else {
        outln!(
            "{problems} component{} need attention. Run `jai models download` to download {} again.",
            if problems == 1 { "" } else { "s" },
            if problems == 1 { "it" } else { "them" }
        );
        Ok(ExitCode::FAILURE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jai_core::Role;

    fn component(id: &str, label: &str, size: u64) -> Component {
        Component {
            id: id.into(),
            role: Role::LlmModel,
            label: label.into(),
            file: format!("{id}.gguf"),
            url: "https://example.invalid/x".into(),
            size,
            sha256: "00".into(),
            license: "MIT".into(),
            source: "test".into(),
            os: None,
            backend: None,
            unpack: None,
            strip: None,
            dir: None,
            exe: None,
        }
    }

    fn status(c: Component, state: InstallState) -> ComponentStatus {
        ComponentStatus {
            path: PathBuf::from("/m").join(&c.file),
            component: c,
            state,
        }
    }

    fn sample() -> Vec<ComponentStatus> {
        vec![
            status(
                component("llm-model", "Qwen3.8 27B", 10_934_860_704),
                InstallState::Installed,
            ),
            status(
                component("image-diffusion", "FLUX.1 schnell", 12_801_721_248),
                InstallState::Partial {
                    bytes: 2_801_721_248,
                },
            ),
            status(
                component("image-vae", "FLUX VAE", 335_304_388),
                InstallState::Missing,
            ),
            status(
                component("image-t5xxl", "T5-XXL", 5_061_584_064),
                InstallState::Reusable {
                    path: PathBuf::from("/lmstudio/t5.gguf"),
                },
            ),
        ]
    }

    #[test]
    fn bytes_to_download() {
        let s = sample();
        let all: Vec<&ComponentStatus> = s.iter().collect();
        assert_eq!(plan_bytes(&all, true), 10_000_000_000 + 335_304_388);
        assert_eq!(
            plan_bytes(&all, false),
            10_000_000_000 + 335_304_388 + 5_061_584_064
        );
    }

    #[test]
    fn status_table() {
        let s = sample();
        let all: Vec<&ComponentStatus> = s.iter().collect();
        let rows = table(&all, true);
        assert_eq!(rows[0], "  installed   Qwen3.8 27B      10.9 GB");
        assert_eq!(
            rows[1],
            "  partial     FLUX.1 schnell   12.8 GB  (2.8 GB of 12.8 GB downloaded)"
        );
        assert_eq!(rows[2], "  missing     FLUX VAE          335 MB");
        assert_eq!(
            rows[3],
            "  on this PC  T5-XXL            5.1 GB  (will reuse /lmstudio/t5.gguf)"
        );
        assert!(table(&all, false)[3].ends_with("(found at /lmstudio/t5.gguf)"));
    }

    #[test]
    fn only_ids() {
        let s = sample();
        assert_eq!(
            check_only(&["image-vae".into(), " llm-model".into()], &s).unwrap(),
            ["image-vae", "llm-model"]
        );
        let e = check_only(&["llama-win-cuda".into()], &s).unwrap_err();
        assert_eq!(super::super::exit_code(&e), 2);
        assert!(e.to_string().contains("llama-win-cuda"));
        assert!(e.to_string().contains("image-diffusion"));
        assert!(check_only(&[], &s).unwrap().is_empty());
    }

    fn progress(id: &str, state: DlState, bytes: u64, speed: u64) -> Progress {
        Progress {
            id: id.into(),
            state,
            bytes,
            total: 0,
            speed_bps: speed,
            error: None,
        }
    }

    #[test]
    fn download_view() {
        let comps = [
            component("a", "Model A", 1_000_000_000),
            component("b", "Model B", 3_000_000_000),
        ];
        let mut v = DownloadView::new(&comps);
        assert_eq!(v.update(&progress("a", DlState::Queued, 0, 0)), None);
        assert_eq!(
            v.update(&progress("a", DlState::Downloading, 0, 0))
                .as_deref(),
            Some("  download   Model A (1.0 GB)")
        );
        assert_eq!(
            v.update(&progress(
                "a",
                DlState::Downloading,
                500_000_000,
                50_000_000
            )),
            None
        );
        assert_eq!(
            v.status_line(),
            "1 active · 500 MB of 4.0 GB (12%) · 50.0 MB/s · 1m 10s left"
        );
        assert_eq!(
            v.update(&progress("a", DlState::Done, 1_000_000_000, 0))
                .as_deref(),
            Some("  done       Model A")
        );
        let mut failed = progress("b", DlState::Failed, 0, 0);
        failed.error = Some("SHA-256 mismatch".into());
        assert_eq!(
            v.update(&failed).as_deref(),
            Some("  failed     Model B: SHA-256 mismatch")
        );
        assert_eq!(v.status_line(), "0 active · 1.0 GB of 4.0 GB (25%)");
        assert_eq!(v.update(&progress("unknown", DlState::Done, 0, 0)), None);
    }

    #[test]
    fn hashing() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("x");
        std::fs::write(&f, b"abc").unwrap();
        let progress = AtomicU64::new(0);
        assert_eq!(
            sha256_file(&f, &progress).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(progress.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn engine_markers() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path());
        let mut c = component("llama-linux-cpu", "llama.cpp", 1);
        c.role = Role::LlmEngine;
        c.sha256 = "ABCD".into();
        c.unpack = Some(jai_core::Unpack::TarGz);
        c.dir = Some("llama".into());
        c.exe = Some("llama-server".into());
        let sel = Selection {
            components: vec![c.clone()],
            llm_backend: Backend::Cpu,
            image_backend: Backend::Cpu,
        };
        assert!(archive_ok(&paths, &sel, &c)
            .unwrap_err()
            .contains(".ok-llama-linux-cpu"));
        let engine = paths.engines().join("llama");
        std::fs::create_dir_all(&engine).unwrap();
        std::fs::write(engine.join(".ok-llama-linux-cpu"), "abcd\n").unwrap();
        assert!(archive_ok(&paths, &sel, &c)
            .unwrap_err()
            .contains("llama-server"));
        // Where the install status finds it too: under a folder of the archive.
        std::fs::create_dir_all(engine.join("bin")).unwrap();
        std::fs::write(engine.join("bin").join("llama-server"), b"").unwrap();
        assert!(archive_ok(&paths, &sel, &c).is_ok());
        let statuses = install::status(&paths, &sel);
        assert_eq!(statuses[0].state, InstallState::Installed);
        std::fs::write(engine.join(".ok-llama-linux-cpu"), "ffff").unwrap();
        assert!(archive_ok(&paths, &sel, &c).is_err());
        // A runtime has no executable: the marker is enough.
        let mut rt = component("llama-linux-cuda-rt", "CUDA runtime", 1);
        rt.role = Role::LlmRuntime;
        rt.sha256 = "ABCD".into();
        rt.dir = Some("llama".into());
        std::fs::write(engine.join(".ok-llama-linux-cuda-rt"), "abcd").unwrap();
        assert!(archive_ok(&paths, &sel, &rt).is_ok());
    }

    #[test]
    fn damaged_models_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("model.gguf");
        std::fs::write(&file, b"bit rot").unwrap();
        let why = remove_damaged(&file);
        assert!(why.contains("removed"), "{why}");
        assert!(!file.exists());
        let why = remove_damaged(&file);
        assert!(why.contains("cannot be removed"), "{why}");
    }

    #[test]
    fn licences() {
        let s = sample();
        let all: Vec<&ComponentStatus> = s.iter().collect();
        assert_eq!(
            licence_lines(&all),
            ["  MIT: Qwen3.8 27B, FLUX.1 schnell, FLUX VAE, T5-XXL"]
        );
        let mut other = s[1].clone();
        other.component.license = "NVIDIA CUDA EULA (redistributable runtime)".into();
        let mixed = [&s[0], &other, &s[2]];
        assert_eq!(
            licence_lines(&mixed),
            [
                "  MIT: Qwen3.8 27B, FLUX VAE",
                "  NVIDIA CUDA EULA (redistributable runtime): FLUX.1 schnell"
            ]
        );
    }

    #[test]
    fn one_download_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path());
        let first = download_lock(&paths).unwrap();
        let e = download_lock(&paths).unwrap_err();
        assert!(e.to_string().contains("another download"), "{e}");
        drop(first);
        assert!(download_lock(&paths).is_ok());
    }
}
