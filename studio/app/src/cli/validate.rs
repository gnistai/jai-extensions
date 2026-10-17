//! `jai validate`: `jai_js::validate`, like the Node CLI's validate: the
//! canonical text, then one line per diagnostic (for a `data:image/jai` URL,
//! its own advisories too). Exits with 1 when the document cannot be read
//! (`malformed-container`, `invalid-json`, `undecodable`, or a
//! `data:image/jai` URL that carries no document), with only the decoder's
//! diagnostics (and the URL's advisories). The canonical text keeps a
//! document's control characters (the library flags them), so it is printed
//! through `term::safe`.

use std::process::ExitCode;

use crate::output::format_diag;
use crate::ValidateArgs;

pub async fn run(args: ValidateArgs) -> anyhow::Result<ExitCode> {
    let input = super::doc_input(&args.input)?;
    let result = super::validate_doc(input).await?;
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
