//! `jai render`: a document rendered with `jai_js::render` against the local
//! server, written to an image file.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context as _;
use jai_js::RenderOptions;

use super::{engine_progress, usage};
use crate::output::{format_diag, js_error};
use crate::term::Live;
use crate::RenderArgs;

pub async fn run(args: RenderArgs) -> anyhow::Result<ExitCode> {
    if let Some(seed) = &args.seed {
        check_seed(seed)?;
    }
    let format = match args.out.as_deref() {
        Some(out) => format_for(out)?,
        None => "png",
    };
    let (input, _) = super::doc_input(&args.input)?;
    let paths = super::paths()?;
    let live = Live::new();
    let seed = args.seed.clone();
    let progress_live = live.clone();
    let out = super::with_server(&paths, &live, false, move |client, _| async move {
        let _progress = engine_progress(&progress_live, &client, "Rendering");
        jai_js::render(
            input,
            RenderOptions {
                server: client.server.clone(),
                seed,
                model: None,
                output_format: Some(format.to_string()),
                scale: None,
            },
        )
        .await
        .map_err(js_error)
    })
    .await?;
    let file = match args.out {
        Some(f) => f,
        None => unique_path(PathBuf::from(format!(
            "render-{}.{}",
            out.seed,
            extension_for(&out.mime)
        ))),
    };
    std::fs::write(&file, &out.bytes)
        .with_context(|| format!("cannot write {}", file.display()))?;
    // The sent prompt and the diagnostics quote the document.
    outln!(
        "{} -> {} ({}x{}, seed {})",
        out.model_label,
        file.display(),
        out.width,
        out.height,
        out.seed
    );
    outln!("sent: {}", out.sent_prompt);
    for d in &out.diagnostics {
        outln!("  {}", format_diag(d));
    }
    Ok(ExitCode::SUCCESS)
}

/// A seed is a non-negative integer of any size; the renderer reduces it
/// modulo 2^32 and says so.
pub fn check_seed(seed: &str) -> anyhow::Result<()> {
    if !seed.is_empty() && seed.len() <= 40 && seed.bytes().all(|b| b.is_ascii_digit()) {
        Ok(())
    } else {
        Err(usage(format!(
            "--seed must be a non-negative integer, not {seed:?}"
        )))
    }
}

/// The format for an output file: JPEG for `.jpg` and `.jpeg`, PNG for
/// `.png` or no extension. Anything else is a usage error rather than PNG
/// bytes under another type's name.
pub fn format_for(path: &Path) -> anyhow::Result<&'static str> {
    match path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg" | "jpeg") => Ok("jpeg"),
        Some("png") | None => Ok("png"),
        Some(other) => Err(usage(format!(
            "--out {}: render writes PNG or JPEG, so give a .png, .jpg or .jpeg file, not .{other}",
            path.display()
        ))),
    }
}

/// The file extension for an image MIME type.
pub fn extension_for(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        _ => "png",
    }
}

/// The path itself when it is free, else `<stem>-2.<ext>`, `<stem>-3.<ext>`…
pub fn unique_path(path: PathBuf) -> PathBuf {
    if !path.exists() {
        return path;
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (2..)
        .map(|i| path.with_file_name(format!("{stem}-{i}{ext}")))
        .find(|p| !p.exists())
        .unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds() {
        assert!(check_seed("0").is_ok());
        assert!(check_seed("123456789012345678901234567890").is_ok());
        assert!(check_seed("-1").is_err());
        assert!(check_seed("1.5").is_err());
        assert!(check_seed("").is_err());
    }

    #[test]
    fn formats_and_names() {
        assert_eq!(format_for(Path::new("a.JPG")).unwrap(), "jpeg");
        assert_eq!(format_for(Path::new("a.jpeg")).unwrap(), "jpeg");
        assert_eq!(format_for(Path::new("a.png")).unwrap(), "png");
        assert_eq!(format_for(Path::new("a.PNG")).unwrap(), "png");
        assert_eq!(format_for(Path::new("a")).unwrap(), "png");
        // Not PNG bytes under a WebP, BMP or GIF name.
        for name in ["a.webp", "a.bmp", "out/a.gif", "a.jai"] {
            let e = format_for(Path::new(name)).unwrap_err();
            assert_eq!(super::super::exit_code(&e), 2, "{name}");
            assert!(e.to_string().contains(".png, .jpg or .jpeg"), "{e}");
        }
        assert_eq!(extension_for("image/jpeg"), "jpg");
        assert_eq!(extension_for("image/png"), "png");
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("render-7.png");
        assert_eq!(unique_path(p.clone()), p);
        std::fs::write(&p, b"x").unwrap();
        assert_eq!(unique_path(p.clone()), dir.path().join("render-7-2.png"));
        std::fs::write(dir.path().join("render-7-2.png"), b"x").unwrap();
        assert_eq!(unique_path(p), dir.path().join("render-7-3.png"));
    }
}
