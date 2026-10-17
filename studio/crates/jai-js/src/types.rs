//! Rust mirrors of the types in `src/studio/host-entry.ts` (camelCase).

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::imageops::Thumb;

/// The local server. The bundle's `fetch` reaches only this origin.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerRef {
    pub url: String,
    pub token: String,
}

/// A document as file bytes (`.jai`, JSON or text) or as text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DocInput {
    Bytes(Vec<u8>),
    Text(String),
}

/// A diagnostic from the JAI library or a renderer. Other fields a diagnostic
/// may carry are dropped; numbers and booleans in these fields become text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diag {
    pub code: String,
    #[serde(
        default,
        deserialize_with = "lenient::text",
        skip_serializing_if = "Option::is_none"
    )]
    pub key: Option<String>,
    #[serde(
        default,
        deserialize_with = "lenient::text",
        skip_serializing_if = "Option::is_none"
    )]
    pub requested: Option<String>,
    #[serde(
        default,
        deserialize_with = "lenient::text",
        skip_serializing_if = "Option::is_none"
    )]
    pub actual: Option<String>,
    #[serde(
        default,
        deserialize_with = "lenient::text",
        skip_serializing_if = "Option::is_none"
    )]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidateResult {
    pub ok: bool,
    /// `invalid-json`, `malformed-container` or `undecodable` was reported.
    pub fatal: bool,
    /// `gzip`, `json` or `text`.
    pub form: Option<String>,
    /// The canonical text.
    pub text: Option<String>,
    pub diagnostics: Vec<Diag>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderOptions {
    pub server: ServerRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `png` or `jpeg`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderOutput {
    pub bytes: Vec<u8>,
    pub mime: String,
    pub model: String,
    pub model_label: String,
    pub sent_prompt: String,
    pub seed: u64,
    pub width: u32,
    pub height: u32,
    pub description: String,
    pub document_text: String,
    pub diagnostics: Vec<Diag>,
}

/// The target image as the analyst sees it: a downscaled JPEG data URL.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncodeTarget {
    pub url: String,
    pub width: u32,
    pub height: u32,
}

/// Mirrors `EncodeHostOptions`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EncodeOptions {
    pub server: ServerRef,
    pub target: EncodeTarget,
    pub target_thumbnail: Thumb,
    pub generations: u32,
    pub population: u32,
    pub seeds: Vec<u32>,
    pub restarts: u32,
    pub goal: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_limit_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render_scale: Option<f64>,
    /// `off`, `propose` or `all`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EncodeBest {
    pub id: String,
    /// NaN when JavaScript reported a non-finite number (JSON null).
    #[serde(deserialize_with = "lenient::number")]
    pub fitness: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EncodeOutput {
    pub text: String,
    /// The `.jai` file (gzip form).
    pub jai: Vec<u8>,
    pub stop_reason: String,
    #[serde(deserialize_with = "lenient::number")]
    pub seconds: f64,
    /// Why the run stopped early, when it stopped on an error (stop reason
    /// `error`) or with a message; the best result so far is still here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub best: EncodeBest,
    /// Same shape as the Node CLI's report.json; renders are named by file.
    pub report: serde_json::Value,
}

/// Where an encode sends its events and renders.
#[derive(Clone)]
pub struct EncodeSink {
    /// One engine event, with renders replaced by `{model, seed, file, error,
    /// measure, score}`.
    pub on_event: Arc<dyn Fn(serde_json::Value) + Send + Sync>,
    /// `__jai.saveRender(name, bytes)` writes `<render_dir>/<name>`.
    pub render_dir: PathBuf,
}

/// Deserializers that accept what JavaScript's JSON.stringify produces.
mod lenient {
    use serde::{Deserialize, Deserializer};
    use serde_json::Value;

    /// A string field that may arrive as a number, a boolean or null.
    pub fn text<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
        Ok(match Value::deserialize(d)? {
            Value::Null => None,
            Value::String(s) => Some(s),
            other => Some(other.to_string()),
        })
    }

    /// A number that JSON.stringify wrote as null (NaN or an infinity).
    pub fn number<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
        Ok(Option::<f64>::deserialize(d)?.unwrap_or(f64::NAN))
    }
}
