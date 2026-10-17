//! The desktop app (DESIGN.md section 10, "GUI (Tauri 2.12)"): the window, the
//! tray, single instance, the in-process server, and the Tauri commands the
//! webview UI calls. The main window's label is `main` (capabilities/default.json).
//!
//! Every way out (Quit in the tray, closing the window without close-to-tray,
//! Ctrl+C, an exit request) goes through [`begin_quit`]: it hides the window,
//! shuts the engines and the server down, and only then lets the app exit.
//!
//! Second launches, and the first launch's own document, go through a
//! [`LaunchGate`]: nothing touches windows before setup has finished, and
//! documents reach the page once it listens for them (`ui_ready`).

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Context as _;
use jai_core::Paths;
use jai_server::Core;
use tauri::webview::PageLoadEvent;
use tauri::{
    AppHandle, Emitter, Manager, RunEvent, Runtime, WebviewUrl, WebviewWindow, WindowEvent,
};

use crate::gui_support::{forwarded, LaunchGate};
use crate::{settings, GuiMode};

mod commands;
mod state;
mod tray;

use commands::{OpenPath, OPEN_PATH};
use state::{AppState, Shared};

/// The main window's label.
pub const MAIN: &str = "main";

/// Longest wait for the engines and the server to stop on quit.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);

/// How often the app tries to start its server while it cannot (another
/// process holds the GPU, or the ports are taken).
const SERVER_RETRY: Duration = Duration::from_secs(5);

/// How long after a page load documents are sent anyway to a page that never
/// calls `ui_ready` (its `open-path` listener is registered first thing).
const UI_READY_FALLBACK: Duration = Duration::from_secs(1);

/// Runs the GUI until Quit and returns the exit code.
/// tauri-plugin-single-instance is registered first, so a second launch hands
/// its arguments to the first instance and exits.
pub fn run(mode: GuiMode) -> anyhow::Result<u8> {
    // WebKitGTK has blank-window bugs on NVIDIA with the DMA-BUF renderer;
    // this must be set before the webview starts (and before any thread).
    #[cfg(target_os = "linux")]
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }
    let paths = crate::cli::paths()?;
    crate::logging::init_file(&paths);
    tracing::info!(
        "JAI Studio {} starting ({mode:?})",
        env!("CARGO_PKG_VERSION")
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("jai")
        .build()
        .context("cannot start the async runtime")?;
    tauri::async_runtime::set(runtime.handle().clone());

    let (tray_mode, open) = match mode {
        GuiMode::Tray => (true, None),
        GuiMode::Window { open } => (false, open.map(|p| absolute(&p))),
    };
    let shared_slot: Arc<OnceLock<Arc<Shared>>> = Arc::default();
    let setup_slot = shared_slot.clone();
    let handle = runtime.handle().clone();

    let app = tauri::Builder::default()
        // Managed before any plugin can call back, so second launches that
        // arrive during setup are held, not acted on.
        .manage(LaunchGate::new(open))
        .plugin(tauri_plugin_single_instance::init(|app, args, cwd| {
            on_second_launch(app, &args, Path::new(&cwd));
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .invoke_handler(commands::handler())
        .on_page_load(|webview, payload| {
            if webview.label() == MAIN {
                on_page_load(webview.app_handle(), payload.event());
            }
        })
        .on_window_event(|window, event| {
            if window.label() == MAIN {
                if let WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    on_close_requested(window.app_handle());
                }
            }
        })
        .setup(move |app| {
            let _enter = handle.enter();
            setup(app.handle(), paths, tray_mode, &setup_slot);
            Ok(())
        })
        .build(tauri::generate_context!())
        .context("cannot start the desktop app")?;

    let code = app.run_return(|app, event| {
        if let RunEvent::ExitRequested { api, .. } = event {
            if let Some(state) = app.try_state::<AppState>() {
                if !state.shutdown_done.load(Ordering::SeqCst) {
                    api.prevent_exit();
                    begin_quit(app);
                }
            }
        }
    });

    // The shutdown normally ran before the event loop ended; this covers an
    // event loop that ended some other way.
    if let Some(shared) = shared_slot.get() {
        if !shared.shutdown_done.load(Ordering::SeqCst) {
            runtime.block_on(async {
                if tokio::time::timeout(SHUTDOWN_TIMEOUT, shared.shutdown())
                    .await
                    .is_err()
                {
                    tracing::warn!("the shutdown timed out");
                }
            });
        }
    }
    runtime.shutdown_timeout(Duration::from_secs(2));
    Ok(u8::try_from(code).unwrap_or(1))
}

/// A path made absolute against the current folder.
fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Creates the core, the state, the tray, the window and the server, then
/// lets held launches act.
fn setup<R: Runtime>(
    app: &AppHandle<R>,
    paths: Paths,
    tray_mode: bool,
    slot: &OnceLock<Arc<Shared>>,
) {
    // Only the primary instance gets here: it may rotate jai.log.
    crate::logging::allow_rotation();
    #[cfg(windows)]
    register_notifications(&paths);
    let cfg = settings::load_config(&paths);
    let core = match Core::new(paths.clone(), cfg) {
        Ok(core) => core,
        Err(e) => return fatal(app, &format!("{e:#}")),
    };
    let http = match crate::cli::local_http() {
        Ok(http) => http,
        Err(e) => return fatal(app, &format!("{e:#}")),
    };
    let shared = Shared::new(paths, core, http);
    let _ = slot.set(shared.clone());
    app.manage(AppState::new(shared.clone()));

    if tray::available() {
        match tray::create(app, shared.clone()) {
            Ok(()) => shared.has_tray.store(true, Ordering::SeqCst),
            Err(e) => tracing::warn!("no tray icon: {e}"),
        }
    } else {
        tracing::warn!("no tray icon: libayatana-appindicator3 is not installed");
    }

    let window = match app.get_webview_window(MAIN) {
        Some(window) => Ok(window),
        None => create_main_window(app, !tray_mode),
    };
    if let Err(e) = window {
        return fatal(
            app,
            &format!("cannot open the window: {e}. JAI Studio needs the Microsoft Edge WebView2 Runtime on Windows, or WebKitGTK 4.1 on Linux."),
        );
    }
    // The window exists: launches that came during setup may act now.
    if app.state::<LaunchGate>().setup_finished() {
        show_main(app);
    }

    // The server, retried while another process holds the GPU or the ports
    // are taken.
    {
        let shared = shared.clone();
        tauri::async_runtime::spawn(async move {
            shared.ensure_server().await;
            let mut tick = tokio::time::interval(SERVER_RETRY);
            tick.tick().await;
            loop {
                tick.tick().await;
                if shared.quitting.load(Ordering::SeqCst) {
                    break;
                }
                shared.ensure_server().await;
            }
        });
    }

    // Keep the registry or desktop entries pointing at this exe.
    let app_cfg = shared.config().app;
    tauri::async_runtime::spawn_blocking(move || {
        if app_cfg.start_at_login {
            if let Err(e) = crate::integration::set_start_at_login(true) {
                tracing::warn!("cannot refresh start at login: {e:#}");
            }
        }
        if app_cfg.file_association {
            if let Err(e) = crate::integration::set_file_association(true) {
                tracing::warn!("cannot refresh the file association: {e:#}");
            }
        }
    });

    // Ctrl+C in the terminal that started the app (Linux) quits cleanly.
    let quit_app = app.clone();
    tauri::async_runtime::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            begin_quit(&quit_app);
        }
    });
}

/// Registers the AppUserModelID the notifications are shown under, with the
/// JAI icon (written to `cache/notification-icon.png`), off the main thread.
/// Failures are logged: the app works without notifications.
#[cfg(windows)]
fn register_notifications(paths: &Paths) {
    const ICON: &[u8] = include_bytes!("../../icons/128x128.png");
    let icon = paths.root.join("cache").join("notification-icon.png");
    tauri::async_runtime::spawn_blocking(move || {
        let fresh = std::fs::read(&icon).is_ok_and(|b| b == ICON);
        if !fresh {
            if let Some(dir) = icon.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Err(e) = std::fs::write(&icon, ICON) {
                tracing::warn!("cannot write {}: {e}", icon.display());
            }
        }
        if let Err(e) = crate::win::register_app_id(&icon) {
            tracing::warn!("notifications may not show: {e:#}");
        }
    });
}

/// A failure that leaves nothing to show: say why and exit.
fn fatal<R: Runtime>(app: &AppHandle<R>, message: &str) {
    tracing::error!("cannot start: {message}");
    errln!("jai: {message}");
    #[cfg(windows)]
    crate::win::error_box(
        "JAI Studio",
        &format!("JAI Studio cannot start.\n\n{message}"),
    );
    if let Some(state) = app.try_state::<AppState>() {
        state.shutdown_done.store(true, Ordering::SeqCst);
    }
    app.exit(1);
}

/// The one main window, created hidden in tray mode.
fn create_main_window<R: Runtime>(
    app: &AppHandle<R>,
    visible: bool,
) -> tauri::Result<WebviewWindow<R>> {
    tauri::WebviewWindowBuilder::new(app, MAIN, WebviewUrl::App("index.html".into()))
        .title("JAI Studio")
        .inner_size(1180.0, 800.0)
        .min_inner_size(760.0, 560.0)
        .center()
        .visible(visible)
        .build()
}

/// Shows and focuses the window, creating it if it is gone. Never creates
/// one before setup has finished: setup creates it, and a second creation
/// from inside that one (a callback while it pumps messages) would re-enter
/// window creation.
pub fn show_main<R: Runtime>(app: &AppHandle<R>) {
    let window = match app.get_webview_window(MAIN) {
        Some(w) => w,
        None if !app
            .try_state::<LaunchGate>()
            .is_some_and(|g| g.is_setup_done()) =>
        {
            return;
        }
        None => match create_main_window(app, true) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("cannot open the window: {e}");
                return;
            }
        },
    };
    let _ = window.unminimize();
    let _ = window.show();
    let _ = window.set_focus();
}

/// Whether the window is on screen.
pub fn window_visible<R: Runtime>(app: &AppHandle<R>) -> bool {
    app.get_webview_window(MAIN)
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false)
}

/// A desktop notification; failures are logged.
pub fn notify<R: Runtime>(app: &AppHandle<R>, title: &str, body: &str) {
    use tauri_plugin_notification::NotificationExt as _;
    if let Err(e) = app.notification().builder().title(title).body(body).show() {
        tracing::warn!("cannot show the notification {title:?}: {e}");
    }
}

/// A second launch, passed on by tauri-plugin-single-instance from its
/// window procedure, possibly in the middle of setup: the gate decides what
/// may happen now.
fn on_second_launch<R: Runtime>(app: &AppHandle<R>, args: &[String], cwd: &Path) {
    if app
        .try_state::<AppState>()
        .is_some_and(|s| s.quitting.load(Ordering::SeqCst))
    {
        return;
    }
    let Some(gate) = app.try_state::<LaunchGate>() else {
        return;
    };
    let action = gate.launch(forwarded(args, cwd));
    if action.show {
        show_main(app);
    }
    for path in action.open {
        emit_open(app, path);
    }
}

/// A page load in the main window: documents wait while it loads; a page
/// that never calls `ui_ready` gets them a moment after it has loaded.
fn on_page_load<R: Runtime>(app: &AppHandle<R>, event: PageLoadEvent) {
    let Some(gate) = app.try_state::<LaunchGate>() else {
        return;
    };
    match event {
        PageLoadEvent::Started => {
            gate.page_loading();
        }
        PageLoadEvent::Finished => {
            let load = gate.current_load();
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(UI_READY_FALLBACK).await;
                if let Some(gate) = app.try_state::<LaunchGate>() {
                    for path in gate.page_loaded(load) {
                        emit_open(&app, path);
                    }
                }
            });
        }
    }
}

/// The page listens for `open-path` (`ui_ready`): send it the documents
/// that waited.
pub fn page_ready<R: Runtime>(app: &AppHandle<R>) {
    if let Some(gate) = app.try_state::<LaunchGate>() {
        for path in gate.page_ready() {
            emit_open(app, path);
        }
    }
}

/// Sends a document path to the viewer.
fn emit_open<R: Runtime>(app: &AppHandle<R>, path: PathBuf) {
    if let Err(e) = app.emit(OPEN_PATH, OpenPath { path }) {
        tracing::warn!("cannot send open-path: {e}");
    }
}

/// Closing the window hides it when close-to-tray is on and a tray exists
/// (saying so the first time); otherwise it quits.
fn on_close_requested<R: Runtime>(app: &AppHandle<R>) {
    let Some(state) = app.try_state::<AppState>() else {
        app.exit(0);
        return;
    };
    let to_tray = state.config().app.close_to_tray
        && state.has_tray.load(Ordering::SeqCst)
        && !state.quitting.load(Ordering::SeqCst);
    if !to_tray {
        begin_quit(app);
        return;
    }
    if let Some(window) = app.get_webview_window(MAIN) {
        let _ = window.hide();
    }
    if !state.told_about_tray.swap(true, Ordering::SeqCst) {
        notify(
            app,
            "JAI Studio is still running",
            "The model server keeps running in the tray. Quit from the tray menu.",
        );
    }
}

/// Quits once: hides the window and the tray, runs the shutdown (engines,
/// server, server.json, GPU lock), then exits.
pub fn begin_quit<R: Runtime>(app: &AppHandle<R>) {
    let Some(state) = app.try_state::<AppState>() else {
        app.exit(0);
        return;
    };
    if state.quitting.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Some(window) = app.get_webview_window(MAIN) {
        let _ = window.hide();
    }
    if let Some(tray) = app.tray_by_id(tray::TRAY_ID) {
        let _ = tray.set_visible(false);
    }
    let shared = state.shared();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if tokio::time::timeout(SHUTDOWN_TIMEOUT, shared.shutdown())
            .await
            .is_err()
        {
            tracing::warn!("the shutdown timed out");
        }
        shared.shutdown_done.store(true, Ordering::SeqCst);
        app.exit(0);
    });
}
