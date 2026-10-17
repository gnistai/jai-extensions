//! `jai serve`: the headless server in the foreground, holding the GPU lock;
//! Ctrl+C stops it. A second `jai serve` fails with "already running (pid,
//! port)".

use std::process::ExitCode;

use anyhow::Context as _;
use jai_core::install;
use jai_server::gpu_lock;
use jai_server::{discovery, Core};

use super::usage;
use crate::settings;
use crate::ServeArgs;

pub async fn run(args: ServeArgs) -> anyhow::Result<ExitCode> {
    let paths = super::paths()?;
    let mut cfg = settings::load_config(&paths);
    if let Some(host) = args.host {
        cfg.server.host = host;
    }
    if let Some(port) = args.port {
        cfg.server.port = port;
    }
    if args.allow_lan {
        cfg.server.allow_lan = true;
    }
    settings::validate(&cfg).map_err(usage)?;

    let Some(lock) = gpu_lock::try_acquire(&paths)? else {
        return Err(match discovery::find(&paths).await {
            Some((i, _)) => anyhow::anyhow!("already running (pid {}, port {})", i.pid, i.port),
            // A command's temporary server is private, so it is not found.
            None => anyhow::anyhow!(
                "another JAI process is using the GPU (a `jai` command, or a server that is \
                 starting); try again when it ends"
            ),
        });
    };

    let lan = !settings::is_loopback(&cfg.server.host);
    let core = Core::new(paths.clone(), cfg).context("cannot set up the model server")?;
    let handle = jai_server::start(core.clone())
        .await
        .context("cannot start the model server")?;
    let url = settings::addr_url(handle.addr);
    let installed = install::ready(&paths, &core.selection);
    for line in banner(&url, handle.addr.to_string().as_str(), installed, lan) {
        outln!("{line}");
    }

    stop_signal().await;
    errln!("Stopping…");
    // A second Ctrl+C while the engines stop exits at once.
    tokio::spawn(async {
        if tokio::signal::ctrl_c().await.is_ok() {
            std::process::exit(1);
        }
    });
    core.scheduler.shutdown().await;
    handle.stop().await;
    discovery::remove(&paths);
    drop(lock);
    errln!("Stopped.");
    Ok(ExitCode::SUCCESS)
}

/// What `jai serve` prints once it listens.
pub fn banner(url: &str, bound: &str, installed: bool, lan: bool) -> Vec<String> {
    let mut lines = vec![
        format!("JAI Studio server {} on {url}", env!("CARGO_PKG_VERSION")),
        "  POST /v1/chat/completions    model qwen3.8-27b".to_string(),
        "  POST /v1/images/generations  model flux-schnell".to_string(),
        "  Token: `jai token` prints it; send it as \"Authorization: Bearer <token>\".".to_string(),
    ];
    if !installed {
        lines.push(
            "  Models: not installed. Requests fail until you run `jai models download`."
                .to_string(),
        );
    }
    if lan {
        lines.push(format!(
            "  Warning: listening on {bound} for the network. Anyone who has the token can use it."
        ));
    }
    lines.push("Press Ctrl+C to stop.".to_string());
    lines
}

/// Ctrl+C; also SIGTERM on Unix, and closing the console or Ctrl+Break on
/// Windows. Never resolves when no handler can be installed.
async fn stop_signal() {
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let other = async {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                term.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(windows)]
    let other = async {
        use tokio::signal::windows::{ctrl_break, ctrl_close};
        match (ctrl_close(), ctrl_break()) {
            (Ok(mut close), Ok(mut brk)) => {
                tokio::select! {
                    _ = close.recv() => {}
                    _ = brk.recv() => {}
                }
            }
            _ => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(any(unix, windows)))]
    let other = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = other => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_lines() {
        let b = banner("http://127.0.0.1:5240", "127.0.0.1:5240", true, false);
        assert!(b[0].ends_with("on http://127.0.0.1:5240"));
        assert!(b.iter().any(|l| l.contains("/v1/chat/completions")));
        assert!(b.iter().any(|l| l.contains("/v1/images/generations")));
        assert!(!b
            .iter()
            .any(|l| l.contains("Warning") || l.contains("not installed")));
        assert_eq!(b.last().map(String::as_str), Some("Press Ctrl+C to stop."));
        let b = banner("http://127.0.0.1:5240", "0.0.0.0:5240", false, true);
        assert!(b.iter().any(|l| l.contains("not installed")));
        assert!(b
            .iter()
            .any(|l| l.contains("Warning: listening on 0.0.0.0:5240")));
    }
}
