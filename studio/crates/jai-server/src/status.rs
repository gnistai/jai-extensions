//! `GET /health`, `GET /v1/models` and `GET /v1/status`, and the install
//! check behind `models_missing` (DESIGN.md section 7).

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{RawQuery, State};
use axum::Json;
use jai_core::install::InstallState;
use jai_core::Role;
use jai_engines::EngineKind;
use serde_json::{json, Value};

use crate::error::ApiError;
use crate::server::Shared;
use crate::{proof, Core, VERSION};

/// The LLM's id in the API.
pub(crate) const LLM_MODEL: &str = "qwen3.8-27b";
/// The image model's id in the API.
pub(crate) const IMAGE_MODEL: &str = "flux-schnell";
/// The image model's document id, shared with the Replicate renderer.
pub(crate) const IMAGE_MODEL_ALIAS: &str = "black-forest-labs/flux-schnell";

/// Longest wait for `nvidia-smi` in `/v1/status`.
const GPU_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether each engine has everything it needs on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Installed {
    pub llm: bool,
    pub image: bool,
}

impl Installed {
    pub fn has(self, kind: EngineKind) -> bool {
        match kind {
            EngineKind::Llm => self.llm,
            EngineKind::Image => self.image,
        }
    }
}

/// The engine a component belongs to.
fn kind_of(role: Role) -> EngineKind {
    match role {
        Role::LlmModel | Role::LlmProjector | Role::LlmEngine | Role::LlmRuntime => EngineKind::Llm,
        Role::ImageDiffusion
        | Role::ImageVae
        | Role::ImageClipL
        | Role::ImageT5xxl
        | Role::ImageEngine
        | Role::ImageRuntime => EngineKind::Image,
    }
}

/// Install state per engine: an engine is installed when it has components
/// and all of them are Installed, as `install::ready` decides for the whole
/// selection. Cheap (file sizes and markers), but it touches the disk, so it
/// runs on the blocking pool.
pub(crate) async fn installed(core: &Arc<Core>) -> Result<Installed, ApiError> {
    let core = Arc::clone(core);
    tokio::task::spawn_blocking(move || {
        // (components seen, all installed) per engine
        let mut llm = (false, true);
        let mut image = (false, true);
        for status in jai_core::install::status(&core.paths, &core.selection) {
            let entry = match kind_of(status.component.role) {
                EngineKind::Llm => &mut llm,
                EngineKind::Image => &mut image,
            };
            entry.0 = true;
            entry.1 &= matches!(status.state, InstallState::Installed);
        }
        Installed {
            llm: llm.0 && llm.1,
            image: image.0 && image.1,
        }
    })
    .await
    .map_err(|err| ApiError::internal(format!("could not check the installed models: {err}")))
}

/// 409 `models_missing` unless the engine of this kind is installed.
pub(crate) async fn require_installed(core: &Arc<Core>, kind: EngineKind) -> Result<(), ApiError> {
    if installed(core).await?.has(kind) {
        Ok(())
    } else {
        Err(ApiError::models_missing())
    }
}

/// `GET /health`: open to anyone who passes the Host check. With
/// `?nonce=…` the answer also carries `proof`, which shows that this server
/// holds the token without revealing it; clients check it before they send
/// the token (DESIGN.md section 7).
pub(crate) async fn health(
    State(shared): State<Shared>,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let mut body = json!({ "status": "ok", "name": "jai", "version": VERSION });
    if let Some(nonce) = nonce_of(query.as_deref())? {
        let token = crate::read_lock(&shared.core.token, Clone::clone);
        if !token.is_empty() {
            body["proof"] = Value::String(proof::proof(&token, shared.hosts.port(), nonce));
            // Which process answered: server.json can still name an earlier
            // server on this port that was killed.
            body["pid"] = json!(std::process::id());
        }
    }
    Ok(Json(body))
}

/// The `nonce` query parameter: at most once, 16 to 128 base64url
/// characters (which need no percent-decoding).
fn nonce_of(query: Option<&str>) -> Result<Option<&str>, ApiError> {
    let mut found = None;
    for pair in query.unwrap_or_default().split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key != "nonce" {
            continue;
        }
        if found.is_some() || !proof::valid_nonce(value) {
            return Err(ApiError::invalid(
                "invalid_nonce",
                "nonce must be 16 to 128 base64url characters, given once.",
            ));
        }
        found = Some(value);
    }
    Ok(found)
}

/// `GET /v1/models`
pub(crate) async fn models(State(shared): State<Shared>) -> Result<Json<Value>, ApiError> {
    let installed = installed(&shared.core).await?;
    Ok(Json(json!({
        "object": "list",
        "data": [
            {
                "id": LLM_MODEL,
                "object": "model",
                "owned_by": "jai",
                "kind": "llm",
                "installed": installed.llm,
            },
            {
                "id": IMAGE_MODEL,
                "object": "model",
                "owned_by": "jai",
                "kind": "image",
                "installed": installed.image,
                "aliases": [IMAGE_MODEL_ALIAS],
            },
        ],
    })))
}

/// `GET /v1/status`. `backend` is the LLM's; `imageBackend` differs where
/// stable-diffusion.cpp has no build for it (CUDA on Linux).
pub(crate) async fn status(State(shared): State<Shared>) -> Result<Json<Value>, ApiError> {
    let installed = installed(&shared.core).await?;
    let gpu = gpu_summary().await;
    let selection = &shared.core.selection;
    Ok(Json(json!({
        "version": VERSION,
        "engines": shared.core.scheduler.status(),
        "installed": installed.llm && installed.image,
        "gpu": gpu,
        "backend": selection.llm_backend,
        "imageBackend": selection.image_backend,
    })))
}

/// `{name, vramUsedMb, vramTotalMb}` from nvidia-smi, or null.
async fn gpu_summary() -> Value {
    let probe = tokio::task::spawn_blocking(jai_core::gpu::detect);
    match tokio::time::timeout(GPU_PROBE_TIMEOUT, probe).await {
        Ok(Ok(Some(gpu))) => json!({
            "name": gpu.name,
            "vramUsedMb": gpu.vram_used_mb,
            "vramTotalMb": gpu.vram_total_mb,
        }),
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonce_parameters() {
        let nonce = "abcdefghijklmnop";
        assert_eq!(nonce_of(None).unwrap(), None);
        assert_eq!(nonce_of(Some("")).unwrap(), None);
        assert_eq!(nonce_of(Some("x=1&y")).unwrap(), None);
        assert_eq!(
            nonce_of(Some("nonce=abcdefghijklmnop")).unwrap(),
            Some(nonce)
        );
        assert_eq!(
            nonce_of(Some("x=1&nonce=abcdefghijklmnop&y=2")).unwrap(),
            Some(nonce)
        );
        for bad in [
            "nonce=short",
            "nonce=",
            "nonce",
            "nonce=abcdefghijklmnop&nonce=abcdefghijklmnop",
            "nonce=abcdefghijklmn%2F",
        ] {
            assert_eq!(
                nonce_of(Some(bad)).unwrap_err().code,
                "invalid_nonce",
                "{bad}"
            );
        }
    }

    #[test]
    fn roles_map_to_engines() {
        assert_eq!(kind_of(Role::LlmRuntime), EngineKind::Llm);
        assert_eq!(kind_of(Role::LlmProjector), EngineKind::Llm);
        assert_eq!(kind_of(Role::ImageT5xxl), EngineKind::Image);
        assert_eq!(kind_of(Role::ImageRuntime), EngineKind::Image);
    }
}
