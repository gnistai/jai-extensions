//! `jai status`: server and engine status, from `GET /v1/status` when a
//! server is running, else from this PC's install state and GPU.

use std::process::ExitCode;

use jai_core::{catalog, gpu, install};
use jai_engines::SchedulerStatus;
use jai_js::ServerRef;
use jai_server::discovery::{self, ServerInfo};
use serde_json::{json, Value};

use super::{local_http, Client};
use crate::output::engines_summary;
use crate::settings;

pub async fn run() -> anyhow::Result<ExitCode> {
    let paths = super::paths()?;
    let lines = match discovery::find(&paths).await {
        Some((info, token)) => {
            let url = settings::client_url(&info.host, info.port);
            let client = Client {
                server: ServerRef {
                    url: url.clone(),
                    token,
                },
                http: local_http()?,
            };
            let v = client.status().await?;
            status_lines(Some((&info, url.as_str())), &v)
        }
        None => {
            let v = tokio::task::spawn_blocking(move || {
                let cfg = settings::load_config(&paths);
                let gpu = gpu::detect();
                let backend = gpu::effective_backend(cfg.backend, gpu.as_ref());
                let sel = catalog::select(catalog::catalog(), catalog::current_os(), backend);
                json!({
                    "installed": install::ready(&paths, &sel),
                    "gpu": gpu.map(|g| json!({"name": g.name, "vramUsedMb": g.vram_used_mb, "vramTotalMb": g.vram_total_mb})),
                    "backend": backend,
                })
            })
            .await?;
            status_lines(None, &v)
        }
    };
    // The version and the last engine error come from the server.
    for line in lines {
        outln!("{line}");
    }
    Ok(ExitCode::SUCCESS)
}

fn gb(mb: u64) -> String {
    format!("{:.1} GB", mb as f64 / 1024.0)
}

/// The status report: the server, the engines (when it runs), the models,
/// the GPU and the backend.
pub fn status_lines(server: Option<(&ServerInfo, &str)>, v: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    match server {
        Some((info, url)) => {
            let version = v
                .get("version")
                .and_then(Value::as_str)
                .unwrap_or(&info.version);
            lines.push(format!(
                "Server:   running at {url} (pid {}, version {version})",
                info.pid
            ));
            if let Some(s) = v
                .get("engines")
                .and_then(|e| serde_json::from_value::<SchedulerStatus>(e.clone()).ok())
            {
                lines.push(format!(
                    "Engines:  {}; {} active, {} waiting; {} swap{}; MTP {}",
                    engines_summary(&s),
                    s.active,
                    s.waiting_llm + s.waiting_image,
                    s.swaps,
                    if s.swaps == 1 { "" } else { "s" },
                    if s.mtp { "on" } else { "off" }
                ));
                if let Some(e) = &s.last_error {
                    lines.push(format!("Last error: {e}"));
                }
            }
        }
        None => lines.push("Server:   not running (start JAI Studio, or run `jai serve`)".into()),
    }
    let installed = v.get("installed").and_then(Value::as_bool).unwrap_or(false);
    lines.push(if installed {
        "Models:   installed".into()
    } else {
        "Models:   not installed: run `jai models download`".into()
    });
    match v.get("gpu").filter(|g| g.is_object()) {
        Some(g) => {
            let name = g
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown GPU");
            let used = g.get("vramUsedMb").and_then(Value::as_u64).unwrap_or(0);
            let total = g.get("vramTotalMb").and_then(Value::as_u64).unwrap_or(0);
            lines.push(format!(
                "GPU:      {name}, {} of {} VRAM in use",
                gb(used),
                gb(total)
            ));
        }
        None => lines.push("GPU:      no NVIDIA GPU found".into()),
    }
    if let Some(b) = v.get("backend").and_then(Value::as_str) {
        lines.push(format!("Backend:  {b}"));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_server() {
        let info = ServerInfo {
            version: "0.1.0".into(),
            pid: 4242,
            host: "127.0.0.1".into(),
            port: 5240,
            started: 0,
        };
        let v = json!({
            "version": "0.1.0",
            "engines": {"loaded": "llm", "state": "ready", "active": 1, "waitingLlm": 0, "waitingImage": 2,
                        "mtp": false, "lastError": null, "swaps": 3, "sinceMs": 1000},
            "installed": true,
            "gpu": {"name": "NVIDIA GeForce RTX 5070 Ti", "vramUsedMb": 2150, "vramTotalMb": 16303},
            "backend": "cuda"
        });
        assert_eq!(
            status_lines(Some((&info, "http://127.0.0.1:5240")), &v),
            [
                "Server:   running at http://127.0.0.1:5240 (pid 4242, version 0.1.0)",
                "Engines:  LLM ready · Image waiting; 1 active, 2 waiting; 3 swaps; MTP off",
                "Models:   installed",
                "GPU:      NVIDIA GeForce RTX 5070 Ti, 2.1 GB of 15.9 GB VRAM in use",
                "Backend:  cuda",
            ]
        );
    }

    #[test]
    fn no_server() {
        let v = json!({"installed": false, "gpu": null, "backend": "vulkan"});
        assert_eq!(
            status_lines(None, &v),
            [
                "Server:   not running (start JAI Studio, or run `jai serve`)",
                "Models:   not installed: run `jai models download`",
                "GPU:      no NVIDIA GPU found",
                "Backend:  vulkan",
            ]
        );
    }
}
