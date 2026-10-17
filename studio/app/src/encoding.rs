//! One encode job, shared by `jai encode` and the GUI's `encode_start`: the
//! target image prepared for the analyst, `jai_js::encode` against a server,
//! and the results on disk, as the Node CLI writes them: the `.jai` file,
//! `target.jpg`, `renders/`, `report.json` and an `index.html` contact sheet.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use jai_js::imageops::{self, PreparedTarget};
use jai_js::{EncodeOptions, EncodeOutput, EncodeSink, EncodeTarget, ServerRef};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::output::{describe_observation, first_feedback, short_model};

/// Largest image accepted for encoding (DESIGN.md section 12).
pub const MAX_IMAGE_BYTES: u64 = 40 << 20;

/// Encode settings, with the CLI's defaults. The GUI receives them as
/// `encode_start` options (camelCase).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EncodeSettings {
    pub generations: u32,
    pub population: u32,
    pub seeds: Vec<u32>,
    pub restarts: u32,
    pub goal: f64,
    pub scale: f64,
    /// `off`, `propose` or `all`.
    pub thinking: String,
}

impl Default for EncodeSettings {
    fn default() -> Self {
        EncodeSettings {
            generations: 8,
            population: 4,
            seeds: vec![1, 2],
            restarts: 1,
            goal: 0.9,
            scale: 1.0,
            thinking: "off".into(),
        }
    }
}

/// Ranges of the settings (the Node CLI's, plus scale and thinking).
pub const GENERATIONS: (u32, u32) = (1, 30);
pub const POPULATION: (u32, u32) = (1, 9);
pub const RESTARTS: (u32, u32) = (1, 5);
pub const MAX_SEEDS: usize = 8;
/// Every FLUX size times this stays within the server's 2048 px side and
/// 2.36 MP limits.
pub const SCALE: (f64, f64) = (0.25, 1.25);

impl EncodeSettings {
    /// Checks every setting against its range.
    pub fn check(&self) -> Result<(), String> {
        let int = |name: &str, v: u32, (lo, hi): (u32, u32)| {
            if (lo..=hi).contains(&v) {
                Ok(())
            } else {
                Err(format!("{name} must be from {lo} to {hi}"))
            }
        };
        int("generations", self.generations, GENERATIONS)?;
        int("population", self.population, POPULATION)?;
        int("restarts", self.restarts, RESTARTS)?;
        if self.seeds.is_empty() || self.seeds.len() > MAX_SEEDS {
            return Err(format!("give 1 to {MAX_SEEDS} seeds"));
        }
        if !(0.0..=1.0).contains(&self.goal) {
            return Err("goal must be from 0 to 1".into());
        }
        if !(SCALE.0..=SCALE.1).contains(&self.scale) {
            return Err(format!("scale must be from {} to {}", SCALE.0, SCALE.1));
        }
        if !["off", "propose", "all"].contains(&self.thinking.as_str()) {
            return Err("thinking must be off, propose or all".into());
        }
        Ok(())
    }
}

/// Where one job reads and writes.
#[derive(Clone, Debug)]
pub struct EncodeJob {
    pub image: PathBuf,
    /// The `.jai` file to write.
    pub out: PathBuf,
    /// `target.jpg`, `renders/`, `report.json` and `index.html` go here.
    pub report_dir: PathBuf,
    pub settings: EncodeSettings,
}

impl EncodeJob {
    pub fn render_dir(&self) -> PathBuf {
        self.report_dir.join("renders")
    }

    /// The image's file name, as the report names it.
    pub fn source_name(&self) -> String {
        self.image
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "image".into())
    }
}

/// `<image without extension>.jai`, next to the image (the Node CLI's default).
pub fn default_out(image: &Path) -> PathBuf {
    image.with_extension("jai")
}

/// `<out>.report` (the Node CLI's default).
pub fn default_report(out: &Path) -> PathBuf {
    let mut s = out.as_os_str().to_owned();
    s.push(".report");
    PathBuf::from(s)
}

/// Checks, before an encode that may run for an hour, that its `.jai` file
/// can be written at the end: `out` is not a folder, is not an existing file
/// unless `force`, and can be opened for writing (an existing file) or
/// created (a new one, in a folder created now). The probe file is removed
/// again. Usage errors exit with 2.
pub fn check_out(out: &Path, force: bool) -> anyhow::Result<()> {
    use crate::cli::usage;
    if out.is_dir() {
        return Err(usage(format!(
            "{} is a folder; give the output a file name, such as photo.jai",
            out.display()
        )));
    }
    if out.exists() {
        if !force {
            return Err(usage(format!(
                "{} exists; pass --force to overwrite it",
                out.display()
            )));
        }
        // Opened for writing, but not truncated: it is replaced at the end.
        std::fs::OpenOptions::new()
            .write(true)
            .open(out)
            .with_context(|| format!("cannot write {}", out.display()))?;
        return Ok(());
    }
    if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out)
        .with_context(|| format!("cannot create {}", out.display()))?;
    let _ = std::fs::remove_file(out);
    Ok(())
}

#[cfg(any(feature = "gui", test))]
/// A name reduced to safe file-name characters (`A-Z a-z 0-9 . _ -`), at
/// most 40 of them; `image` when nothing is left.
pub fn safe_name(raw: &str) -> String {
    let mut name = String::new();
    for c in raw.chars() {
        let c = if c.is_ascii_alphanumeric() || c == '_' || c == '.' {
            c
        } else {
            '-'
        };
        if c == '-' && name.ends_with('-') {
            continue;
        }
        name.push(c);
    }
    let name: String = name.trim_matches(['-', '.']).chars().take(40).collect();
    let name = name.trim_end_matches(['-', '.']);
    if name.is_empty() {
        "image".into()
    } else {
        name.to_string()
    }
}

#[cfg(any(feature = "gui", test))]
/// A folder name for a GUI job: `<yyyymmdd-hhmmss>-<image name>`.
pub fn job_dir_name(stamp: &str, image: &Path) -> String {
    let stem = image
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("{stamp}-{}", safe_name(&stem))
}

/// Reads the image (at most 40 MB) and prepares the analyst's copy: EXIF
/// orientation, at most 1024 px, JPEG, and the 16×16 thumbnail. Writes
/// `target.jpg` into the report folder, creating it and `renders/`.
pub async fn prepare(job: &EncodeJob) -> anyhow::Result<PreparedTarget> {
    let image = job.image.clone();
    let target = tokio::task::spawn_blocking(move || -> anyhow::Result<PreparedTarget> {
        let bytes = crate::cli::read_limited(&image, MAX_IMAGE_BYTES)?;
        imageops::prepare_target(&bytes)
            .with_context(|| format!("cannot read {} as an image", image.display()))
    })
    .await
    .context("the image task failed")??;
    std::fs::create_dir_all(job.render_dir())
        .with_context(|| format!("cannot create {}", job.render_dir().display()))?;
    let target_jpg = job.report_dir.join("target.jpg");
    std::fs::write(&target_jpg, &target.jpeg)
        .with_context(|| format!("cannot write {}", target_jpg.display()))?;
    Ok(target)
}

/// A finished encode: what the encoder produced, and whether its files were
/// written. The output is kept when writing fails, so the document text is
/// never lost with the file.
pub struct Finished {
    pub output: EncodeOutput,
    pub written: anyhow::Result<()>,
}

/// Runs the encoder against `server`, then writes the results. Events go to
/// `on_event` as the engine emits them; renders are saved in `renders/`.
pub async fn run(
    job: &EncodeJob,
    target: PreparedTarget,
    server: ServerRef,
    on_event: Arc<dyn Fn(Value) + Send + Sync>,
    cancel: CancellationToken,
) -> anyhow::Result<Finished> {
    let (width, height) = (target.width, target.height);
    let s = &job.settings;
    let opts = EncodeOptions {
        server,
        target: EncodeTarget {
            url: target.data_url,
            width,
            height,
        },
        target_thumbnail: target.thumb,
        generations: s.generations,
        population: s.population,
        seeds: s.seeds.clone(),
        restarts: s.restarts,
        goal: s.goal,
        time_limit_ms: None,
        render_scale: Some(s.scale),
        thinking: Some(s.thinking.clone()),
        source_name: Some(job.source_name()),
    };
    let sink = EncodeSink {
        on_event,
        render_dir: job.render_dir(),
    };
    let output = jai_js::encode(opts, sink, cancel)
        .await
        .map_err(crate::output::js_error)?;
    let written = write_results(job, &output, width, height);
    Ok(Finished { output, written })
}

/// Writes `report.json` and `index.html`, which both hold the document text,
/// then the `.jai` file: a `.jai` that cannot be written leaves the report.
pub fn write_results(
    job: &EncodeJob,
    output: &EncodeOutput,
    width: u32,
    height: u32,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(&job.report_dir)
        .with_context(|| format!("cannot create {}", job.report_dir.display()))?;
    let report_json = job.report_dir.join("report.json");
    let json = serde_json::to_string_pretty(&output.report)?;
    std::fs::write(&report_json, json)
        .with_context(|| format!("cannot write {}", report_json.display()))?;
    let source = job.source_name();
    let html = contact_sheet(
        &output.report,
        &SheetInfo {
            source: &source,
            width,
            height,
            text: &output.text,
            stop_reason: &output.stop_reason,
            seconds: output.seconds,
        },
    );
    let index = job.report_dir.join("index.html");
    std::fs::write(&index, html).with_context(|| format!("cannot write {}", index.display()))?;
    if let Some(dir) = job.out.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    std::fs::write(&job.out, &output.jai)
        .with_context(|| format!("cannot write {}", job.out.display()))?;
    Ok(())
}

#[cfg(any(feature = "gui", test))]
/// A render file name as `__jai.saveRender` accepts it:
/// `^[A-Za-z0-9._-]{1,120}$`, and not `.` or `..`.
pub fn render_file_ok(name: &str) -> bool {
    (1..=120).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && name != "."
        && name != ".."
}

/// The report-relative path of a render file.
pub fn render_href(file: &str) -> String {
    if file.contains('/') {
        file.to_string()
    } else {
        format!("renders/{file}")
    }
}

#[cfg(any(feature = "gui", test))]
/// Adds `thumb` (from `thumb(file)`) to every render reference in an encode
/// event: an object with a `seed` and a string `file`.
pub fn add_thumbs(v: &mut Value, thumb: &mut dyn FnMut(&str) -> Option<String>) {
    match v {
        Value::Object(map) => {
            let file = map
                .get("file")
                .and_then(Value::as_str)
                .filter(|_| map.contains_key("seed"))
                .map(str::to_string);
            for child in map.values_mut() {
                add_thumbs(child, thumb);
            }
            if let Some(t) = file.as_deref().and_then(&mut *thumb) {
                map.insert("thumb".into(), Value::String(t));
            }
        }
        Value::Array(items) => {
            for item in items {
                add_thumbs(item, thumb);
            }
        }
        _ => {}
    }
}

/// The facts of a finished job that the report may lack.
pub struct SheetInfo<'a> {
    pub source: &'a str,
    pub width: u32,
    pub height: u32,
    pub text: &'a str,
    pub stop_reason: &'a str,
    pub seconds: f64,
}

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn f64_of(v: &Value, key: &str) -> f64 {
    v.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn arr<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

const SHEET_STYLE: &str = "body{font:15px/1.5 system-ui,sans-serif;margin:0 auto;max-width:1200px;padding:16px;color:#1c1917;background:#fafaf9}h1{font-size:1.4rem}pre{white-space:pre-wrap;background:#fff;border:1px solid #d6d3d1;padding:12px;border-radius:8px}.gen{margin:24px 0}.row{display:flex;flex-wrap:wrap;gap:12px}.cand{background:#fff;border:1px solid #d6d3d1;border-radius:8px;padding:8px;width:min(100%,360px)}.cand.best{border:2px solid #c2410c}.imgs{display:flex;gap:6px}figure{margin:0;flex:1}figure img{width:100%;border-radius:4px}figcaption{font-size:.8rem;color:#57534e}.cand p{font-size:.85rem;margin:6px 0 0}.fail{background:#fee2e2;padding:20px;text-align:center}.fb{color:#57534e;font-size:.78rem!important}table{border-collapse:collapse}td,th{padding:2px 10px;text-align:left}";

/// One candidate card, as the Node CLI draws it.
fn card(c: &Value, best_id: &str) -> String {
    let id = str_of(c, "id");
    let mut s = format!(
        "<div class=\"cand{}\"><div class=\"h\"><b>{}</b> {:.3}</div><div class=\"imgs\">",
        if id == best_id { " best" } else { "" },
        esc(id),
        f64_of(c, "fitness")
    );
    for r in arr(c, "renders") {
        let img = match r.get("file").and_then(Value::as_str) {
            Some(f) => format!(
                "<img src=\"{}\" alt=\"\" loading=\"lazy\">",
                esc(&render_href(f))
            ),
            None => "<div class=\"fail\">failed</div>".to_string(),
        };
        let _ = write!(
            s,
            "<figure>{img}<figcaption>{} {:.3}</figcaption></figure>",
            esc(short_model(str_of(r, "model"))),
            f64_of(r, "score")
        );
    }
    let feedback = first_feedback(c).unwrap_or("");
    let prompt = c.get("draft").map(|d| str_of(d, "prompt")).unwrap_or("");
    let _ = write!(
        s,
        "</div><p class=\"fb\">{}</p><p>{}",
        esc(feedback),
        esc(prompt)
    );
    if let Some(refinements) = c.get("refinements").and_then(Value::as_object) {
        for (model, values) in refinements {
            let joined = values
                .as_array()
                .map(|v| {
                    v.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            let _ = write!(
                s,
                "<br><i>+ {}: {}</i>",
                esc(short_model(model)),
                esc(&joined)
            );
        }
    }
    s.push_str("</p></div>");
    s
}

/// The `index.html` contact sheet of a report, like the Node CLI's: the
/// target and what was measured in it, the result text, the ranking, and
/// every generation's candidates with their renders.
pub fn contact_sheet(report: &Value, info: &SheetInfo) -> String {
    let source = match str_of(report, "source") {
        "" => info.source,
        s => s,
    };
    let (width, height) = match report.get("size").and_then(Value::as_array) {
        Some(size) if size.len() == 2 => (
            size[0].as_u64().unwrap_or(u64::from(info.width)),
            size[1].as_u64().unwrap_or(u64::from(info.height)),
        ),
        _ => (u64::from(info.width), u64::from(info.height)),
    };
    let text = match str_of(report, "text") {
        "" => info.text,
        t => t,
    };
    let stop = match str_of(report, "stopReason") {
        "" => info.stop_reason,
        s => s,
    };
    let seconds = report
        .get("seconds")
        .and_then(Value::as_f64)
        .unwrap_or(info.seconds);
    let target = report
        .get("target")
        .filter(|t| t.is_object())
        .map(describe_observation)
        .unwrap_or_default();
    let best_id = report.get("best").map(|b| str_of(b, "id")).unwrap_or("");
    let error = str_of(report, "error");
    let analyst = str_of(report, "analyst");

    let mut h = String::new();
    let _ = write!(
        h,
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>JAI encoding: {src}</title>\n<style>{SHEET_STYLE}</style></head><body>\n<h1>JAI encoding of {src}</h1>\n<div class=\"row\"><figure style=\"max-width:360px\"><img src=\"target.jpg\" alt=\"Target image\"><figcaption>Target, {width}×{height}</figcaption></figure><pre style=\"flex:1;min-width:260px\">{target}</pre></div>\n<h2>Result</h2><pre>{text}</pre>\n<table><tr><th>Model</th><th>Score</th></tr>",
        src = esc(source),
        target = esc(&target),
        text = esc(text),
    );
    for r in arr(report, "ranking") {
        let _ = write!(
            h,
            "<tr><td>{}</td><td>{:.3}</td></tr>",
            esc(str_of(r, "model")),
            f64_of(r, "score")
        );
    }
    let _ = write!(
        h,
        "</table>\n<p>Stopped: {}{} in {:.0} s.{} Fitness is measured against the target's boxes, horizon and colours.</p>\n",
        esc(stop),
        if error.is_empty() {
            String::new()
        } else {
            format!(" ({})", esc(error))
        },
        seconds,
        if analyst.is_empty() {
            String::new()
        } else {
            format!(" Analyst {}.", esc(analyst))
        }
    );
    let candidates = arr(report, "candidates");
    for g in arr(report, "generations") {
        let title = if str_of(g, "phase") == "refine" {
            "Refinement".to_string()
        } else {
            format!("Lineage {}, generation", f64_of(g, "lineage"))
        };
        let _ = write!(
            h,
            "<section class=\"gen\"><h3>{title} {}: best {:.3}</h3>",
            f64_of(g, "index"),
            f64_of(g, "bestFitness")
        );
        let notes = str_of(g, "notes");
        if !notes.is_empty() {
            let _ = write!(h, "<p>{}</p>", esc(notes));
        }
        h.push_str("<div class=\"row\">");
        for id in arr(g, "candidates").iter().filter_map(Value::as_str) {
            if let Some(c) = candidates.iter().find(|c| str_of(c, "id") == id) {
                h.push_str(&card(c, best_id));
            }
        }
        h.push_str("</div></section>\n");
    }
    h.push_str("</body></html>");
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn settings_ranges() {
        let s = EncodeSettings::default();
        assert!(s.check().is_ok());
        let bad = |f: fn(&mut EncodeSettings)| {
            let mut s = EncodeSettings::default();
            f(&mut s);
            s.check().unwrap_err()
        };
        assert!(bad(|s| s.generations = 0).contains("generations"));
        assert!(bad(|s| s.population = 10).contains("population"));
        assert!(bad(|s| s.restarts = 6).contains("restarts"));
        assert!(bad(|s| s.seeds.clear()).contains("seeds"));
        assert!(bad(|s| s.goal = 1.5).contains("goal"));
        assert!(bad(|s| s.scale = 2.0).contains("scale"));
        assert!(bad(|s| s.thinking = "sometimes".into()).contains("thinking"));
        let s: EncodeSettings = serde_json::from_value(json!({
            "generations": 3, "population": 2, "seeds": [7], "restarts": 1,
            "goal": 0.8, "scale": 0.5, "thinking": "propose"
        }))
        .unwrap();
        assert!(s.check().is_ok());
    }

    #[test]
    fn output_names() {
        assert_eq!(
            default_out(Path::new("/p/photo.png")),
            PathBuf::from("/p/photo.jai")
        );
        assert_eq!(default_out(Path::new("photo")), PathBuf::from("photo.jai"));
        assert_eq!(
            default_report(Path::new("/p/photo.jai")),
            PathBuf::from("/p/photo.jai.report")
        );
        let p = Path::new("/x/My Holiday (1).JPG");
        assert_eq!(
            job_dir_name("20261017-110509", p),
            "20261017-110509-My-Holiday-1"
        );
        assert_eq!(
            job_dir_name("20261017-110509", Path::new("/x/@@@.png")),
            "20261017-110509-image"
        );
        let long = format!("/x/{}.png", "a".repeat(80));
        assert_eq!(job_dir_name("s", Path::new(&long)).len(), 2 + 40);
        assert_eq!(safe_name("..My  photo!.."), "My-photo");
        assert_eq!(safe_name("∂∂"), "image");
    }

    #[test]
    fn render_files() {
        assert!(render_file_ok("001-l1-g1-c1-flux-schnell-s1.jpg"));
        assert!(!render_file_ok("../evil.jpg"));
        assert!(!render_file_ok("a/b.jpg"));
        assert!(!render_file_ok(".."));
        assert!(!render_file_ok(""));
        assert!(!render_file_ok(&"a".repeat(121)));
        assert_eq!(render_href("001.jpg"), "renders/001.jpg");
        assert_eq!(render_href("renders/001.jpg"), "renders/001.jpg");
    }

    #[test]
    fn thumbs_on_render_references() {
        let mut e = json!({"type": "candidate", "candidate": {"id": "c1", "renders": [
            {"model": "m", "seed": 1, "file": "001.jpg", "score": 0.5},
            {"model": "m", "seed": 2, "file": null, "error": "timeout", "score": 0}
        ], "draft": {"prompt": "a file"}}});
        let mut seen = Vec::new();
        add_thumbs(&mut e, &mut |f| {
            seen.push(f.to_string());
            Some(format!("data:image/jpeg;base64,{f}"))
        });
        assert_eq!(seen, ["001.jpg"]);
        let renders = &e["candidate"]["renders"];
        assert_eq!(renders[0]["thumb"], "data:image/jpeg;base64,001.jpg");
        assert!(renders[1].get("thumb").is_none());
        assert!(e["candidate"]["draft"].get("thumb").is_none());
    }

    fn sample_report() -> Value {
        json!({
            "source": "castle <1>.png", "size": [1344, 768],
            "analyst": "local/qwen3.8-27b",
            "target": {"subjects": [{"name": "castle", "box": {"ymin": 0.2, "xmin": 0.1, "ymax": 0.7, "xmax": 0.5}}],
                       "horizon": 0.6, "horizonShape": "flat", "camera": "wide", "lighting": "dusk", "medium": "photo"},
            "text": "a castle at dusk\nstyle=photo",
            "ranking": [{"model": "black-forest-labs/flux-schnell", "score": 0.734}],
            "stopReason": "goal", "error": null, "seconds": 312.4,
            "generations": [
                {"lineage": 1, "index": 1, "phase": "evolve", "candidates": ["c1", "c2"], "best": "c2", "bestFitness": 0.734, "notes": "move it left", "spent": 0}
            ],
            "best": {"id": "c2"},
            "candidates": [
                {"id": "c1", "fitness": 0.5, "draft": {"prompt": "a castle"}, "refinements": {},
                 "renders": [{"model": "black-forest-labs/flux-schnell", "seed": 1, "file": "001.jpg", "score": 0.5, "measure": {"feedback": "too small"}}]},
                {"id": "c2", "fitness": 0.734, "draft": {"prompt": "a big castle"}, "refinements": {"black-forest-labs/flux-schnell": ["dusk"]},
                 "renders": [{"model": "black-forest-labs/flux-schnell", "seed": 1, "file": null, "score": 0}]}
            ]
        })
    }

    #[test]
    fn contact_sheet_like_the_node_cli() {
        let info = SheetInfo {
            source: "fallback.png",
            width: 1,
            height: 1,
            text: "fallback",
            stop_reason: "generations",
            seconds: 1.0,
        };
        let html = contact_sheet(&sample_report(), &info);
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("<title>JAI encoding: castle &lt;1&gt;.png</title>"));
        assert!(html.contains("Target, 1344×768"));
        assert!(html.contains("- castle: x 10%–50%, y 20%–70%"));
        assert!(html.contains("<pre>a castle at dusk\nstyle=photo</pre>"));
        assert!(html.contains("<tr><td>black-forest-labs/flux-schnell</td><td>0.734</td></tr>"));
        assert!(html.contains("Stopped: goal in 312 s. Analyst local/qwen3.8-27b."));
        assert!(html.contains("<h3>Lineage 1, generation 1: best 0.734</h3><p>move it left</p>"));
        assert!(html.contains("<img src=\"renders/001.jpg\" alt=\"\" loading=\"lazy\">"));
        assert!(html.contains("<div class=\"cand best\"><div class=\"h\"><b>c2</b> 0.734</div>"));
        assert!(html.contains("<div class=\"fail\">failed</div>"));
        assert!(html.contains("<p class=\"fb\">too small</p>"));
        assert!(html.contains("<br><i>+ flux-schnell: dusk</i>"));
        assert!(html.ends_with("</body></html>"));

        // A report without the optional fields falls back to the job's facts.
        let html = contact_sheet(&json!({}), &info);
        assert!(html.contains("JAI encoding of fallback.png"));
        assert!(html.contains("Target, 1×1"));
        assert!(html.contains("Stopped: generations in 1 s."));
    }

    /// Through serde, as the host hands it over, so optional fields the
    /// host adds later need no change here.
    fn sample_output() -> EncodeOutput {
        serde_json::from_value(json!({
            "text": "a castle", "jai": [0x1f, 0x8b, 1, 2], "stopReason": "goal", "seconds": 12.0,
            "best": {"id": "c2", "fitness": 0.9}, "report": sample_report()
        }))
        .unwrap()
    }

    #[test]
    fn output_checked_before_encoding() {
        use crate::cli::exit_code;
        let dir = tempfile::tempdir().unwrap();
        // A new file in a new folder: the folder is made, the probe removed.
        let out = dir.path().join("results").join("castle.jai");
        check_out(&out, false).unwrap();
        assert!(dir.path().join("results").is_dir() && !out.exists());
        // A folder is a usage error, even with --force.
        let e = check_out(&dir.path().join("results"), true).unwrap_err();
        assert_eq!(exit_code(&e), 2, "{e}");
        assert!(e.to_string().contains("is a folder"), "{e}");
        // An existing file needs --force, and then is left as it was.
        std::fs::write(&out, b"old").unwrap();
        assert_eq!(exit_code(&check_out(&out, false).unwrap_err()), 2);
        check_out(&out, true).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"old");
        // A read-only file fails now, not after the encode.
        let mut perms = std::fs::metadata(&out).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&out, perms).unwrap();
        let can_write_anyway = std::fs::OpenOptions::new().write(true).open(&out).is_ok();
        if !can_write_anyway {
            let e = check_out(&out, true).unwrap_err();
            assert_eq!(exit_code(&e), 1, "{e}");
            assert!(e.to_string().contains("cannot write"), "{e}");
        }
        // A folder that cannot be made: its parent is a file.
        let e = check_out(
            &dir.path().join("results").join("castle.jai").join("x.jai"),
            false,
        )
        .unwrap_err();
        assert!(e.to_string().contains("cannot create"), "{e}");
    }

    #[test]
    fn report_survives_an_unwritable_jai() {
        let dir = tempfile::tempdir().unwrap();
        // The output is a folder by now, so the .jai cannot be written.
        let out = dir.path().join("castle.jai");
        std::fs::create_dir(&out).unwrap();
        let job = EncodeJob {
            image: dir.path().join("castle.png"),
            out,
            report_dir: dir.path().join("report"),
            settings: EncodeSettings::default(),
        };
        let e = write_results(&job, &sample_output(), 1344, 768).unwrap_err();
        assert!(e.to_string().contains("cannot write"), "{e}");
        let report = std::fs::read_to_string(job.report_dir.join("report.json")).unwrap();
        assert!(report.contains("a castle at dusk"), "{report}");
        assert!(job.report_dir.join("index.html").is_file());
    }

    #[test]
    fn writes_results() {
        let dir = tempfile::tempdir().unwrap();
        let job = EncodeJob {
            image: dir.path().join("castle.png"),
            out: dir.path().join("out").join("castle.jai"),
            report_dir: dir.path().join("castle.jai.report"),
            settings: EncodeSettings::default(),
        };
        let output = sample_output();
        write_results(&job, &output, 1344, 768).unwrap();
        assert_eq!(std::fs::read(&job.out).unwrap(), output.jai);
        let report: Value =
            serde_json::from_slice(&std::fs::read(job.report_dir.join("report.json")).unwrap())
                .unwrap();
        assert_eq!(report, sample_report());
        let html = std::fs::read_to_string(job.report_dir.join("index.html")).unwrap();
        assert!(html.contains("JAI encoding of castle &lt;1&gt;.png"));
    }
}
