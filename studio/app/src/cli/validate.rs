//! `jai validate`: `jai_js::validate`, like the Node CLI's validate: the
//! canonical text, then one line per diagnostic. Exits with 1 when the
//! document cannot be read at all. The canonical text keeps a document's
//! control characters (the library flags them), so it is printed through
//! `term::safe`.

use std::process::ExitCode;

use anyhow::Context as _;

use crate::output::{format_diag, js_error};
use crate::ValidateArgs;

pub async fn run(args: ValidateArgs) -> anyhow::Result<ExitCode> {
    let (input, _) = super::doc_input(&args.input)?;
    let result = tokio::task::spawn_blocking(move || jai_js::validate(input))
        .await
        .context("the validation task failed")?
        .map_err(js_error)?;
    if let Some(text) = &result.text {
        outln!("{}", text.trim_end_matches('\n'));
    }
    for d in &result.diagnostics {
        outln!("  {}", format_diag(d));
    }
    Ok(if result.fatal {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}
