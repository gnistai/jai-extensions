//! `POST /v1/images/generations`: checked, then run on sd-server's native async
//! API (DESIGN.md section 7).
//!
//! The job is submitted to `/sdcpp/v1/img_gen` and polled at
//! `/sdcpp/v1/jobs/{id}`. If the request ends before the job does (the client
//! left, the 10 minute timeout, shutdown), the job is cancelled while it is
//! still queued, and the lease is held until sd-server has answered that. The
//! submission runs in a task of its own, so this holds even when the request
//! ends while sd-server is still answering it.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use jai_engines::{EngineKind, Lease};
use rand::rngs::OsRng;
use rand::TryRngCore;
use serde_json::{json, Map, Value};
use tokio::sync::oneshot;
use tokio::time::MissedTickBehavior;

use crate::body::{field, json_object};
use crate::error::{ApiError, ErrorType};
use crate::server::Shared;
use crate::status::{require_installed, IMAGE_MODEL, IMAGE_MODEL_ALIAS};

/// Name of the engine in error messages.
const ENGINE: &str = "image";
const MAX_PROMPT_CHARS: usize = 8000;
/// sd-server would read engine parameters from this tag in a prompt.
const EXTRA_ARGS_TAG: &str = "<sd_cpp_extra_args";
const MIN_SIDE: u64 = 256;
const MAX_SIDE: u64 = 2048;
const SIDE_STEP: u64 = 64;
/// 1536 × 1536.
const MAX_PIXELS: u64 = 2_359_296;
const DEFAULT_SIDE: u64 = 1024;
const MAX_STEPS: u64 = 16;
const MAX_N: u64 = 4;
const FORMATS: [&str; 3] = ["png", "jpeg", "webp"];
const DEFAULT_COMPRESSION: u64 = 90;
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Longest time from the request's arrival to the finished images, waiting
/// for the GPU and starting the engine included.
const JOB_TIMEOUT: Duration = Duration::from_secs(600);
/// Longest time for one call to sd-server.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest time for the cancel call after the client has gone.
const CANCEL_TIMEOUT: Duration = Duration::from_secs(5);

/// A checked request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImageRequest {
    pub prompt: String,
    pub width: u64,
    pub height: u64,
    pub n: u64,
    pub seed: Option<u64>,
    pub steps: u64,
    pub output_format: &'static str,
    pub output_compression: u64,
}

/// `POST /v1/images/generations`
pub(crate) async fn generations(
    State(shared): State<Shared>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let arrived = Instant::now();
    let deadline = tokio::time::Instant::now() + JOB_TIMEOUT;
    let default_steps = crate::read_lock(&shared.core.config, |c| c.image.steps);
    let req = check(&json_object(body)?, default_steps)?;
    require_installed(&shared.core, EngineKind::Image).await?;
    let seed = match req.seed {
        Some(seed) => seed,
        None => random_seed(req.n)?,
    };
    // The limit covers the wait for the GPU and the engine start too, as
    // `queueMs` does.
    let work = async {
        let lease = shared
            .core
            .scheduler
            .acquire(EngineKind::Image)
            .await
            .map_err(ApiError::engine)?;
        run(&shared.core.http, lease, &req, seed).await
    };
    let done = tokio::time::timeout_at(deadline, work)
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::GATEWAY_TIMEOUT,
                ErrorType::Server,
                "timeout",
                "The image took longer than 10 minutes, waiting for the GPU included.",
            )
        })??;

    let started = done.started.unwrap_or(done.finished);
    tracing::info!(
        n = req.n,
        width = req.width,
        height = req.height,
        steps = req.steps,
        ms = millis(done.finished.saturating_duration_since(arrived)),
        "image generated"
    );
    let data: Vec<Value> = done
        .images
        .into_iter()
        .map(|(index, b64)| json!({ "b64_json": b64, "seed": seed + index }))
        .collect();
    Ok(Json(json!({
        "created": unix_now(),
        "model": IMAGE_MODEL,
        "width": req.width,
        "height": req.height,
        "steps": req.steps,
        "output_format": req.output_format,
        "data": data,
        "timings": {
            "queueMs": millis(started.saturating_duration_since(arrived)),
            "generateMs": millis(done.finished.saturating_duration_since(started)),
        },
    }))
    .into_response())
}

/// Applies the request rules.
pub(crate) fn check(
    req: &Map<String, Value>,
    default_steps: u32,
) -> Result<ImageRequest, ApiError> {
    let prompt = match field(req, "prompt") {
        Some(Value::String(p)) => p,
        Some(_) => {
            return Err(ApiError::invalid(
                "invalid_prompt",
                "prompt must be a string.",
            ))
        }
        None => return Err(ApiError::invalid("invalid_prompt", "prompt is required.")),
    };
    let chars = prompt.chars().count();
    if chars == 0 || chars > MAX_PROMPT_CHARS {
        return Err(ApiError::invalid(
            "invalid_prompt",
            format!("prompt must be 1 to {MAX_PROMPT_CHARS} characters; it has {chars}."),
        ));
    }
    if prompt.to_ascii_lowercase().contains(EXTRA_ARGS_TAG) {
        return Err(ApiError::invalid(
            "invalid_prompt",
            "prompt may not contain <sd_cpp_extra_args>.",
        ));
    }
    match field(req, "model") {
        None => {}
        Some(Value::String(m)) if m == IMAGE_MODEL || m == IMAGE_MODEL_ALIAS => {}
        Some(other) => {
            return Err(ApiError::invalid(
                "model_not_found",
                format!("Unknown model {other}: use \"{IMAGE_MODEL}\"."),
            ))
        }
    }
    let (width, height) = size(req)?;
    let steps = uint(req, "steps", 1, MAX_STEPS)?
        .unwrap_or_else(|| u64::from(default_steps).clamp(1, MAX_STEPS));
    let n = uint(req, "n", 1, MAX_N)?.unwrap_or(1);
    let seed = uint(req, "seed", 0, u64::from(u32::MAX))?;
    match field(req, "response_format") {
        None => {}
        Some(Value::String(f)) if f == "b64_json" => {}
        Some(Value::String(f)) if f == "url" => {
            return Err(ApiError::invalid(
                "unsupported_response_format",
                "response_format \"url\" is not supported: JAI Studio returns b64_json.",
            ))
        }
        Some(_) => {
            return Err(ApiError::invalid(
                "unsupported_response_format",
                "response_format must be \"b64_json\".",
            ))
        }
    }
    let output_format = match field(req, "output_format") {
        None => "png",
        Some(value) => value
            .as_str()
            .and_then(|f| FORMATS.iter().find(|known| **known == f).copied())
            .ok_or_else(|| {
                ApiError::invalid("invalid_value", "output_format must be png, jpeg or webp.")
            })?,
    };
    let output_compression =
        uint(req, "output_compression", 0, 100)?.unwrap_or(DEFAULT_COMPRESSION);
    Ok(ImageRequest {
        prompt: prompt.clone(),
        width,
        height,
        n,
        seed,
        steps,
        output_format,
        output_compression,
    })
}

/// `size` ("WxH"), or `width` and `height`; 1024x1024 by default.
fn size(req: &Map<String, Value>) -> Result<(u64, u64), ApiError> {
    let from_size = match field(req, "size") {
        None => None,
        Some(Value::String(s)) => Some(parse_size(s).ok_or_else(|| {
            ApiError::invalid(
                "invalid_size",
                format!("size must look like \"1024x768\", not {s:?}."),
            )
        })?),
        Some(_) => {
            return Err(ApiError::invalid(
                "invalid_size",
                "size must be a string like \"1024x768\".",
            ))
        }
    };
    let side = |key: &str| match field(req, key) {
        None => Ok(None),
        Some(value) => value.as_u64().map(Some).ok_or_else(|| {
            ApiError::invalid("invalid_size", format!("{key} must be a positive integer."))
        }),
    };
    let from_sides = match (side("width")?, side("height")?) {
        (None, None) => None,
        (Some(w), Some(h)) => Some((w, h)),
        _ => {
            return Err(ApiError::invalid(
                "invalid_size",
                "width and height go together.",
            ))
        }
    };
    let (w, h) = match (from_size, from_sides) {
        (Some(a), Some(b)) if a != b => {
            return Err(ApiError::invalid(
                "invalid_size",
                "size and width/height disagree.",
            ))
        }
        (Some(a), _) | (None, Some(a)) => a,
        (None, None) => (DEFAULT_SIDE, DEFAULT_SIDE),
    };
    for (name, side) in [("width", w), ("height", h)] {
        if !(MIN_SIDE..=MAX_SIDE).contains(&side) || side % SIDE_STEP != 0 {
            return Err(ApiError::invalid(
                "invalid_size",
                format!("{name} must be a multiple of 64 from 256 to 2048, not {side}."),
            ));
        }
    }
    if w * h > MAX_PIXELS {
        return Err(ApiError::invalid(
            "invalid_size",
            format!(
                "{w}x{h} is too large: width × height may be at most {MAX_PIXELS} (1536x1536)."
            ),
        ));
    }
    Ok((w, h))
}

/// "1344x768" (or "1344X768").
fn parse_size(s: &str) -> Option<(u64, u64)> {
    let (w, h) = s.split_once(['x', 'X'])?;
    let side = |t: &str| {
        (!t.is_empty() && t.len() <= 6 && t.bytes().all(|b| b.is_ascii_digit()))
            .then(|| t.parse().ok())
            .flatten()
    };
    Some((side(w)?, side(h)?))
}

/// An optional integer field in `min..=max`.
fn uint(req: &Map<String, Value>, key: &str, min: u64, max: u64) -> Result<Option<u64>, ApiError> {
    match field(req, key) {
        None => Ok(None),
        Some(value) => match value.as_u64() {
            Some(v) if (min..=max).contains(&v) => Ok(Some(v)),
            _ => Err(ApiError::invalid(
                "invalid_value",
                format!("{key} must be an integer from {min} to {max}."),
            )),
        },
    }
}

/// A seed from the OS RNG. sd-server's own default is a fixed 42. The range
/// leaves room for `seed + i` in a batch, so every image's seed can be sent
/// back on its own.
fn random_seed(n: u64) -> Result<u64, ApiError> {
    let max = u64::from(u32::MAX) - (n.max(1) - 1);
    let raw = OsRng
        .try_next_u64()
        .map_err(|e| ApiError::internal(format!("the OS random number generator failed: {e}")))?;
    Ok(raw % (max + 1))
}

/// The finished job.
struct Done {
    /// (batch index, base64 image), by index.
    images: Vec<(u64, String)>,
    /// When the job was first seen generating.
    started: Option<Instant>,
    finished: Instant,
}

/// Submits the job and polls it to the end.
async fn run(
    http: &reqwest::Client,
    lease: Lease,
    req: &ImageRequest,
    seed: u64,
) -> Result<Done, ApiError> {
    let base = lease.base_url.trim_end_matches('/').to_owned();
    let body = json!({
        "prompt": req.prompt,
        "width": req.width,
        "height": req.height,
        "seed": seed,
        "batch_count": req.n,
        "sample_params": {
            "sample_steps": req.steps,
            "sample_method": "euler",
            "guidance": { "txt_cfg": 1.0 },
        },
        "output_format": req.output_format,
        "output_compression": req.output_compression,
    });
    let mut job = submit(http.clone(), lease, base.clone(), body).await?;
    let id = job.id.clone();
    let poll_url = format!("{base}/sdcpp/v1/jobs/{id}");

    let mut started = None;
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let (status, bytes) = call(http.get(&poll_url)).await?;
        if !status.is_success() {
            return Err(ApiError::upstream_invalid(
                ENGINE,
                format!("job {id} answered {status}"),
            ));
        }
        let state: Value =
            serde_json::from_slice(&bytes).map_err(|e| ApiError::upstream_invalid(ENGINE, e))?;
        match state.get("status").and_then(Value::as_str) {
            Some("queued") => {}
            Some("generating") => {
                started.get_or_insert_with(Instant::now);
            }
            Some("completed") => {
                let finished = Instant::now();
                job.finish();
                return Ok(Done {
                    images: images_of(state)?,
                    started,
                    finished,
                });
            }
            Some("failed") => {
                job.finish();
                let reason = state
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("no reason given");
                return Err(ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ErrorType::Server,
                    "generation_failed",
                    format!("The image engine could not generate the image: {reason}"),
                ));
            }
            Some("cancelled") => {
                job.finish();
                return Err(ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    ErrorType::Unavailable,
                    "cancelled",
                    "The image engine cancelled the job.",
                ));
            }
            other => {
                return Err(ApiError::upstream_invalid(
                    ENGINE,
                    format!("job status {other:?}"),
                ))
            }
        }
    }
}

/// Submits the job from a task that owns the lease until it hands the job
/// over. If the request has ended by the time sd-server accepts the job (the
/// client left, the timeout, shutdown), dropping the job that comes back
/// cancels it, and the lease goes only after that, so the next image job
/// does not start beside an orphan.
async fn submit(
    http: reqwest::Client,
    lease: Lease,
    base: String,
    body: Value,
) -> Result<Submitted, ApiError> {
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let answer = call(http.post(format!("{base}/sdcpp/v1/img_gen")).json(&body)).await;
        let submitted = answer
            .and_then(|(status, bytes)| {
                if status.is_success() {
                    job_id(&bytes)
                } else {
                    Err(ApiError::upstream_status(ENGINE, status, &bytes))
                }
            })
            .map(|id| Submitted {
                cancel_url: format!("{base}/sdcpp/v1/jobs/{id}/cancel"),
                id,
                http,
                lease: Some(lease),
            });
        // With the request gone, `send` hands the job back and it drops here.
        let _ = tx.send(submitted);
    });
    rx.await.unwrap_or_else(|_| {
        Err(ApiError::internal(
            "the image job submission stopped unexpectedly",
        ))
    })
}

/// Sends one call to sd-server and reads the whole answer.
async fn call(request: reqwest::RequestBuilder) -> Result<(StatusCode, Bytes), ApiError> {
    let res = request
        .timeout(CALL_TIMEOUT)
        .send()
        .await
        .map_err(|e| ApiError::upstream_io(ENGINE, &e))?;
    let status = res.status();
    let bytes = res
        .bytes()
        .await
        .map_err(|e| ApiError::upstream_io(ENGINE, &e))?;
    Ok((status, bytes))
}

/// The job id from the 202 answer. It goes into URL paths, so only the
/// characters sd-server uses are accepted.
fn job_id(bytes: &[u8]) -> Result<String, ApiError> {
    let answer: Value =
        serde_json::from_slice(bytes).map_err(|e| ApiError::upstream_invalid(ENGINE, e))?;
    let id = answer.get("id").and_then(Value::as_str).unwrap_or("");
    let valid = !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if valid {
        Ok(id.to_owned())
    } else {
        Err(ApiError::upstream_invalid(ENGINE, format!("job id {id:?}")))
    }
}

/// `result.images[]` of a completed job, ordered by `index`.
fn images_of(mut state: Value) -> Result<Vec<(u64, String)>, ApiError> {
    let list = match state.pointer_mut("/result/images").map(Value::take) {
        Some(Value::Array(list)) if !list.is_empty() => list,
        _ => {
            return Err(ApiError::upstream_invalid(
                ENGINE,
                "the job finished without images",
            ))
        }
    };
    let mut images = Vec::with_capacity(list.len());
    for (pos, mut image) in list.into_iter().enumerate() {
        let index = image
            .get("index")
            .and_then(Value::as_u64)
            .unwrap_or(pos as u64);
        match image.get_mut("b64_json").map(Value::take) {
            Some(Value::String(b64)) => images.push((index, b64)),
            _ => {
                return Err(ApiError::upstream_invalid(
                    ENGINE,
                    "an image without b64_json",
                ))
            }
        }
    }
    images.sort_by_key(|(index, _)| *index);
    Ok(images)
}

/// A submitted job that holds the lease. Dropped before `finish` (the client
/// left, the timeout, shutdown, an engine error), it asks sd-server to
/// cancel the job and releases the lease once that call returns. sd-server
/// only cancels queued jobs; for a running one the call is a harmless 409.
struct Submitted {
    id: String,
    http: reqwest::Client,
    cancel_url: String,
    lease: Option<Lease>,
}

impl Submitted {
    /// The job ended on its own: nothing to cancel, and the GPU is free.
    fn finish(&mut self) {
        self.lease = None;
    }
}

impl Drop for Submitted {
    fn drop(&mut self) {
        let Some(lease) = self.lease.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let http = self.http.clone();
        let url = std::mem::take(&mut self.cancel_url);
        runtime.spawn(async move {
            match http.post(&url).timeout(CANCEL_TIMEOUT).send().await {
                Ok(res) => tracing::info!(status = %res.status(), "asked the image engine to cancel an abandoned job"),
                Err(err) => tracing::debug!(error = %err, "could not cancel an abandoned job"),
            }
            drop(lease);
        });
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(json: Value) -> Map<String, Value> {
        json.as_object().unwrap().clone()
    }

    fn code(json: Value) -> String {
        check(&req(json), 4).unwrap_err().code.into_owned()
    }

    #[test]
    fn defaults() {
        let r = check(&req(json!({ "prompt": "a cat" })), 4).unwrap();
        assert_eq!(
            r,
            ImageRequest {
                prompt: "a cat".into(),
                width: 1024,
                height: 1024,
                n: 1,
                seed: None,
                steps: 4,
                output_format: "png",
                output_compression: 90,
            }
        );
        assert_eq!(check(&req(json!({ "prompt": "a" })), 40).unwrap().steps, 16);
        assert_eq!(check(&req(json!({ "prompt": "a" })), 0).unwrap().steps, 1);
    }

    #[test]
    fn everything_set() {
        let r = check(
            &req(json!({
                "prompt": "a cat", "model": "black-forest-labs/flux-schnell", "n": 4,
                "size": "1344x768", "width": 1344, "height": 768, "seed": 4294967295u64,
                "steps": 16, "output_format": "webp", "output_compression": 0,
                "response_format": "b64_json", "quality": "hd", "style": "vivid",
            })),
            4,
        )
        .unwrap();
        assert_eq!((r.width, r.height, r.n, r.steps), (1344, 768, 4, 16));
        assert_eq!(r.seed, Some(4294967295));
        assert_eq!(r.output_format, "webp");
        assert_eq!(r.output_compression, 0);
    }

    #[test]
    fn sizes() {
        let ok = |j: Value| check(&req(j), 4).map(|r| (r.width, r.height));
        assert_eq!(
            ok(json!({ "prompt": "a", "size": "256x2048" })).unwrap(),
            (256, 2048)
        );
        assert_eq!(
            ok(json!({ "prompt": "a", "size": "1536X1536" })).unwrap(),
            (1536, 1536)
        );
        assert_eq!(
            ok(json!({ "prompt": "a", "width": 832, "height": 1216 })).unwrap(),
            (832, 1216)
        );
        for bad in [
            json!({ "prompt": "a", "size": "1000x1000" }),
            json!({ "prompt": "a", "size": "2048x2048" }),
            json!({ "prompt": "a", "size": "192x256" }),
            json!({ "prompt": "a", "size": "4096x256" }),
            json!({ "prompt": "a", "size": "1024" }),
            json!({ "prompt": "a", "size": "auto" }),
            json!({ "prompt": "a", "size": "+1024x1024" }),
            json!({ "prompt": "a", "size": 1024 }),
            json!({ "prompt": "a", "width": 1024 }),
            json!({ "prompt": "a", "width": 1024, "height": -64 }),
            json!({ "prompt": "a", "width": 1024.5, "height": 1024 }),
            json!({ "prompt": "a", "size": "1024x1024", "width": 512, "height": 512 }),
        ] {
            assert_eq!(code(bad.clone()), "invalid_size", "{bad}");
        }
    }

    #[test]
    fn rules() {
        assert_eq!(code(json!({})), "invalid_prompt");
        assert_eq!(code(json!({ "prompt": "" })), "invalid_prompt");
        assert_eq!(code(json!({ "prompt": 5 })), "invalid_prompt");
        assert_eq!(
            code(json!({ "prompt": "x".repeat(8001) })),
            "invalid_prompt"
        );
        assert!(check(&req(json!({ "prompt": "é".repeat(8000) })), 4).is_ok());
        assert_eq!(
            code(json!({ "prompt": "a cat <sd_cpp_extra_args>{\"seed\":1}</sd_cpp_extra_args>" })),
            "invalid_prompt"
        );
        assert_eq!(
            code(json!({ "prompt": "a <SD_CPP_EXTRA_ARGS>" })),
            "invalid_prompt"
        );
        assert_eq!(
            code(json!({ "prompt": "a", "model": "dall-e-3" })),
            "model_not_found"
        );
        assert_eq!(code(json!({ "prompt": "a", "n": 0 })), "invalid_value");
        assert_eq!(code(json!({ "prompt": "a", "n": 5 })), "invalid_value");
        assert_eq!(code(json!({ "prompt": "a", "steps": 0 })), "invalid_value");
        assert_eq!(code(json!({ "prompt": "a", "steps": 17 })), "invalid_value");
        assert_eq!(code(json!({ "prompt": "a", "seed": -1 })), "invalid_value");
        assert_eq!(
            code(json!({ "prompt": "a", "seed": 4294967296u64 })),
            "invalid_value"
        );
        assert_eq!(code(json!({ "prompt": "a", "seed": "1" })), "invalid_value");
        assert_eq!(
            code(json!({ "prompt": "a", "response_format": "url" })),
            "unsupported_response_format"
        );
        assert_eq!(
            code(json!({ "prompt": "a", "output_format": "gif" })),
            "invalid_value"
        );
        assert_eq!(
            code(json!({ "prompt": "a", "output_compression": 101 })),
            "invalid_value"
        );
    }

    #[test]
    fn random_seeds_leave_room_for_the_batch() {
        for n in 1..=4 {
            let seed = random_seed(n).unwrap();
            assert!(seed + n - 1 <= u64::from(u32::MAX));
        }
    }

    #[test]
    fn job_ids() {
        assert_eq!(job_id(br#"{"id":"job_01HTX-yz"}"#).unwrap(), "job_01HTX-yz");
        assert!(job_id(br#"{"id":"../../etc"}"#).is_err());
        assert!(job_id(br#"{"id":""}"#).is_err());
        assert!(job_id(br#"{}"#).is_err());
    }

    /// SERVER-JS-8: the 10 minute limit counts from the request's arrival,
    /// so a request stuck waiting for the GPU or for the engine to start
    /// ends with the documented 504 instead of waiting longer. The engine
    /// here never becomes ready; its own start timeout is far longer.
    #[tokio::test(start_paused = true)]
    async fn the_limit_covers_the_gpu_wait() {
        use std::sync::{Arc, RwLock};

        use jai_core::{Backend, Component, Config, Paths, Role, Selection, Unpack};
        use jai_engines::{LaunchAttempt, Launcher, RunningEngine, Scheduler, SchedulerConfig};
        use tokio_util::sync::CancellationToken;

        use crate::auth::HostRule;
        use crate::Core;

        struct NeverReady;

        #[async_trait::async_trait]
        impl Launcher for NeverReady {
            async fn launch(
                &self,
                _kind: EngineKind,
                _attempt: LaunchAttempt,
            ) -> anyhow::Result<Box<dyn RunningEngine>> {
                std::future::pending().await
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path().join("jai"));
        // An image runtime with its install marker, so the engine counts as
        // installed (as in tests/common).
        let runtime = Component {
            id: "jai-test-image-runtime".into(),
            role: Role::ImageRuntime,
            label: "runtime".into(),
            file: "runtime.zip".into(),
            url: "https://example.invalid/runtime.zip".into(),
            size: 1,
            sha256: "ab".repeat(32),
            license: "MIT".into(),
            source: "test".into(),
            os: None,
            backend: None,
            unpack: Some(Unpack::Zip),
            strip: None,
            dir: Some("runtime-dir".into()),
            exe: None,
        };
        let marker_dir = paths.engines().join("runtime-dir");
        std::fs::create_dir_all(&marker_dir).unwrap();
        std::fs::write(
            marker_dir.join(format!(".ok-{}", runtime.id)),
            &runtime.sha256,
        )
        .unwrap();
        let scheduler = Scheduler::new(
            Arc::new(NeverReady),
            SchedulerConfig {
                idle_unload: None,
                start_timeout: Duration::from_secs(4 * 3600),
                mtp: false,
                ..SchedulerConfig::default()
            },
        );
        let core = Arc::new(Core {
            paths,
            config: Arc::new(RwLock::new(Config::default())),
            token: Arc::new(RwLock::new("jai_test".into())),
            selection: Selection {
                components: vec![runtime],
                llm_backend: Backend::Cpu,
                image_backend: Backend::Cpu,
            },
            scheduler,
            http: reqwest::Client::new(),
        });
        let shared = Shared {
            core,
            hosts: Arc::new(HostRule::new(5240, std::net::Ipv4Addr::LOCALHOST.into())),
            shutdown: CancellationToken::new(),
            image_work: Arc::new(tokio::sync::Semaphore::new(1)),
        };
        let begun = tokio::time::Instant::now();
        let body = Bytes::from_static(br#"{"prompt":"a fox","size":"256x256","seed":1}"#);
        let err = generations(State(shared), Ok(body)).await.unwrap_err();
        assert_eq!(err.status, StatusCode::GATEWAY_TIMEOUT, "{err:?}");
        assert_eq!(err.code, "timeout");
        let waited = begun.elapsed();
        assert!(
            waited >= JOB_TIMEOUT && waited < JOB_TIMEOUT + Duration::from_secs(5),
            "{waited:?}"
        );
    }

    #[test]
    fn images_by_index() {
        let state = json!({ "result": { "images": [
            { "index": 1, "b64_json": "B" },
            { "index": 0, "b64_json": "A" },
        ] } });
        assert_eq!(
            images_of(state).unwrap(),
            vec![(0, "A".to_owned()), (1, "B".to_owned())]
        );
        assert!(images_of(json!({ "result": { "images": [] } })).is_err());
        assert!(images_of(json!({ "result": null })).is_err());
    }
}
