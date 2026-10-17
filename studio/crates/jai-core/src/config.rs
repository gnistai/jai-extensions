//! Settings in `<root>/config.json` (DESIGN.md section 3).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::warn;

use crate::fsutil;
use crate::paths::Paths;

/// A `config.json` that is not a JSON object is kept under this prefix plus
/// the Unix time before Studio saves over it.
pub(crate) const INVALID_PREFIX: &str = "config.json.invalid-";

/// All settings. camelCase on disk; every field has a default and unknown
/// fields are ignored.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    pub version: u32,
    pub server: ServerSettings,
    pub idle_unload_minutes: u32,
    pub backend: BackendPref,
    pub llm: LlmSettings,
    pub image: ImageSettings,
    pub app: AppSettings,
}

/// Where the HTTP server binds. A host other than a loopback address requires
/// `allow_lan`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ServerSettings {
    pub host: String,
    pub port: u16,
    pub allow_lan: bool,
}

/// `auto` means CUDA when an NVIDIA GPU that can run the CUDA 13 engines is
/// detected (Turing or newer with an R580 driver or newer; see
/// `gpu::cuda_unsupported`), otherwise Vulkan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendPref {
    Auto,
    Cuda,
    Vulkan,
    Cpu,
}

/// llama-server settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LlmSettings {
    pub ctx: u32,
    /// `"all"` or a layer count.
    pub gpu_layers: String,
    pub mtp: bool,
    pub parallel: u32,
    pub image_max_tokens: u32,
    pub extra_args: Vec<String>,
}

/// sd-server settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ImageSettings {
    pub steps: u32,
    pub extra_args: Vec<String>,
}

/// Desktop app settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppSettings {
    pub close_to_tray: bool,
    pub start_at_login: bool,
    pub file_association: bool,
}

impl Config {
    /// Reads `config.json`. A missing file gives the defaults. An unreadable
    /// or invalid file gives the defaults with a warning; a field with a bad
    /// value falls back to its own default and keeps the others.
    pub fn load(paths: &Paths) -> Config {
        Config::load_checked(paths).0
    }

    /// [`Config::load`], plus what was wrong with `config.json`, if anything,
    /// in words for the UI: a syntax error (the defaults are in use), or the
    /// settings whose invalid values were replaced by their defaults.
    pub fn load_checked(paths: &Paths) -> (Config, Option<String>) {
        let file = paths.config_file();
        match fs::read_to_string(&file) {
            Ok(text) => parse(&text, &file),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (Config::default(), None),
            Err(e) => {
                warn!(path = %file.display(), error = %e, "cannot read the settings; using the defaults");
                let problem = format!(
                    "{} cannot be read ({e}), so JAI Studio uses the default settings",
                    file.display()
                );
                (Config::default(), Some(problem))
            }
        }
    }

    /// Writes `config.json` atomically (a temporary file, then a rename).
    ///
    /// Keys Studio does not know, from a newer version or added by hand, are
    /// kept. A file that is not a JSON object (such as a hand edit with a
    /// syntax error, which [`Config::load`] replaced with the defaults) is
    /// first renamed to `config.json.invalid-<unix time>`, so the edit is not
    /// lost.
    pub fn save(&self, paths: &Paths) -> anyhow::Result<()> {
        let file = paths.config_file();
        let ours = serde_json::to_value(self).context("cannot serialize the settings")?;
        let existing = match fs::read(&file) {
            Ok(bytes) => Some(serde_json::from_slice::<Value>(&bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", file.display())),
        };
        let merged = match existing {
            Some(Ok(theirs @ Value::Object(_))) => merge(theirs, ours.clone()),
            Some(_) => {
                let kept = keep_invalid(&file)?;
                warn!(path = %kept.display(), "the settings file was not valid JSON; kept it under this name");
                ours.clone()
            }
            None => ours.clone(),
        };
        // Field order as in the struct unless unknown keys have to be kept.
        let mut json = if merged == ours {
            serde_json::to_vec_pretty(self)
        } else {
            serde_json::to_vec_pretty(&merged)
        }
        .context("cannot serialize the settings")?;
        json.push(b'\n');
        fsutil::write_atomic(&file, &json, None)
            .with_context(|| format!("cannot write {}", file.display()))
    }
}

/// `theirs` with every value of `ours` written over it, object by object, so
/// keys only `theirs` has stay.
fn merge(theirs: Value, ours: Value) -> Value {
    match (theirs, ours) {
        (Value::Object(mut theirs), Value::Object(ours)) => {
            for (key, value) in ours {
                let merged = match theirs.remove(&key) {
                    Some(old) => merge(old, value),
                    None => value,
                };
                theirs.insert(key, merged);
            }
            Value::Object(theirs)
        }
        (_, ours) => ours,
    }
}

/// Renames an unusable settings file to `config.json.invalid-<unix time>`
/// (with `-<n>` added if that exists) and returns the new name.
fn keep_invalid(file: &Path) -> anyhow::Result<PathBuf> {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    for n in 0..100u32 {
        let name = match n {
            0 => format!("{INVALID_PREFIX}{secs}"),
            n => format!("{INVALID_PREFIX}{secs}-{n}"),
        };
        let kept = file.with_file_name(name);
        if kept.exists() {
            continue;
        }
        fs::rename(file, &kept).with_context(|| {
            format!(
                "cannot keep the invalid settings file {} before saving",
                file.display()
            )
        })?;
        return Ok(kept);
    }
    anyhow::bail!(
        "cannot keep the invalid settings file {} before saving: too many kept copies",
        file.display()
    )
}

/// Parses the settings text, tolerating bad values field by field.
fn parse(text: &str, file: &Path) -> (Config, Option<String>) {
    let error = match serde_json::from_str::<Config>(text) {
        Ok(config) => return (config, None),
        Err(e) => e,
    };
    let user = match serde_json::from_str::<Value>(text) {
        Ok(v @ Value::Object(_)) => v,
        Ok(_) => {
            warn!(path = %file.display(), "the settings file is not a JSON object; using the defaults");
            let problem = format!(
                "{} does not hold a JSON object, so JAI Studio uses the default settings. \
                 Saving settings keeps the file as {INVALID_PREFIX}<time>.",
                file.display()
            );
            return (Config::default(), Some(problem));
        }
        Err(syntax) => {
            warn!(path = %file.display(), error = %syntax, "the settings file is not valid JSON; using the defaults");
            let problem = format!(
                "{} is not valid JSON ({syntax}), so JAI Studio uses the default settings. \
                 Fix the file and restart, or save settings here: the file is then kept as \
                 {INVALID_PREFIX}<time>.",
                file.display()
            );
            return (Config::default(), Some(problem));
        }
    };
    let Ok(mut merged) = serde_json::to_value(Config::default()) else {
        return (Config::default(), Some(format!("{error}")));
    };
    let mut rejected = Vec::new();
    overlay(&mut merged, "", &user, &mut rejected);
    let fields: Vec<String> = rejected
        .iter()
        .map(|p| p.trim_start_matches('/').replace('/', "."))
        .collect();
    warn!(path = %file.display(), fields = %fields.join(", "), %error, "ignoring invalid settings; using their defaults");
    let problem = if fields.is_empty() {
        format!("{} is not in the expected form ({error})", file.display())
    } else {
        format!(
            "{} has invalid values for {}; JAI Studio uses their defaults",
            file.display(),
            fields.join(", ")
        )
    };
    (
        serde_json::from_value(merged).unwrap_or_default(),
        Some(problem),
    )
}

/// Copies the values of `user` into `merged` under the JSON pointer
/// `pointer`, one field at a time, keeping a value only if the whole still
/// reads as a [`Config`]. Unknown fields are skipped; the pointers of
/// rejected values go into `rejected`.
fn overlay(merged: &mut Value, pointer: &str, user: &Value, rejected: &mut Vec<String>) {
    let Some(fields) = user.as_object() else {
        return;
    };
    for (key, value) in fields {
        let child = format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1"));
        let Some(current) = merged.pointer(&child) else {
            continue;
        };
        if current.is_object() && value.is_object() {
            overlay(merged, &child, value, rejected);
            continue;
        }
        let Some(slot) = merged.pointer_mut(&child) else {
            continue;
        };
        let previous = std::mem::replace(slot, value.clone());
        if serde_json::from_value::<Config>(merged.clone()).is_err() {
            if let Some(slot) = merged.pointer_mut(&child) {
                *slot = previous;
            }
            rejected.push(child);
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            version: 1,
            server: ServerSettings::default(),
            idle_unload_minutes: 15,
            backend: BackendPref::Auto,
            llm: LlmSettings::default(),
            image: ImageSettings::default(),
            app: AppSettings::default(),
        }
    }
}

impl Default for ServerSettings {
    fn default() -> Self {
        ServerSettings {
            host: "127.0.0.1".into(),
            port: 5240,
            allow_lan: false,
        }
    }
}

impl Default for LlmSettings {
    fn default() -> Self {
        LlmSettings {
            ctx: 20480,
            gpu_layers: "all".into(),
            mtp: true,
            parallel: 1,
            image_max_tokens: 1536,
            extra_args: Vec::new(),
        }
    }
}

impl Default for ImageSettings {
    fn default() -> Self {
        ImageSettings {
            steps: 4,
            extra_args: Vec::new(),
        }
    }
}

impl Default for AppSettings {
    fn default() -> Self {
        AppSettings {
            close_to_tray: true,
            start_at_login: false,
            file_association: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example in DESIGN.md section 3 is exactly the defaults.
    #[test]
    fn design_example_is_the_default() {
        let json = r#"{
          "version": 1,
          "server": { "host": "127.0.0.1", "port": 5240, "allowLan": false },
          "idleUnloadMinutes": 15,
          "backend": "auto",
          "llm": { "ctx": 20480, "gpuLayers": "all", "mtp": true, "parallel": 1, "imageMaxTokens": 1536, "extraArgs": [] },
          "image": { "steps": 4, "extraArgs": [] },
          "app": { "closeToTray": true, "startAtLogin": false, "fileAssociation": false }
        }"#;
        let parsed: Config = serde_json::from_str(json).unwrap();
        assert_eq!(parsed, Config::default());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path().join("root"));
        assert_eq!(Config::load(&paths), Config::default(), "missing file");

        let mut cfg = Config::default();
        cfg.server.port = 5300;
        cfg.server.allow_lan = true;
        cfg.backend = BackendPref::Vulkan;
        cfg.llm.extra_args = vec!["--threads".into(), "8".into()];
        cfg.app.start_at_login = true;
        cfg.save(&paths).unwrap();
        assert_eq!(Config::load(&paths), cfg);

        let text = std::fs::read_to_string(paths.config_file()).unwrap();
        assert!(
            text.contains("\"allowLan\": true"),
            "camelCase on disk: {text}"
        );
        assert!(text.contains("\"backend\": \"vulkan\""));
        let leftovers = std::fs::read_dir(&paths.root).unwrap().count();
        assert_eq!(leftovers, 1, "only config.json is left behind");
    }

    #[test]
    fn invalid_json_gives_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path());
        std::fs::write(paths.config_file(), "{ not json").unwrap();
        assert_eq!(Config::load(&paths), Config::default());
        std::fs::write(paths.config_file(), "[1, 2]").unwrap();
        assert_eq!(Config::load(&paths), Config::default());
        let (cfg, problem) = Config::load_checked(&paths);
        assert_eq!(cfg, Config::default());
        assert!(problem.unwrap().contains("does not hold a JSON object"));
    }

    /// A hand edit with a trailing comma loads as the defaults; saving other
    /// settings then keeps the edit instead of overwriting it.
    #[test]
    fn a_syntax_error_is_reported_and_kept_when_saving() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path());
        let edited = r#"{"server":{"port":5300},"llm":{"ctx":32768,"extraArgs":["--threads","8"],},"image":{"extraArgs":["--vae-tiling"]}}"#;
        std::fs::write(paths.config_file(), edited).unwrap();
        let (mut cfg, problem) = Config::load_checked(&paths);
        assert_eq!(cfg, Config::default());
        let problem = problem.expect("a problem");
        assert!(problem.contains("is not valid JSON"), "{problem}");
        assert!(problem.contains("line 1"), "{problem}");

        cfg.app.close_to_tray = false;
        cfg.save(&paths).unwrap();
        let kept: Vec<_> = std::fs::read_dir(&paths.root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(INVALID_PREFIX))
            .collect();
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(
            std::fs::read_to_string(paths.root.join(&kept[0])).unwrap(),
            edited
        );
        assert_eq!(Config::load_checked(&paths), (cfg, None));
    }

    /// Keys from a newer version or added by hand survive a save, nested ones
    /// too; known keys take the saved values.
    #[test]
    fn unknown_keys_survive_a_save() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path());
        std::fs::write(
            paths.config_file(),
            r#"{"version": 1, "future": {"knob": true}, "server": {"port": 5300, "tls": "later"}}"#,
        )
        .unwrap();
        let mut cfg = Config::load(&paths);
        assert_eq!(cfg.server.port, 5300);
        cfg.idle_unload_minutes = 5;
        cfg.save(&paths).unwrap();
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(paths.config_file()).unwrap()).unwrap();
        assert_eq!(saved["future"], serde_json::json!({"knob": true}));
        assert_eq!(saved["server"]["tls"], "later");
        assert_eq!(saved["server"]["port"], 5300);
        assert_eq!(saved["idleUnloadMinutes"], 5);
        assert_eq!(Config::load(&paths), cfg);
        let leftovers = std::fs::read_dir(&paths.root).unwrap().count();
        assert_eq!(leftovers, 1, "nothing kept aside for a valid file");
    }

    #[test]
    fn a_bad_field_only_resets_that_field() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path());
        std::fs::write(
            paths.config_file(),
            r#"{"server": {"port": 99999, "host": "0.0.0.0", "allowLan": true},
                "backend": "rocm", "idleUnloadMinutes": 5,
                "llm": {"ctx": "big", "parallel": 2}, "image": null,
                "app": {"closeToTray": false}, "server/port": 1}"#,
        )
        .unwrap();
        let (cfg, problem) = Config::load_checked(&paths);
        let problem = problem.expect("a problem");
        assert!(
            problem.contains("server.port") && problem.contains("llm.ctx"),
            "{problem}"
        );
        assert_eq!(cfg.server.port, 5240);
        assert_eq!(cfg.server.host, "0.0.0.0");
        assert!(cfg.server.allow_lan);
        assert_eq!(cfg.backend, BackendPref::Auto);
        assert_eq!(cfg.idle_unload_minutes, 5);
        assert_eq!(cfg.llm.ctx, 20480);
        assert_eq!(cfg.llm.parallel, 2);
        assert_eq!(cfg.image, ImageSettings::default());
        assert!(!cfg.app.close_to_tray);
    }

    #[test]
    fn missing_and_unknown_fields() {
        let parsed: Config =
            serde_json::from_str(r#"{"server":{"port":5300},"future":true}"#).unwrap();
        assert_eq!(parsed.server.port, 5300);
        assert_eq!(parsed.server.host, "127.0.0.1");
        assert_eq!(parsed.llm, LlmSettings::default());
    }
}
