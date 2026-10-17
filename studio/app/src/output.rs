//! Plain-text formatting shared by the CLI, the tray and the encode report:
//! sizes, speeds, durations, diagnostics, engine states, encode events and
//! timestamps. Everything here is pure, so it is unit-tested on every OS.

use jai_engines::{EngineKind, EngineState, SchedulerStatus};
use jai_js::{Diag, ValidateResult};
use serde_json::Value;

/// Decimal units, as disk and download sizes are usually shown: "30.3 GB",
/// "928 MB", "512 B".
pub fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "kB", "MB", "GB", "TB", "PB"];
    let mut value = n as f64;
    let mut i = 0;
    while value >= 1000.0 && i + 1 < UNITS.len() {
        value /= 1000.0;
        i += 1;
    }
    if i == 0 {
        return format!("{n} B");
    }
    let decimals: i32 = if value < 99.95 { 1 } else { 0 };
    let scale = 10f64.powi(decimals);
    // Rounding can reach 1000 (999.96 MB): show the next unit instead.
    if (value * scale).round() / scale >= 1000.0 && i + 1 < UNITS.len() {
        return format!("{:.1} {}", value / 1000.0, UNITS[i + 1]);
    }
    format!("{value:.prec$} {}", UNITS[i], prec = decimals as usize)
}

/// "85.2 MB/s".
pub fn format_speed(bytes_per_second: u64) -> String {
    format!("{}/s", format_bytes(bytes_per_second))
}

/// "42s", "3m 05s", "1h 02m".
pub fn format_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// "12%" of a total; 0% when the total is unknown.
pub fn percent(done: u64, total: u64) -> String {
    if total == 0 {
        return "0%".into();
    }
    let p = (done as f64 / total as f64 * 100.0).clamp(0.0, 100.0);
    format!("{}%", p.floor() as u64)
}

/// One diagnostic as the Node CLI prints it (`code (key)`), plus what was
/// requested and used, and the reason, when the diagnostic carries them.
pub fn format_diag(d: &Diag) -> String {
    let mut s = d.code.clone();
    if let Some(key) = &d.key {
        s.push_str(&format!(" ({key})"));
    }
    let mut details = Vec::new();
    if let Some(r) = &d.requested {
        details.push(format!("requested {r}"));
    }
    if let Some(a) = &d.actual {
        details.push(format!("used {a}"));
    }
    if let Some(r) = &d.reason {
        details.push(r.clone());
    }
    if !details.is_empty() {
        s.push_str(": ");
        s.push_str(&details.join(", "));
    }
    s
}

/// Why a document cannot be read (`ValidateResult.fatal`), in the words of
/// `jai_js::render`: `The document cannot be read: malformed-container
/// (reason)`. The fatal codes are `malformed-container` and `invalid-json`
/// (a `.jai` file that is not the file form) and `undecodable` (a
/// `data:image/jai` body that is not UTF-8 text within the text bound).
pub fn fatal_message(validated: &ValidateResult) -> String {
    let fatal = validated.diagnostics.iter().find(|d| {
        matches!(
            d.code.as_str(),
            "malformed-container" | "invalid-json" | "undecodable"
        )
    });
    match fatal {
        Some(Diag {
            code,
            reason: Some(reason),
            ..
        }) => format!("The document cannot be read: {code} ({reason})"),
        Some(d) => format!("The document cannot be read: {}", d.code),
        None => "The document cannot be read".into(),
    }
}

/// The state of one engine in a word: the scheduler's state for the loaded
/// engine, `waiting` when requests queue for the other one, else `stopped`.
pub fn engine_word(status: &SchedulerStatus, kind: EngineKind) -> &'static str {
    if status.loaded == Some(kind) {
        return match status.state {
            EngineState::Stopped => "stopped",
            EngineState::Starting => "starting",
            EngineState::Ready => "ready",
            EngineState::Stopping => "stopping",
            EngineState::Failed => "failed",
        };
    }
    let waiting = match kind {
        EngineKind::Llm => status.waiting_llm,
        EngineKind::Image => status.waiting_image,
    };
    if waiting > 0 {
        "waiting"
    } else {
        "stopped"
    }
}

/// "LLM ready · Image stopped".
pub fn engines_summary(status: &SchedulerStatus) -> String {
    format!(
        "LLM {} · Image {}",
        engine_word(status, EngineKind::Llm),
        engine_word(status, EngineKind::Image)
    )
}

#[cfg(any(feature = "gui", test))]
/// The tray tooltip: "JAI Studio: LLM ready · Image stopped".
pub fn tray_tooltip(status: &SchedulerStatus) -> String {
    format!("JAI Studio: {}", engines_summary(status))
}

/// `Math.round(|x| * 100)%`, as the encoder's `pct`.
fn pct(x: f64) -> String {
    format!("{}%", (x.abs() * 100.0).round() as i64)
}

fn num(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64)
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// An encoder `Observation` in words, as `describeObservation` in
/// `src/encoder/metrics.ts`.
pub fn describe_observation(o: &Value) -> String {
    let mut lines = Vec::new();
    for s in o
        .get("subjects")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(b) = s.get("box").filter(|b| b.is_object()) else {
            continue;
        };
        let (Some(ymin), Some(xmin), Some(ymax), Some(xmax)) = (
            num(b, "ymin"),
            num(b, "xmin"),
            num(b, "ymax"),
            num(b, "xmax"),
        ) else {
            continue;
        };
        lines.push(format!(
            "- {}: x {}–{}, y {}–{} of the frame (width {}, height {})",
            text(s, "name"),
            pct(xmin),
            pct(xmax),
            pct(ymin),
            pct(ymax),
            pct(xmax - xmin),
            pct(ymax - ymin)
        ));
    }
    if let Some(h) = num(o, "horizon") {
        let shape = text(o, "horizonShape");
        let shape = if shape.is_empty() {
            String::new()
        } else {
            format!(", {shape}")
        };
        lines.push(format!("- horizon: {} from the top{shape}", pct(h)));
    }
    lines.push(format!(
        "- camera: {}; light: {}; medium: {}",
        text(o, "camera"),
        text(o, "lighting"),
        text(o, "medium")
    ));
    lines.join("\n")
}

/// An error from the JavaScript host as a person reads it: without the
/// stack trace and the `Error: ` prefix of JavaScript errors.
pub fn js_message(raw: &str) -> String {
    let kept: Vec<&str> = raw
        .lines()
        .filter(|l| !l.trim_start().starts_with("at "))
        .collect();
    let text = kept.join("\n").replace(": Error: ", ": ");
    let text = text.trim();
    text.strip_prefix("Error: ").unwrap_or(text).to_string()
}

/// [`js_message`] as an error; the full text goes to the debug log.
pub fn js_error(e: anyhow::Error) -> anyhow::Error {
    let raw = format!("{e:#}");
    tracing::debug!("JavaScript host error: {raw}");
    anyhow::anyhow!(js_message(&raw))
}

/// A path as people read it: without the `\\?\` verbatim prefix that
/// canonical Windows paths carry.
pub fn display_path(path: &std::path::Path) -> String {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    match s.strip_prefix(r"\\?\") {
        Some(rest) => rest.to_string(),
        None => s.into_owned(),
    }
}

/// The part of a model id after its owner: `flux-schnell` for
/// `black-forest-labs/flux-schnell`.
pub fn short_model(id: &str) -> &str {
    id.split_once('/').map_or(id, |(_, name)| name)
}

/// At most `n` characters of `s`.
pub fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// The lines the Node CLI prints for one encode event (`bin/jai.mjs`), minus
/// spending, which is always zero locally. Unknown events print nothing.
pub fn event_lines(e: &Value) -> Vec<String> {
    let mut out = Vec::new();
    match text(e, "type") {
        "phase" => {
            let lineage = num(e, "lineage")
                .map(|l| format!(" (lineage {l})"))
                .unwrap_or_default();
            out.push(String::new());
            out.push(format!("== {}{lineage}", text(e, "phase")));
        }
        "target" => {
            let d = e
                .get("observation")
                .map(describe_observation)
                .unwrap_or_default();
            out.push(format!("  target: {}", d.replace('\n', "\n          ")));
        }
        "candidate" => {
            let Some(c) = e.get("candidate") else {
                return out;
            };
            let scores = c
                .get("perModel")
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .map(|(model, s)| {
                            format!("{} {:.3}", short_model(model), s.as_f64().unwrap_or(0.0))
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            out.push(format!(
                "  {:<16} {:.3}  [{scores}]",
                text(c, "id"),
                num(c, "fitness").unwrap_or(0.0)
            ));
            if let Some(fb) = first_feedback(c) {
                out.push(format!("     {}", clip(fb, 240)));
            }
        }
        "generation" => {
            let Some(g) = e.get("generation") else {
                return out;
            };
            out.push(format!(
                "  -> l{} gen {} best {} {:.3}",
                num(g, "lineage").unwrap_or(0.0),
                num(g, "index").unwrap_or(0.0),
                text(g, "best"),
                num(g, "bestFitness").unwrap_or(0.0)
            ));
            let notes = text(g, "notes");
            if !notes.is_empty() {
                out.push(format!("     {}", clip(notes, 300)));
            }
        }
        "log" => out.push(format!("  {}", text(e, "message"))),
        _ => {}
    }
    out
}

/// The measured feedback of a candidate's first inspected render.
pub fn first_feedback(candidate: &Value) -> Option<&str> {
    candidate
        .get("renders")?
        .as_array()?
        .iter()
        .find_map(|r| r.get("measure")?.get("feedback")?.as_str())
}

#[cfg(any(feature = "gui", test))]
/// `yyyymmdd-hhmmss` for a Unix time in UTC.
pub fn utc_stamp(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let rem = unix_secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

#[cfg(any(feature = "gui", test))]
/// Days since 1970-01-01 to (year, month, day); Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(any(feature = "gui", test))]
/// Seconds since the Unix epoch.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(any(feature = "gui", test))]
/// `yyyymmdd-hhmmss` in local time, for folder names; UTC when the local time
/// is unavailable.
pub fn local_stamp() -> String {
    local_stamp_impl().unwrap_or_else(|| utc_stamp(unix_now()))
}

#[cfg(all(unix, any(feature = "gui", test)))]
fn local_stamp_impl() -> Option<String> {
    let now = libc::time_t::try_from(unix_now()).ok()?;
    // SAFETY: localtime_r only writes the tm we pass; a zeroed tm is valid.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&now, &mut tm).is_null() };
    ok.then(|| {
        format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    })
}

#[cfg(all(windows, any(feature = "gui", test)))]
fn local_stamp_impl() -> Option<String> {
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    // SAFETY: GetLocalTime fills the SYSTEMTIME we pass.
    let t = unsafe {
        let mut t = std::mem::zeroed();
        GetLocalTime(&mut t);
        t
    };
    Some(format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    ))
}

#[cfg(all(not(any(unix, windows)), any(feature = "gui", test)))]
fn local_stamp_impl() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bytes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1500), "1.5 kB");
        assert_eq!(format_bytes(927_607_488), "928 MB");
        assert_eq!(format_bytes(10_934_860_704), "10.9 GB");
        assert_eq!(format_bytes(30_307_221_044), "30.3 GB");
        assert_eq!(format_bytes(5_061_584_064), "5.1 GB");
        assert_eq!(format_bytes(999_960_000), "1.0 GB");
        assert_eq!(format_bytes(120_500_000_000), "120 GB");
        assert_eq!(format_speed(85_200_000), "85.2 MB/s");
    }

    #[test]
    fn durations_and_percent() {
        assert_eq!(format_duration(42), "42s");
        assert_eq!(format_duration(185), "3m 05s");
        assert_eq!(format_duration(3720), "1h 02m");
        assert_eq!(percent(1, 3), "33%");
        assert_eq!(percent(5, 0), "0%");
        assert_eq!(percent(10, 10), "100%");
    }

    #[test]
    fn diagnostics() {
        let d = |code: &str| Diag {
            code: code.into(),
            key: None,
            requested: None,
            actual: None,
            reason: None,
        };
        assert_eq!(format_diag(&d("invalid-json")), "invalid-json");
        let mut a = d("adjusted-value");
        a.key = Some("aspect".into());
        a.requested = Some("7:3".into());
        a.actual = Some("21:9".into());
        assert_eq!(
            format_diag(&a),
            "adjusted-value (aspect): requested 7:3, used 21:9"
        );
        let mut m = d("malformed-container");
        m.reason = Some("truncated gzip".into());
        assert_eq!(format_diag(&m), "malformed-container: truncated gzip");
        // An unreadable document, in the host's words: the first fatal code.
        let validated = |diagnostics: Vec<Diag>| ValidateResult {
            ok: false,
            fatal: true,
            text: None,
            diagnostics,
        };
        assert_eq!(
            fatal_message(&validated(vec![d("adjusted-value"), m.clone()])),
            "The document cannot be read: malformed-container (truncated gzip)"
        );
        for code in ["invalid-json", "undecodable"] {
            assert_eq!(
                fatal_message(&validated(vec![d(code), d("missing-prompt")])),
                format!("The document cannot be read: {code}")
            );
        }
        assert_eq!(
            fatal_message(&validated(vec![])),
            "The document cannot be read"
        );
    }

    fn status(loaded: Option<EngineKind>, state: EngineState) -> SchedulerStatus {
        SchedulerStatus {
            loaded,
            state,
            active: 0,
            waiting_llm: 0,
            waiting_image: 0,
            mtp: true,
            last_error: None,
            swaps: 0,
            since_ms: 0,
        }
    }

    #[test]
    fn engine_states() {
        let s = status(None, EngineState::Stopped);
        assert_eq!(tray_tooltip(&s), "JAI Studio: LLM stopped · Image stopped");
        let s = status(Some(EngineKind::Llm), EngineState::Ready);
        assert_eq!(engines_summary(&s), "LLM ready · Image stopped");
        let mut s = status(Some(EngineKind::Image), EngineState::Starting);
        s.waiting_llm = 2;
        assert_eq!(engines_summary(&s), "LLM waiting · Image starting");
        let s = status(Some(EngineKind::Image), EngineState::Failed);
        assert_eq!(engine_word(&s, EngineKind::Image), "failed");
    }

    #[test]
    fn observation_in_words() {
        let o = json!({
            "subjects": [
                {"name": "castle", "box": {"ymin": 0.2, "xmin": 0.1, "ymax": 0.7, "xmax": 0.55}},
                {"name": "moon", "box": null}
            ],
            "horizon": 0.62, "horizonShape": "flat",
            "camera": "wide, eye level", "lighting": "dusk", "medium": "photo"
        });
        assert_eq!(
            describe_observation(&o),
            "- castle: x 10%–55%, y 20%–70% of the frame (width 45%, height 50%)\n\
             - horizon: 62% from the top, flat\n\
             - camera: wide, eye level; light: dusk; medium: photo"
        );
        let o = json!({"subjects": [], "horizon": null, "horizonShape": "", "camera": "c", "lighting": "l", "medium": "m"});
        assert_eq!(describe_observation(&o), "- camera: c; light: l; medium: m");
    }

    #[test]
    fn encode_events() {
        assert_eq!(
            event_lines(&json!({"type": "phase", "phase": "describe", "lineage": 1})),
            ["", "== describe (lineage 1)"]
        );
        assert_eq!(
            event_lines(&json!({"type": "phase", "phase": "inspect"})),
            ["", "== inspect"]
        );
        let c = json!({"type": "candidate", "candidate": {
            "id": "l1-g1-c2", "fitness": 0.61234,
            "perModel": {"black-forest-labs/flux-schnell": 0.61234},
            "renders": [
                {"model": "black-forest-labs/flux-schnell", "seed": 1, "file": null, "measure": null},
                {"model": "black-forest-labs/flux-schnell", "seed": 2, "file": "002.jpg", "measure": {"feedback": "move the castle left"}}
            ]
        }});
        assert_eq!(
            event_lines(&c),
            [
                "  l1-g1-c2         0.612  [flux-schnell 0.612]",
                "     move the castle left"
            ]
        );
        let g = json!({"type": "generation", "generation": {"lineage": 1, "index": 2, "best": "l1-g2-c1", "bestFitness": 0.7, "notes": "", "spent": 0}});
        assert_eq!(event_lines(&g), ["  -> l1 gen 2 best l1-g2-c1 0.700"]);
        let t = json!({"type": "target", "observation": {"subjects": [], "horizon": 0.5, "horizonShape": "", "camera": "c", "lighting": "l", "medium": "m"}});
        assert_eq!(
            event_lines(&t),
            ["  target: - horizon: 50% from the top\n          - camera: c; light: l; medium: m"]
        );
        assert_eq!(
            event_lines(&json!({"type": "log", "message": "retrying"})),
            ["  retrying"]
        );
        assert!(event_lines(&json!({"type": "spend", "spent": 0, "limit": 0})).is_empty());
    }

    #[test]
    fn stamps() {
        assert_eq!(utc_stamp(0), "19700101-000000");
        // 2026-10-17 11:05:09 UTC
        assert_eq!(utc_stamp(1_792_235_109), "20261017-110509");
        // A leap day.
        assert_eq!(utc_stamp(951_782_400), "20000229-000000");
        let local = local_stamp();
        assert_eq!(local.len(), 15, "{local}");
        assert_eq!(&local[8..9], "-");
    }

    #[test]
    fn display_paths() {
        use std::path::Path;
        assert_eq!(
            display_path(Path::new(r"\\?\C:\Users\a\.lmstudio\models\q.gguf")),
            r"C:\Users\a\.lmstudio\models\q.gguf"
        );
        assert_eq!(
            display_path(Path::new(r"\\?\UNC\nas\models\q.gguf")),
            r"\\nas\models\q.gguf"
        );
        assert_eq!(display_path(Path::new("/home/a/q.gguf")), "/home/a/q.gguf");
    }

    #[test]
    fn javascript_errors() {
        let raw = "Error: Image generation failed (HTTP 409): Download the models first.\n    at RenderError (jai-host/host.mjs:1861:11)\n    at render (jai-host/host.mjs:2097:72)";
        assert_eq!(
            js_message(raw),
            "Image generation failed (HTTP 409): Download the models first."
        );
        assert_eq!(
            js_message("encoding failed: Error: Chat completion failed\n    at encode2 (x:1:2)"),
            "encoding failed: Chat completion failed"
        );
        assert_eq!(
            js_message("TypeError: fetch blocked"),
            "TypeError: fetch blocked"
        );
        assert_eq!(
            js_error(anyhow::anyhow!("Error: x\n    at y")).to_string(),
            "x"
        );
    }

    #[test]
    fn models_and_clipping() {
        assert_eq!(
            short_model("black-forest-labs/flux-schnell"),
            "flux-schnell"
        );
        assert_eq!(short_model("local"), "local");
        assert_eq!(clip("héllo", 2), "hé");
    }
}
