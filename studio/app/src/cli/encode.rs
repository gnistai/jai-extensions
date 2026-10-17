//! `jai encode`: an image encoded into a `.jai` document with `jai_js::encode`
//! against the local server, with a report folder like the Node CLI's.
//! Ctrl+C stops the encoder and keeps the best result so far. The output file
//! is checked before the encode starts, and the result text is printed even
//! when the file cannot be written at the end.

use std::process::ExitCode;
use std::sync::Arc;

use serde_json::Value;

use super::{engine_progress, usage};
use crate::encoding::{self, EncodeJob, EncodeSettings};
use crate::output::event_lines;
use crate::term::Live;
use crate::EncodeArgs;

pub async fn run(args: EncodeArgs) -> anyhow::Result<ExitCode> {
    let settings = EncodeSettings {
        generations: args.generations,
        population: args.population,
        seeds: args.seeds.clone(),
        restarts: args.restarts,
        goal: args.goal,
        scale: args.scale,
        thinking: args.think.as_str().to_string(),
    };
    settings.check().map_err(usage)?;
    if !args.image.is_file() {
        return Err(usage(format!("{}: no such file", args.image.display())));
    }
    let out = args
        .out
        .clone()
        .unwrap_or_else(|| encoding::default_out(&args.image));
    encoding::check_out(&out, args.force)?;
    let report_dir = args
        .report
        .clone()
        .unwrap_or_else(|| encoding::default_report(&out));
    let job = EncodeJob {
        image: args.image.clone(),
        out,
        report_dir,
        settings,
    };

    let paths = super::paths()?;
    let live = Live::new();
    let target = encoding::prepare(&job).await?;
    live.out(&format!(
        "Encoding {} ({}x{}) for FLUX.1 schnell; analyst Qwen3.8 27B; population {}, up to {} generations, seeds {}",
        job.source_name(),
        target.width,
        target.height,
        job.settings.population,
        job.settings.generations,
        job.settings
            .seeds
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",")
    ));
    let on_event: Arc<dyn Fn(Value) + Send + Sync> = {
        let live = live.clone();
        Arc::new(move |e: Value| {
            for line in event_lines(&e) {
                live.out(&line);
            }
        })
    };
    let progress_live = live.clone();
    let run_job = job.clone();
    let finished = super::with_server(&paths, &live, true, move |client, cancel| async move {
        let _progress = engine_progress(&progress_live, &client, "Encoding");
        encoding::run(&run_job, target, client.server.clone(), on_event, cancel).await
    })
    .await
    .map_err(|e| e.context("encoding failed"))?;
    let output = &finished.output;

    live.out("");
    live.out(&output.text);
    live.out("");
    for r in output
        .report
        .get("ranking")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        live.out(&format!(
            "  {:<36} {:.3}",
            r.get("model").and_then(Value::as_str).unwrap_or(""),
            r.get("score").and_then(Value::as_f64).unwrap_or(0.0)
        ));
    }
    let error = output
        .error
        .as_deref()
        .or_else(|| output.report.get("error").and_then(Value::as_str))
        .map(|e| format!(" ({e})"))
        .unwrap_or_default();
    live.out("");
    live.out(&format!(
        "Stopped: {}{error}. {:.0} s.",
        output.stop_reason, output.seconds
    ));
    if let Err(e) = &finished.written {
        live.err(&format!(
            "jai: {e:#}. The document is printed above{}.",
            if job.report_dir.join("report.json").is_file() {
                format!(
                    " and kept in {}",
                    job.report_dir.join("report.json").display()
                )
            } else {
                String::new()
            }
        ));
        return Ok(ExitCode::FAILURE);
    }
    live.out(&format!(
        "Wrote {} and {}",
        job.out.display(),
        job.report_dir.join("index.html").display()
    ));
    Ok(exit_for(&output.stop_reason))
}

/// 1 when the encoder was canceled or failed part way (the best result so
/// far is still written), else 0.
pub fn exit_for(stop_reason: &str) -> ExitCode {
    if matches!(stop_reason, "canceled" | "error") {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes() {
        assert_eq!(exit_for("goal"), ExitCode::SUCCESS);
        assert_eq!(exit_for("generations"), ExitCode::SUCCESS);
        assert_eq!(exit_for("plateau"), ExitCode::SUCCESS);
        assert_eq!(exit_for("canceled"), ExitCode::FAILURE);
        assert_eq!(exit_for("error"), ExitCode::FAILURE);
    }
}
