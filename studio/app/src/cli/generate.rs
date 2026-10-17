//! `jai generate`: images from a prompt through `POST /v1/images/generations`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{anyhow, Context as _};
use base64::Engine as _;
use serde_json::{json, Value};

use super::{check, engine_progress};
use crate::cli::render::unique_path;
use crate::term::Live;
use crate::{GenerateArgs, ImageFormat};

/// Longest wait for a generation, engine start included.
const TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Largest image area the server accepts (DESIGN.md section 7).
const MAX_PIXELS: u32 = 2_359_296;

/// A `--size` value: "WxH", each side a multiple of 64 from 256 to 2048, at
/// most 2,359,296 pixels (DESIGN.md section 7).
pub fn parse_size(s: &str) -> Result<String, String> {
    let bad = || format!("{s:?} is not WxH, such as 1344x768");
    let (w, h) = s.trim().split_once(['x', 'X', '×']).ok_or_else(bad)?;
    let w: u32 = w.trim().parse().map_err(|_| bad())?;
    let h: u32 = h.trim().parse().map_err(|_| bad())?;
    if [w, h]
        .iter()
        .any(|side| side % 64 != 0 || !(256..=2048).contains(side))
    {
        return Err("each side must be a multiple of 64 from 256 to 2048".into());
    }
    if w * h > MAX_PIXELS {
        return Err(format!(
            "{w}x{h} is too large: at most 2,359,296 pixels, such as 1536x1536"
        ));
    }
    Ok(format!("{w}x{h}"))
}

/// The API name of a format.
pub fn format_name(f: ImageFormat) -> &'static str {
    match f {
        ImageFormat::Png => "png",
        ImageFormat::Jpeg => "jpeg",
        ImageFormat::Webp => "webp",
    }
}

fn format_ext(f: ImageFormat) -> &'static str {
    match f {
        ImageFormat::Png => "png",
        ImageFormat::Jpeg => "jpg",
        ImageFormat::Webp => "webp",
    }
}

/// `--format`, else the `--out` extension, else PNG.
pub fn choose_format(format: Option<ImageFormat>, out: Option<&Path>) -> ImageFormat {
    if let Some(f) = format {
        return f;
    }
    match out
        .and_then(|p| p.extension())
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("jpg" | "jpeg") => ImageFormat::Jpeg,
        Some("webp") => ImageFormat::Webp,
        _ => ImageFormat::Png,
    }
}

/// The request body.
pub fn request_body(
    prompt: &str,
    size: Option<&str>,
    seed: Option<u64>,
    steps: Option<u32>,
    n: u32,
    format: ImageFormat,
) -> Value {
    let mut body = json!({
        "prompt": prompt,
        "model": "flux-schnell",
        "n": n,
        "output_format": format_name(format),
        "response_format": "b64_json",
    });
    if let Some(size) = size {
        body["size"] = json!(size);
    }
    if let Some(seed) = seed {
        body["seed"] = json!(seed);
    }
    if let Some(steps) = steps {
        body["steps"] = json!(steps);
    }
    body
}

/// One generated image.
pub struct Generated {
    pub bytes: Vec<u8>,
    pub seed: u64,
}

/// The images of a response, with its width and height.
pub fn parse_response(v: &Value) -> anyhow::Result<(Vec<Generated>, u64, u64)> {
    let data = v
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("the server's answer has no images"))?;
    let mut images = Vec::new();
    for item in data {
        let b64 = item
            .get("b64_json")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("an image in the server's answer has no data"))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .context("an image in the server's answer is not valid base64")?;
        let seed = item.get("seed").and_then(Value::as_u64).unwrap_or(0);
        images.push(Generated { bytes, seed });
    }
    let width = v.get("width").and_then(Value::as_u64).unwrap_or(0);
    let height = v.get("height").and_then(Value::as_u64).unwrap_or(0);
    Ok((images, width, height))
}

/// Where the images go: `--out` for one image, `<stem>-1.<ext>`… for
/// several, else `jai-<seed>.<ext>` in the current folder.
pub fn output_paths(out: Option<&Path>, seeds: &[u64], ext: &str) -> Vec<PathBuf> {
    match out {
        Some(out) if seeds.len() == 1 => vec![out.to_path_buf()],
        Some(out) => {
            let stem = out
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "jai".into());
            let ext = out
                .extension()
                .map(|e| format!(".{}", e.to_string_lossy()))
                .unwrap_or_default();
            (1..=seeds.len())
                .map(|i| out.with_file_name(format!("{stem}-{i}{ext}")))
                .collect()
        }
        None => seeds
            .iter()
            .map(|s| PathBuf::from(format!("jai-{s}.{ext}")))
            .collect(),
    }
}

pub async fn run(args: GenerateArgs) -> anyhow::Result<ExitCode> {
    let format = choose_format(args.format, args.out.as_deref());
    let n = args.n.unwrap_or(1);
    let body = request_body(
        &args.prompt,
        args.size.as_deref(),
        args.seed,
        args.steps,
        n,
        format,
    );
    let paths = super::paths()?;
    let live = Live::new();
    let progress_live = live.clone();
    let what = if n == 1 {
        "Generating 1 image".to_string()
    } else {
        format!("Generating {n} images")
    };
    let v: Value = super::with_server(&paths, &live, false, move |client, _| async move {
        let _progress = engine_progress(&progress_live, &client, &what);
        let resp = client
            .post("/v1/images/generations")
            .json(&body)
            .timeout(TIMEOUT)
            .send()
            .await
            .context("cannot reach the JAI server")?;
        Ok(check(resp).await?.json::<Value>().await?)
    })
    .await?;
    let (images, width, height) = parse_response(&v)?;
    let seeds: Vec<u64> = images.iter().map(|i| i.seed).collect();
    let names = output_paths(args.out.as_deref(), &seeds, format_ext(format));
    for (image, name) in images.iter().zip(names) {
        let name = if args.out.is_some() {
            name
        } else {
            unique_path(name)
        };
        std::fs::write(&name, &image.bytes)
            .with_context(|| format!("cannot write {}", name.display()))?;
        outln!(
            "Wrote {} ({width}x{height}, seed {})",
            name.display(),
            image.seed
        );
    }
    if let Some(ms) = v
        .get("timings")
        .and_then(|t| t.get("generateMs"))
        .and_then(Value::as_f64)
    {
        errln!("Generated in {:.1} s.", ms / 1000.0);
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("1344x768").unwrap(), "1344x768");
        assert_eq!(parse_size(" 1024X1024 ").unwrap(), "1024x1024");
        assert_eq!(parse_size("1536x1536").unwrap(), "1536x1536");
        assert!(parse_size("1000x768")
            .unwrap_err()
            .contains("multiple of 64"));
        assert!(parse_size("2048x2048").unwrap_err().contains("too large"));
        assert!(parse_size("192x256").is_err());
        assert!(parse_size("big").unwrap_err().contains("WxH"));
    }

    #[test]
    fn bodies() {
        let b = request_body(
            "a castle",
            Some("1344x768"),
            Some(7),
            None,
            2,
            ImageFormat::Webp,
        );
        assert_eq!(
            b,
            json!({"prompt": "a castle", "model": "flux-schnell", "n": 2, "output_format": "webp",
                   "response_format": "b64_json", "size": "1344x768", "seed": 7})
        );
        let b = request_body("x", None, None, Some(8), 1, ImageFormat::Png);
        assert_eq!(b["steps"], 8);
        assert!(b.get("seed").is_none() && b.get("size").is_none());
    }

    #[test]
    fn formats() {
        assert_eq!(choose_format(None, None), ImageFormat::Png);
        assert_eq!(
            choose_format(None, Some(Path::new("a.JPG"))),
            ImageFormat::Jpeg
        );
        assert_eq!(
            choose_format(None, Some(Path::new("a.webp"))),
            ImageFormat::Webp
        );
        assert_eq!(
            choose_format(Some(ImageFormat::Png), Some(Path::new("a.jpg"))),
            ImageFormat::Png
        );
    }

    #[test]
    fn responses_and_names() {
        let v = json!({"created": 1, "model": "flux-schnell", "width": 1344, "height": 768,
                       "data": [{"b64_json": "iVBORw==", "seed": 7}, {"b64_json": "iVBORw==", "seed": 8}]});
        let (images, w, h) = parse_response(&v).unwrap();
        assert_eq!((images.len(), w, h), (2, 1344, 768));
        assert_eq!(images[0].bytes, [0x89, 0x50, 0x4e, 0x47]);
        assert_eq!(images[1].seed, 8);
        assert!(parse_response(&json!({"data": [{"seed": 1}]})).is_err());
        assert!(parse_response(&json!({})).is_err());

        assert_eq!(
            output_paths(None, &[7, 8], "png"),
            [PathBuf::from("jai-7.png"), PathBuf::from("jai-8.png")]
        );
        assert_eq!(
            output_paths(Some(Path::new("out/x.jpg")), &[7], "jpg"),
            [PathBuf::from("out/x.jpg")]
        );
        assert_eq!(
            output_paths(Some(Path::new("out/x.jpg")), &[7, 8], "jpg"),
            [PathBuf::from("out/x-1.jpg"), PathBuf::from("out/x-2.jpg")]
        );
    }
}
