//! The Tauri commands and events of the webview UI (DESIGN.md section 10,
//! "Tauri commands"). Command names are snake_case. JavaScript passes the
//! arguments in camelCase, which Tauri maps to these snake_case parameters.
//! Errors reach JavaScript as the rejection message.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use jai_core::download::{self, DownloadOptions, Progress};
use jai_core::gpu::{self, GpuInfo, GpuVendor};
use jai_core::install::{self, InstallState};
use jai_core::{catalog, token, Backend, BackendPref, Config, Os, Role};
use jai_engines::{EngineKind, SchedulerConfig, SchedulerStatus};
use jai_js::imageops;
use jai_js::{Diag, DocInput, RenderOptions, ValidateResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager, Runtime, State};
use tauri_plugin_dialog::DialogExt as _;
use tokio_util::sync::CancellationToken;

use super::state::{lock, AppState, Shared};
use super::MAIN;
use crate::cli::read_limited;
pub use crate::encoding::EncodeSettings;
use crate::encoding::{self, EncodeJob, Finished, MAX_IMAGE_BYTES};
use crate::gui_support::{self, ProgressThrottle};
use crate::render_cache::{CachedRender, RenderCache};
use crate::{integration, output, settings};

/// A failed command rejects in JavaScript with this message.
pub type CommandResult<T> = Result<T, String>;

/// Event: one component's `jai_core::download::Progress`.
pub const DOWNLOAD_PROGRESS: &str = "download-progress";
/// Event: [`DownloadDone`].
pub const DOWNLOAD_DONE: &str = "download-done";
/// Event: [`EncodeEvent`].
pub const ENCODE_EVENT: &str = "encode-event";
/// Event: [`EncodeDone`].
pub const ENCODE_DONE: &str = "encode-done";
/// Event: [`OpenPath`], a document from the command line, a second launch or
/// the file association.
pub const OPEN_PATH: &str = "open-path";

/// Largest document file the viewer reads.
const MAX_DOC_BYTES: u64 = 64 << 20;
/// The render cache's size tag: the model, the scale and the format.
const RENDER_SIZE_TAG: &str = "flux-schnell@1/png";
/// Long side of the encode thumbnails sent with events.
const THUMB_SIDE: u32 = 384;
/// Long side of `image_preview`.
const PREVIEW_SIDE: u32 = 1024;
/// Download progress reaches the UI at most this often per component.
const PROGRESS_EVERY: Duration = Duration::from_millis(150);
/// Lines of engine log `engine_logs` returns (the whole ring).
const LOG_LINES: usize = 1000;
/// The VC++ redistributable (DESIGN.md section 10).
#[cfg(windows)]
const VC_REDIST_URL: &str = "https://aka.ms/vc14/vc_redist.x64.exe";

fn err(e: impl std::fmt::Display) -> String {
    let s = format!("{e:#}");
    tracing::debug!("command failed: {s}");
    s
}

/// Runs blocking work off the async threads.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> CommandResult<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(err)?
        .map_err(err)
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub version: String,
    pub data_dir: PathBuf,
    pub os: Os,
    pub backend: Backend,
    pub gpu: Option<GpuInfo>,
    pub vc_runtime_ok: bool,
    /// What is wrong with `config.json`, if anything (`Config::load_checked`).
    pub config_problem: Option<String>,
    /// Why the automatic backend did not pick CUDA on this NVIDIA GPU
    /// (`gpu::cuda_unsupported`); None otherwise.
    pub cuda_note: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerDetails {
    pub url: String,
    pub token: String,
    pub host: String,
    pub port: u16,
    pub running: bool,
    /// Why this app's own server is not running, or that another JAI process
    /// (such as `jai serve`) runs the server at `url`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TokenReply {
    pub token: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct StatusReply {
    pub engines: SchedulerStatus,
    pub installed: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsStatus {
    pub components: Vec<ComponentRow>,
    pub total_bytes: u64,
    pub needed_bytes: u64,
    pub disk_free: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentRow {
    pub id: String,
    pub role: Role,
    pub label: String,
    pub size: u64,
    /// `installed`, `partial`, `missing` or `reusable`.
    pub state: String,
    pub partial_bytes: Option<u64>,
    pub reusable_path: Option<PathBuf>,
    /// For a reusable file: true when installing hard-links it (no space
    /// taken), false when it is copied (another disk, or a file other
    /// accounts can change; `install::reuse_links`).
    pub reuse_links: Option<bool>,
    pub license: String,
    pub source: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct DownloadDone {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DocumentInfo {
    pub name: String,
    pub path: PathBuf,
    pub validate: ValidateResult,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewerRender {
    pub data_url: String,
    pub width: u32,
    pub height: u32,
    pub seed: u64,
    pub sent_prompt: String,
    pub description: String,
    pub document_text: String,
    pub diagnostics: Vec<Diag>,
    pub cached: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct PathReply {
    pub path: PathBuf,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DialogKind {
    Jai,
    Image,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImagePreview {
    pub data_url: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EncodeStarted {
    pub job_id: String,
    pub dir: PathBuf,
}

/// One engine event. Render references carry `thumb`, a JPEG data URL at most
/// 384 px on the long side, so the UI never reads files.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EncodeEvent {
    pub job_id: String,
    pub event: Value,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EncodeDone {
    pub job_id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jai_path: Option<PathBuf>,
    pub report_dir: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct OpenPath {
    pub path: PathBuf,
}

#[tauri::command]
pub async fn app_info(state: State<'_, AppState>) -> CommandResult<AppInfo> {
    let pref = state.config().backend;
    let paths = state.paths.clone();
    let (gpu, vc_runtime_ok, config_problem) = blocking(move || {
        Ok((
            gpu::detect(),
            integration::vc_runtime_ok(),
            Config::load_checked(&paths).1,
        ))
    })
    .await?;
    let cuda_note = gpu
        .as_ref()
        .filter(|g| pref == BackendPref::Auto && g.vendor == GpuVendor::Nvidia)
        .and_then(gpu::cuda_unsupported);
    Ok(AppInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        data_dir: state.paths.root.clone(),
        os: catalog::current_os(),
        backend: gpu::effective_backend(pref, gpu.as_ref()),
        gpu,
        vc_runtime_ok,
        config_problem,
        cuda_note,
    })
}

#[tauri::command]
pub async fn server_info(state: State<'_, AppState>) -> CommandResult<ServerDetails> {
    let cfg = state.config();
    let token = state.token();
    if let Some(addr) = state.server_addr().await {
        return Ok(ServerDetails {
            url: settings::addr_url(addr),
            token,
            host: addr.ip().to_string(),
            port: addr.port(),
            running: true,
            note: None,
        });
    }
    // Another JAI process may run the server; renders and encodes use it.
    if let Some((info, token)) = jai_server::discovery::find(&state.paths).await {
        return Ok(ServerDetails {
            url: settings::client_url(&info.host, info.port),
            token,
            host: info.host.clone(),
            port: info.port,
            running: true,
            note: Some(format!(
                "Another JAI process (pid {}) runs this server; JAI Studio starts its own when that one stops.",
                info.pid
            )),
        });
    }
    Ok(ServerDetails {
        url: settings::client_url(&cfg.server.host, cfg.server.port),
        token,
        host: cfg.server.host.clone(),
        port: cfg.server.port,
        running: false,
        note: state.server_error().await,
    })
}

#[tauri::command]
pub async fn regenerate_token(state: State<'_, AppState>) -> CommandResult<TokenReply> {
    let _serial = state.settings_lock.lock().await;
    // An encode keeps the token it started with for its whole run.
    if !lock(&state.jobs).is_empty() {
        return Err(
            "An encode is running and uses the current token: make a new one when it is done, \
             or cancel it first."
                .into(),
        );
    }
    // A server of another process keeps its token in memory; changing only
    // the file would lock its clients out.
    if state.server_addr().await.is_none() {
        if let Some((info, _)) = jai_server::discovery::find(&state.paths).await {
            return Err(format!(
                "Another JAI process (pid {}) runs the server; stop it before regenerating the token.",
                info.pid
            ));
        }
    }
    let paths = state.paths.clone();
    let token = blocking(move || token::regenerate_token(&paths)).await?;
    state.set_token(token.clone());
    Ok(TokenReply { token })
}

/// The engines' state: this app's own, or those of the other JAI process
/// whose server it uses.
#[tauri::command]
pub async fn status(state: State<'_, AppState>) -> CommandResult<StatusReply> {
    if let Some(remote) = state.remote_engines().await {
        return Ok(StatusReply {
            engines: remote.engines,
            installed: remote.installed,
        });
    }
    let core = state.core();
    let engines = core.scheduler.status();
    let paths = state.paths.clone();
    let installed = blocking(move || Ok(install::ready(&paths, &core.selection))).await?;
    Ok(StatusReply { engines, installed })
}

#[tauri::command]
pub async fn models_status(state: State<'_, AppState>) -> CommandResult<ModelsStatus> {
    let core = state.core();
    let paths = state.paths.clone();
    blocking(move || {
        let statuses = install::status(&paths, &core.selection);
        let components = statuses
            .iter()
            .map(|s| {
                let (state, partial_bytes, reusable_path, reuse_links) = match &s.state {
                    InstallState::Installed => ("installed", None, None, None),
                    InstallState::Partial { bytes } => ("partial", Some(*bytes), None, None),
                    InstallState::Missing => ("missing", None, None, None),
                    InstallState::Reusable { path } => (
                        "reusable",
                        None,
                        Some(PathBuf::from(output::display_path(path))),
                        Some(install::reuse_links(&paths, path)),
                    ),
                };
                let c = &s.component;
                ComponentRow {
                    id: c.id.clone(),
                    role: c.role,
                    label: c.label.clone(),
                    size: c.size,
                    state: state.to_string(),
                    partial_bytes,
                    reusable_path,
                    reuse_links,
                    license: c.license.clone(),
                    source: c.source.clone(),
                }
            })
            .collect();
        Ok(ModelsStatus {
            components,
            total_bytes: core.selection.components.iter().map(|c| c.size).sum(),
            needed_bytes: download::bytes_needed(&paths, &core.selection),
            disk_free: download::disk_free(&paths),
        })
    })
    .await
}

/// Emits `download-progress` and, at the end, `download-done`.
#[tauri::command]
pub async fn download_start<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, AppState>,
    reuse: bool,
) -> CommandResult<()> {
    let shared = state.shared();
    let cancel = {
        let mut slot = lock(&shared.download);
        if slot.is_some() {
            return Err("a download is already running".into());
        }
        let token = CancellationToken::new();
        *slot = Some(token.clone());
        token
    };
    let file_lock = match crate::cli::models::download_lock(&shared.paths) {
        Ok(l) => l,
        Err(e) => {
            lock(&shared.download).take();
            return Err(err(e));
        }
    };
    let components = shared.core().selection.components.clone();
    tauri::async_runtime::spawn(async move {
        let _file_lock = file_lock;
        let throttle = Mutex::new(ProgressThrottle::new(PROGRESS_EVERY));
        let progress_app = app.clone();
        let progress = move |p: Progress| {
            if lock(&throttle).pass(&p.id, p.state, Instant::now()) {
                let _ = progress_app.emit(DOWNLOAD_PROGRESS, &p);
            }
        };
        let opts = DownloadOptions {
            reuse,
            concurrency: 3,
        };
        let result =
            download::install(&shared.paths, &components, opts, progress, cancel.clone()).await;
        lock(&shared.download).take();
        let done = match result {
            Ok(()) => DownloadDone {
                ok: true,
                error: None,
            },
            Err(_) if cancel.is_cancelled() => DownloadDone {
                ok: false,
                error: Some("canceled".into()),
            },
            Err(e) => {
                tracing::warn!("download failed: {e:#}");
                DownloadDone {
                    ok: false,
                    error: Some(format!("{e:#}")),
                }
            }
        };
        if !super::window_visible(&app) {
            match &done.error {
                None => super::notify(&app, "Models downloaded", "JAI Studio is ready to use."),
                Some(e) if e != "canceled" => super::notify(&app, "The download failed", e),
                Some(_) => {}
            }
        }
        let _ = app.emit(DOWNLOAD_DONE, done);
    });
    Ok(())
}

#[tauri::command]
pub async fn download_cancel(state: State<'_, AppState>) -> CommandResult<()> {
    if let Some(token) = lock(&state.download).as_ref() {
        token.cancel();
    }
    Ok(())
}

/// Why the models cannot be managed from here.
pub fn remote_engines_message(pid: u32) -> String {
    format!(
        "The models run in another JAI process (pid {pid}, such as `jai serve`): \
         stop it there to free the GPU."
    )
}

#[tauri::command]
pub async fn unload_models(state: State<'_, AppState>) -> CommandResult<()> {
    if let Some(remote) = state.remote_engines().await {
        return Err(remote_engines_message(remote.pid));
    }
    state.core().scheduler.unload().await;
    Ok(())
}

/// The engine's recent output: from this app's scheduler, or, when another
/// JAI process runs the engines, from the log file they share
/// (`logs/llm.log` or `logs/image.log`).
#[tauri::command]
pub async fn engine_logs(
    state: State<'_, AppState>,
    kind: EngineKind,
) -> CommandResult<Vec<String>> {
    if state.remote_engines().await.is_some() {
        let file = state.paths.logs().join(match kind {
            EngineKind::Llm => "llm.log",
            EngineKind::Image => "image.log",
        });
        return blocking(move || Ok(gui_support::tail_lines(&file, LOG_LINES)?)).await;
    }
    Ok(state.core().scheduler.logs(kind, LOG_LINES))
}

/// The settings as `config.json` holds them: `JAI_PORT` only overrides the
/// port of this run, so the form never shows (and never saves) it.
#[tauri::command]
pub async fn settings_get(state: State<'_, AppState>) -> CommandResult<Config> {
    let paths = state.paths.clone();
    blocking(move || Ok(Config::load(&paths))).await
}

/// Saves the settings form and applies it: autostart, file association and
/// the idle-unload time at once; a port or host change restarts the server,
/// and an engine change (backend, llm, image) a new core, which stops the
/// loaded engine. Both restarts are refused while an encode runs or the models
/// work on requests, which they would cut off; then nothing is saved. A port
/// left as stored keeps the port in use (`settings::applied`). The engines'
/// extra arguments are never taken from the webview (`settings::from_form`).
#[tauri::command]
pub async fn settings_set(state: State<'_, AppState>, config: Config) -> CommandResult<Config> {
    let shared = state.shared();
    let _serial = shared.settings_lock.lock().await;
    let old = shared.config();
    let stored = {
        let paths = shared.paths.clone();
        blocking(move || Ok(Config::load(&paths))).await?
    };
    // The engines' extra arguments stay as config.json holds them.
    let config = settings::from_form(config, &stored);
    settings::validate(&config)?;
    let next = settings::applied(&config, &stored, &old);
    let restart_core = settings::engines_changed(&old, &next);
    let restart_server = settings::server_changed(&old, &next);
    if restart_core || restart_server {
        if let Some(why) = shared.busy() {
            return Err(format!(
                "Not saved: {why}. A new port, backend or engine setting restarts the server and \
                 the models; save it when that is done, or cancel the encode first."
            ));
        }
    }
    {
        let (paths, cfg) = (shared.paths.clone(), config.clone());
        blocking(move || cfg.save(&paths)).await?;
    }
    let mut problems = Vec::new();
    let (login, assoc) = (config.app.start_at_login, config.app.file_association);
    if login || login != old.app.start_at_login {
        if let Err(e) = blocking(move || integration::set_start_at_login(login)).await {
            problems.push(format!("start at login: {e}"));
        }
    }
    if assoc || assoc != old.app.file_association {
        if let Err(e) = blocking(move || integration::set_file_association(assoc)).await {
            problems.push(format!("the file association: {e}"));
        }
    }
    if restart_core {
        shared.replace_core(next).await.map_err(err)?;
    } else {
        if next.idle_unload_minutes != old.idle_unload_minutes {
            let idle = SchedulerConfig::from_config(&next).idle_unload;
            shared.core().scheduler.set_idle_unload(idle);
        }
        shared.set_config(next);
        if restart_server {
            shared.restart_server().await.map_err(err)?;
        }
    }
    if problems.is_empty() {
        Ok(config)
    } else {
        Err(format!(
            "The settings are saved, but {}",
            problems.join("; ")
        ))
    }
}

/// How `install_vc_runtime` ended.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VcInstallReply {
    /// `declined`: the user said no to Studio's question or to Windows'
    /// permission prompt. `finished`: the installer ran and exited with
    /// `exit_code` (0 installed, 3010 installed but Windows must restart,
    /// 1638 another version is installed, 1602 canceled in the installer).
    /// `started`: it started, but there was no process to wait for.
    pub outcome: &'static str,
    pub exit_code: Option<u32>,
}

/// Windows only: downloads https://aka.ms/vc14/vc_redist.x64.exe, runs it
/// after the user confirms, once its Authenticode signature checks out as
/// Microsoft's, and waits for the installer to exit.
#[tauri::command]
pub async fn install_vc_runtime<R: Runtime>(app: AppHandle<R>) -> CommandResult<VcInstallReply> {
    #[cfg(windows)]
    {
        use crate::win::ShellRun;
        let question = "JAI Studio will download the Microsoft Visual C++ Redistributable (x64) \
                        from aka.ms and start its installer. Windows asks for permission before it installs.";
        let reply = |outcome, exit_code| VcInstallReply { outcome, exit_code };
        if !confirm(
            &app,
            "Install the Visual C++ runtime",
            question,
            "Download and install",
        )
        .await
        {
            return Ok(reply("declined", None));
        }
        let path = download_vc_redist().await.map_err(err)?;
        let run = blocking(move || crate::win::run_microsoft_installer(&path)).await?;
        tracing::info!("VC++ installer: {run:?}");
        Ok(match run {
            ShellRun::Declined => reply("declined", None),
            ShellRun::Exited(code) => reply("finished", Some(code)),
            ShellRun::Untracked => reply("started", None),
        })
    }
    #[cfg(not(windows))]
    {
        let _ = app;
        Err("The Visual C++ runtime is only needed on Windows.".into())
    }
}

/// An OK/Cancel question; false when the user cancels or closes it.
#[cfg(windows)]
async fn confirm<R: Runtime>(app: &AppHandle<R>, title: &str, message: &str, ok: &str) -> bool {
    use tauri_plugin_dialog::{MessageDialogButtons, MessageDialogKind};
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut dialog = app
        .dialog()
        .message(message)
        .title(title)
        .kind(MessageDialogKind::Info)
        .buttons(MessageDialogButtons::OkCancelCustom(
            ok.into(),
            "Cancel".into(),
        ));
    if let Some(window) = app.get_webview_window(MAIN) {
        dialog = dialog.parent(&window);
    }
    dialog.show(move |yes| {
        let _ = tx.send(yes);
    });
    rx.await.unwrap_or(false)
}

/// Downloads the redistributable over HTTPS only (redirects included),
/// refusing anything that is not served by Microsoft or is not a Windows
/// executable, into a new file in a new folder of its own with a random name
/// under the user's temp folder. Its signature is checked before it runs
/// (`win::run_microsoft_installer`).
#[cfg(windows)]
async fn download_vc_redist() -> anyhow::Result<PathBuf> {
    let client = reqwest::Client::builder()
        .https_only(true)
        .user_agent(concat!("jai-studio/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(600))
        .build()?;
    let resp = client
        .get(VC_REDIST_URL)
        .send()
        .await
        .context("cannot download the VC++ runtime")?
        .error_for_status()
        .context("cannot download the VC++ runtime")?;
    let url = resp.url();
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    if url.scheme() != "https" || (host != "microsoft.com" && !host.ends_with(".microsoft.com")) {
        anyhow::bail!("the download was redirected to {url}, which is not Microsoft over HTTPS");
    }
    let bytes = resp.bytes().await.context("the download broke off")?;
    if bytes.len() < 1_000_000 || !bytes.starts_with(b"MZ") {
        anyhow::bail!("the download is not the installer");
    }
    tokio::task::spawn_blocking(move || -> anyhow::Result<PathBuf> {
        use std::io::Write as _;
        let dir = tempfile::Builder::new()
            .prefix("jai-vcredist-")
            .tempdir()
            .context("cannot make a folder for the installer")?
            .keep();
        let path = dir.join("vc_redist.x64.exe");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("cannot create {}", path.display()))?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .with_context(|| format!("cannot write {}", path.display()))?;
        Ok(path)
    })
    .await
    .context("the installer task failed")?
}

#[tauri::command]
pub async fn read_document(path: PathBuf) -> CommandResult<DocumentInfo> {
    let file = path.clone();
    let validate = blocking(move || {
        let bytes = read_limited(&file, MAX_DOC_BYTES)?;
        jai_js::validate(DocInput::Bytes(bytes)).map_err(output::js_error)
    })
    .await?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(DocumentInfo {
        name,
        path,
        validate,
    })
}

#[tauri::command]
pub async fn viewer_render(
    state: State<'_, AppState>,
    path: Option<PathBuf>,
    text: Option<String>,
    seed: Option<String>,
) -> CommandResult<ViewerRender> {
    if let Some(s) = &seed {
        crate::cli::render::check_seed(s).map_err(err)?;
    }
    let input = match (path, text) {
        (Some(p), _) => blocking(move || read_limited(&p, MAX_DOC_BYTES))
            .await
            .map(DocInput::Bytes)?,
        (None, Some(t)) => DocInput::Text(t),
        (None, None) => return Err("give the path or the text of a document".into()),
    };
    let check_input = input.clone();
    let validated =
        blocking(move || jai_js::validate(check_input).map_err(output::js_error)).await?;
    if validated.fatal {
        return Err(gui_support::fatal_message(&validated));
    }
    let key = RenderCache::key(
        &gui_support::input_identity(&validated, &input),
        seed.as_deref(),
        RENDER_SIZE_TAG,
    );
    let cache_dir = state.paths.render_cache();
    let lookup = (cache_dir.clone(), key.clone());
    if let Some((bytes, meta)) =
        blocking(move || Ok(RenderCache::new(lookup.0).get(&lookup.1))).await?
    {
        return Ok(viewer_reply(&bytes, meta, true));
    }
    let server = state.server_ref().await.map_err(err)?;
    let out = jai_js::render(
        input,
        RenderOptions {
            server,
            seed,
            model: None,
            output_format: Some("png".into()),
            scale: None,
        },
    )
    .await
    .map_err(|e| err(output::js_error(e)))?;
    let meta = CachedRender {
        mime: out.mime,
        width: out.width,
        height: out.height,
        seed: out.seed,
        sent_prompt: out.sent_prompt,
        description: out.description,
        document_text: out.document_text,
        diagnostics: out.diagnostics,
    };
    let reply = viewer_reply(&out.bytes, meta.clone(), false);
    let bytes = out.bytes;
    tokio::task::spawn_blocking(move || {
        if let Err(e) = RenderCache::new(cache_dir).put(&key, &bytes, &meta) {
            tracing::warn!("cannot cache the render: {e}");
        }
    });
    Ok(reply)
}

fn viewer_reply(bytes: &[u8], meta: CachedRender, cached: bool) -> ViewerRender {
    ViewerRender {
        data_url: gui_support::data_url(&meta.mime, bytes),
        width: meta.width,
        height: meta.height,
        seed: meta.seed,
        sent_prompt: meta.sent_prompt,
        description: meta.description,
        document_text: meta.document_text,
        diagnostics: meta.diagnostics,
        cached,
    }
}

/// Through a save dialog; `None` when the user cancels.
#[tauri::command]
pub async fn save_file<R: Runtime>(
    app: AppHandle<R>,
    data_url: Option<String>,
    text: Option<String>,
    bytes_b64: Option<String>,
    suggested_name: String,
) -> CommandResult<Option<PathReply>> {
    let bytes =
        gui_support::save_payload(data_url.as_deref(), text.as_deref(), bytes_b64.as_deref())
            .map_err(err)?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut dialog = app.dialog().file().set_file_name(&suggested_name);
    if let Some((name, extensions)) = gui_support::save_filter(&suggested_name) {
        dialog = dialog.add_filter(name, extensions);
    }
    if let Some(window) = app.get_webview_window(MAIN) {
        dialog = dialog.set_parent(&window);
    }
    dialog.save_file(move |path| {
        let _ = tx.send(path);
    });
    let Some(chosen) = rx.await.map_err(err)? else {
        return Ok(None);
    };
    let path = chosen.into_path().map_err(err)?;
    tokio::fs::write(&path, bytes)
        .await
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(Some(PathReply { path }))
}

/// `None` when the user cancels.
#[tauri::command]
pub async fn open_dialog<R: Runtime>(
    app: AppHandle<R>,
    kind: DialogKind,
) -> CommandResult<Option<PathReply>> {
    let (name, extensions) = gui_support::open_filter(matches!(kind, DialogKind::Image));
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut dialog = app.dialog().file().add_filter(name, extensions);
    if let Some(window) = app.get_webview_window(MAIN) {
        dialog = dialog.set_parent(&window);
    }
    dialog.pick_file(move |path| {
        let _ = tx.send(path);
    });
    let Some(chosen) = rx.await.map_err(err)? else {
        return Ok(None);
    };
    Ok(Some(PathReply {
        path: chosen.into_path().map_err(err)?,
    }))
}

/// A JPEG data URL at most 1024 px on the long side.
#[tauri::command]
pub async fn image_preview(path: PathBuf) -> CommandResult<ImagePreview> {
    blocking(move || {
        let bytes = read_limited(&path, MAX_IMAGE_BYTES)?;
        let jpeg = imageops::preview_jpeg(&bytes, PREVIEW_SIDE)
            .with_context(|| format!("cannot read {} as an image", path.display()))?;
        let (width, height) =
            gui_support::jpeg_size(&jpeg).context("the preview is not a readable JPEG")?;
        Ok(ImagePreview {
            data_url: gui_support::data_url("image/jpeg", &jpeg),
            width,
            height,
        })
    })
    .await
}

/// Emits `encode-event` and, at the end, `encode-done`.
#[tauri::command]
pub async fn encode_start<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, AppState>,
    path: PathBuf,
    options: EncodeSettings,
) -> CommandResult<EncodeStarted> {
    options.check()?;
    if !path.is_file() {
        return Err(format!("{} does not exist", path.display()));
    }
    let shared = state.shared();
    // Not while settings restart the server: the job takes its address.
    let serial = shared.settings_lock.lock().await;
    let job_id = format!("job-{}", shared.next_job.fetch_add(1, Ordering::SeqCst) + 1);
    let dir = job_dir(&shared, &path).map_err(err)?;
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let job = EncodeJob {
        out: dir.join(format!("{}.jai", encoding::safe_name(&stem))),
        report_dir: dir.clone(),
        image: path,
        settings: options,
    };
    let cancel = CancellationToken::new();
    lock(&shared.jobs).insert(job_id.clone(), cancel.clone());
    drop(serial);
    tauri::async_runtime::spawn(run_encode_job(app, shared, job_id.clone(), job, cancel));
    Ok(EncodeStarted { job_id, dir })
}

/// A new `<root>/encodings/<yyyymmdd-hhmmss>-<name>/` folder.
fn job_dir(shared: &Shared, image: &Path) -> anyhow::Result<PathBuf> {
    let base = encoding::job_dir_name(&output::local_stamp(), image);
    let root = shared.paths.encodings();
    std::fs::create_dir_all(&root).with_context(|| format!("cannot create {}", root.display()))?;
    for i in 1..1000 {
        let dir = if i == 1 {
            root.join(&base)
        } else {
            root.join(format!("{base}-{i}"))
        };
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("cannot create {}", dir.display())),
        }
    }
    anyhow::bail!("too many encodings named {base}")
}

/// A thumbnail data URL for a render file, cached by name.
fn thumb_for(dir: &Path, file: &str, cache: &mut HashMap<String, String>) -> Option<String> {
    if !encoding::render_file_ok(file) {
        return None;
    }
    if let Some(t) = cache.get(file) {
        return Some(t.clone());
    }
    let bytes = std::fs::read(dir.join(file)).ok()?;
    let jpeg = imageops::preview_jpeg(&bytes, THUMB_SIDE).ok()?;
    let url = gui_support::data_url("image/jpeg", &jpeg);
    cache.insert(file.to_string(), url.clone());
    Some(url)
}

/// Runs one encode job: events go out in order with thumbnails added, then
/// `encode-done`.
async fn run_encode_job<R: Runtime>(
    app: AppHandle<R>,
    shared: Arc<Shared>,
    job_id: String,
    job: EncodeJob,
    cancel: CancellationToken,
) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let emitter = {
        let app = app.clone();
        let job_id = job_id.clone();
        let render_dir = job.render_dir();
        tauri::async_runtime::spawn(async move {
            let mut cache = HashMap::new();
            while let Some(event) = rx.recv().await {
                let dir = render_dir.clone();
                let done = tokio::task::spawn_blocking(move || {
                    let mut event = event;
                    let mut cache = cache;
                    encoding::add_thumbs(&mut event, &mut |f| thumb_for(&dir, f, &mut cache));
                    (event, cache)
                })
                .await;
                let Ok((event, back)) = done else {
                    break;
                };
                cache = back;
                let _ = app.emit(
                    ENCODE_EVENT,
                    EncodeEvent {
                        job_id: job_id.clone(),
                        event,
                    },
                );
            }
        })
    };
    let sender = Arc::new(Mutex::new(Some(tx)));
    let on_event: Arc<dyn Fn(Value) + Send + Sync> = {
        let sender = sender.clone();
        Arc::new(move |e: Value| {
            if let Some(tx) = lock(&sender).as_ref() {
                let _ = tx.send(e);
            }
        })
    };
    let result = async {
        let target = encoding::prepare(&job).await?;
        let server = shared.server_ref().await?;
        encoding::run(&job, target, server, on_event, cancel.clone()).await
    }
    .await;
    // Close the channel and let every event out before encode-done.
    lock(&sender).take();
    let _ = emitter.await;
    lock(&shared.jobs).remove(&job_id);
    let done = match result {
        Ok(Finished {
            output,
            written: Ok(()),
        }) => EncodeDone {
            job_id,
            ok: true,
            // Why a run with a result stopped early, if it did.
            error: output.error.or_else(|| {
                output
                    .report
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            }),
            text: Some(output.text),
            jai_path: Some(job.out.clone()),
            report_dir: job.report_dir.clone(),
            stop_reason: Some(output.stop_reason),
        },
        // The text stays with the job even when its file could not be written.
        Ok(Finished {
            output,
            written: Err(e),
        }) => {
            tracing::warn!("cannot save the encoding: {e:#}");
            EncodeDone {
                job_id,
                ok: false,
                text: Some(output.text),
                jai_path: None,
                report_dir: job.report_dir.clone(),
                error: Some(format!("{e:#}")),
                stop_reason: Some(output.stop_reason),
            }
        }
        Err(e) => {
            let canceled = cancel.is_cancelled();
            if !canceled {
                tracing::warn!("encoding failed: {e:#}");
            }
            EncodeDone {
                job_id,
                ok: false,
                text: None,
                jai_path: None,
                report_dir: job.report_dir.clone(),
                error: Some(if canceled {
                    "canceled".into()
                } else {
                    format!("{e:#}")
                }),
                stop_reason: canceled.then(|| "canceled".to_string()),
            }
        }
    };
    if !super::window_visible(&app) && !shared.quitting.load(Ordering::SeqCst) {
        match (done.ok, &done.error) {
            (true, _) => super::notify(
                &app,
                "Encoding finished",
                &format!("Saved {}", job.out.display()),
            ),
            (false, Some(e)) if e != "canceled" => super::notify(&app, "Encoding failed", e),
            (false, _) => {}
        }
    }
    let _ = app.emit(ENCODE_DONE, done);
}

#[tauri::command]
pub async fn encode_cancel(state: State<'_, AppState>, job_id: String) -> CommandResult<()> {
    if let Some(token) = lock(&state.jobs).get(&job_id) {
        token.cancel();
    }
    Ok(())
}

/// Opens the folder in Explorer or the file manager.
#[tauri::command]
pub async fn reveal(path: PathBuf) -> CommandResult<()> {
    blocking(move || integration::reveal(&path)).await
}

/// The page has registered its `open-path` listener: the documents that
/// waited for it are sent now (see `gui_support::LaunchGate`).
#[tauri::command]
pub async fn ui_ready<R: Runtime>(app: AppHandle<R>) -> CommandResult<()> {
    super::page_ready(&app);
    Ok(())
}

/// The licences of the open-source software in JAI Studio, as `jai
/// licenses` prints them.
#[tauri::command]
pub async fn third_party_notices() -> CommandResult<String> {
    Ok(crate::THIRD_PARTY_NOTICES.to_string())
}

/// Every command above, for `tauri::Builder::invoke_handler`.
pub fn handler<R: Runtime>() -> impl Fn(tauri::ipc::Invoke<R>) -> bool + Send + Sync + 'static {
    tauri::generate_handler![
        app_info,
        server_info,
        regenerate_token,
        status,
        models_status,
        download_start,
        download_cancel,
        unload_models,
        engine_logs,
        settings_get,
        settings_set,
        install_vc_runtime,
        read_document,
        viewer_render,
        save_file,
        open_dialog,
        image_preview,
        encode_start,
        encode_cancel,
        reveal,
        ui_ready,
        third_party_notices,
    ]
}
