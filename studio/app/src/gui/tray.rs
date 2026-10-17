//! The tray icon (DESIGN.md section 10, "Tray"): the JAI icon with the engine
//! states in its tooltip and in a disabled menu line, updated from
//! `scheduler.subscribe()`, or every few seconds from the other JAI process
//! whose server the app uses; the menu; left click shows the window.

use std::sync::Arc;
use std::time::Duration;

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Runtime};

use super::state::{AppState, Shared};
use crate::output::{engines_summary, tray_tooltip};

/// The tray icon's id.
pub const TRAY_ID: &str = "jai";

/// How often the tray reads the engines of another JAI process.
const REMOTE_EVERY: Duration = Duration::from_secs(3);

/// Whether a tray can be shown (see `gui_support::tray_available`).
pub fn available() -> bool {
    crate::gui_support::tray_available()
}

/// Builds the tray and starts updating it.
pub fn create<R: Runtime>(app: &AppHandle<R>, shared: Arc<Shared>) -> tauri::Result<()> {
    let status = shared.core().scheduler.status();
    let open = MenuItem::with_id(app, "open", "Open JAI Studio", true, None::<&str>)?;
    let line = MenuItem::with_id(app, "status", engines_summary(&status), false, None::<&str>)?;
    let unload = MenuItem::with_id(app, "unload", "Unload models", true, None::<&str>)?;
    let copy = MenuItem::with_id(app, "copy-token", "Copy server token", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &open,
            &PredefinedMenuItem::separator(app)?,
            &line,
            &unload,
            &copy,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;
    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip(tray_tooltip(&status))
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => super::show_main(app),
            "unload" => unload_models(app),
            "copy-token" => copy_token(app),
            "quit" => super::begin_quit(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                super::show_main(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    let tray = builder.build(app)?;
    watch_status(tray, line, shared);
    Ok(())
}

/// Unloads this app's models, or says that another JAI process has them.
fn unload_models<R: Runtime>(app: &AppHandle<R>) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let shared = state.shared();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        match shared.remote_engines().await {
            Some(remote) => super::notify(
                &app,
                "The models are not JAI Studio's",
                &super::commands::remote_engines_message(remote.pid),
            ),
            None => shared.core().scheduler.unload().await,
        }
    });
}

/// Puts the token on the clipboard and says so.
fn copy_token<R: Runtime>(app: &AppHandle<R>) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    match crate::integration::copy_text(&state.token()) {
        Ok(()) => super::notify(
            app,
            "Server token copied",
            "Paste it into the app that uses JAI Studio's server.",
        ),
        Err(e) => {
            tracing::warn!("cannot copy the token: {e:#}");
            super::notify(
                app,
                "Cannot copy the token",
                "Copy it in JAI Studio's Server tab, or run `jai token`.",
            );
        }
    }
}

/// Keeps the tooltip and the status line current, following the core when
/// settings replace it. While another JAI process runs the server (and the
/// engines), its status is read every few seconds instead.
fn watch_status<R: Runtime>(tray: TrayIcon<R>, line: MenuItem<R>, shared: Arc<Shared>) {
    let mut cores = shared.subscribe_core();
    tauri::async_runtime::spawn(async move {
        let mut tick = tokio::time::interval(REMOTE_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let mut status = cores.borrow_and_update().scheduler.subscribe();
            loop {
                let s = match shared.remote_engines().await {
                    Some(remote) => remote.engines,
                    None => status.borrow_and_update().clone(),
                };
                let _ = tray.set_tooltip(Some(tray_tooltip(&s)));
                let _ = line.set_text(engines_summary(&s));
                tokio::select! {
                    _ = tick.tick() => {}
                    changed = status.changed() => {
                        if changed.is_err() {
                            // This scheduler has shut down: wait for the next core.
                            if cores.changed().await.is_err() {
                                return;
                            }
                            break;
                        }
                    }
                    changed = cores.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        break;
                    }
                }
            }
        }
    });
}
