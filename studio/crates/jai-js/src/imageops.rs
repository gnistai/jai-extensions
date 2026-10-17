//! Image operations for the host and the app (DESIGN.md section 8): decoding
//! with EXIF orientation and colour management, the 16×16 thumbnails the
//! encoder compares, the analyst's copy of an encode target, and previews.
//! Every decode refuses inputs over 40 MB or 64 megapixels before decoding
//! any pixels. Pixels described by an embedded ICC profile are converted to
//! sRGB, as browsers, the extension and sharp (the Node CLI) do, so a
//! wide-gamut photo gets the same thumbnail and analyst copy everywhere.

use std::io::Cursor;

use anyhow::{bail, Context as _};
use base64::Engine as _;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::metadata::Orientation;
use image::{
    DynamicImage, GrayAlphaImage, GrayImage, ImageDecoder, ImageFormat, ImageReader, Limits, Rgb,
    RgbImage, RgbaImage,
};
use moxcms::{ColorProfile, DataColorSpace, Layout, TransformOptions};
use serde::{Deserialize, Serialize};

/// Side of a thumbnail.
pub const THUMB_SIDE: u32 = 16;
/// Largest encoded input accepted.
pub const MAX_INPUT_BYTES: usize = 40 * 1024 * 1024;
/// Largest input accepted, in pixels.
pub const MAX_INPUT_PIXELS: u64 = 64_000_000;
/// Long side of the analyst's copy of an encode target.
pub const TARGET_MAX_SIDE: u32 = 1024;
/// JPEG quality for target copies and previews.
const JPEG_QUALITY: u8 = 85;
/// The formats every decode accepts, whatever else the image crate was built
/// with for other crates (jai-server adds BMP).
const FORMATS: [ImageFormat; 4] = [
    ImageFormat::Png,
    ImageFormat::Jpeg,
    ImageFormat::WebP,
    ImageFormat::Gif,
];

/// A small RGBA image.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thumb {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// Decode (PNG, JPEG, WebP, GIF), orient, centre-square crop, 16×16
/// (CatmullRom). Alpha is kept.
pub fn thumbnail(bytes: &[u8]) -> anyhow::Result<Thumb> {
    let image = decode(bytes)?;
    let (w, h) = (image.width(), image.height());
    let side = w.min(h);
    let square = image.crop_imm((w - side) / 2, (h - side) / 2, side, side);
    let small = square
        .resize_exact(THUMB_SIDE, THUMB_SIDE, FilterType::CatmullRom)
        .to_rgba8();
    Ok(Thumb {
        width: small.width(),
        height: small.height(),
        data: small.into_raw(),
    })
}

/// The encode target: the analyst's JPEG, its data URL, its size and its
/// thumbnail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedTarget {
    pub jpeg: Vec<u8>,
    pub data_url: String,
    /// The oriented size of the original image, which sets the document's
    /// aspect (the Node CLI and the extension pass the same).
    pub width: u32,
    pub height: u32,
    /// The thumbnail of `jpeg`, made the way render thumbnails are.
    pub thumb: Thumb,
}

/// At most 64 MP and 40 MB in; EXIF orientation; at most 1024 px on the long
/// side; JPEG quality 85; no metadata. Transparency is composited onto black,
/// as a canvas JPEG export does.
pub fn prepare_target(bytes: &[u8]) -> anyhow::Result<PreparedTarget> {
    let image = decode(bytes)?;
    let (width, height) = (image.width(), image.height());
    let jpeg = encode_jpeg(&fit(image, TARGET_MAX_SIDE))?;
    let data_url = format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&jpeg)
    );
    let thumb = thumbnail(&jpeg)?;
    Ok(PreparedTarget {
        jpeg,
        data_url,
        width,
        height,
        thumb,
    })
}

/// A JPEG at most `max_side` px on the long side (never enlarged), oriented,
/// without metadata.
pub fn preview_jpeg(bytes: &[u8], max_side: u32) -> anyhow::Result<Vec<u8>> {
    if max_side == 0 {
        bail!("the preview size must be at least 1 px");
    }
    encode_jpeg(&fit(decode(bytes)?, max_side))
}

/// The image type from magic bytes.
pub fn sniff_mime(bytes: &[u8]) -> Option<&'static str> {
    const BMP_HEADER_SIZES: [u32; 6] = [12, 40, 52, 56, 108, 124];
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.len() >= 18
        && bytes.starts_with(b"BM")
        && BMP_HEADER_SIZES.contains(&u32::from_le_bytes([
            bytes[14], bytes[15], bytes[16], bytes[17],
        ]))
    {
        Some("image/bmp")
    } else {
        None
    }
}

/// A reader with the format guessed from the bytes and decoder limits set.
fn reader(bytes: &[u8]) -> anyhow::Result<ImageReader<Cursor<&[u8]>>> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .context("cannot read the image")?;
    if !reader.format().is_some_and(|f| FORMATS.contains(&f)) {
        bail!("not an image in a supported format (PNG, JPEG, WebP or GIF)");
    }
    let mut limits = Limits::default();
    // 64 MP of 16-bit RGBA plus working memory.
    limits.max_alloc = Some(1 << 30);
    reader.limits(limits);
    Ok(reader)
}

/// Decodes after checking the size limits from the header, converts to sRGB
/// with an embedded ICC profile, then applies the EXIF orientation.
fn decode(bytes: &[u8]) -> anyhow::Result<DynamicImage> {
    if bytes.len() > MAX_INPUT_BYTES {
        bail!("the image is larger than {} MB", MAX_INPUT_BYTES >> 20);
    }
    let (w, h) = reader(bytes)?
        .into_dimensions()
        .context("cannot read the image size")?;
    if w == 0 || h == 0 {
        bail!("the image has no pixels");
    }
    if u64::from(w) * u64::from(h) > MAX_INPUT_PIXELS {
        bail!(
            "the image has more than {} megapixels",
            MAX_INPUT_PIXELS / 1_000_000
        );
    }
    let mut decoder = reader(bytes)?
        .into_decoder()
        .context("cannot read the image")?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let icc = decoder.icc_profile().ok().flatten();
    let mut image = DynamicImage::from_decoder(decoder).context("cannot decode the image")?;
    if let Some(icc) = icc {
        image = to_srgb(image, &icc);
    }
    image.apply_orientation(orientation);
    Ok(image)
}

/// The image in sRGB, by its ICC profile, with 8 bits per channel. A profile
/// that cannot be read or does not describe these pixels (a CMYK profile on
/// a JPEG already decoded to RGB, say) is ignored, as browsers ignore it.
fn to_srgb(image: DynamicImage, icc: &[u8]) -> DynamicImage {
    let profile = match ColorProfile::new_from_slice(icc) {
        Ok(profile) => profile,
        Err(err) => {
            tracing::debug!(error = ?err, "ignoring an ICC profile that cannot be read");
            return image;
        }
    };
    let color = image.color();
    let converted = match (profile.color_space, color.has_color(), color.has_alpha()) {
        (DataColorSpace::Rgb, true, false) => {
            let rgb = image.to_rgb8();
            let (w, h) = rgb.dimensions();
            convert(&profile, Layout::Rgb, Layout::Rgb, &rgb, 3)
                .and_then(|px| RgbImage::from_raw(w, h, px))
                .map(DynamicImage::ImageRgb8)
        }
        (DataColorSpace::Rgb, true, true) => {
            let rgba = image.to_rgba8();
            let (w, h) = rgba.dimensions();
            convert(&profile, Layout::Rgba, Layout::Rgba, &rgba, 4)
                .and_then(|px| RgbaImage::from_raw(w, h, px))
                .map(DynamicImage::ImageRgba8)
        }
        (DataColorSpace::Gray, false, false) => {
            let grey: GrayImage = image.to_luma8();
            let (w, h) = grey.dimensions();
            convert(&profile, Layout::Gray, Layout::Rgb, &grey, 3)
                .and_then(|px| RgbImage::from_raw(w, h, px))
                .map(DynamicImage::ImageRgb8)
        }
        (DataColorSpace::Gray, false, true) => {
            let grey: GrayAlphaImage = image.to_luma_alpha8();
            let (w, h) = grey.dimensions();
            convert(&profile, Layout::GrayAlpha, Layout::Rgba, &grey, 4)
                .and_then(|px| RgbaImage::from_raw(w, h, px))
                .map(DynamicImage::ImageRgba8)
        }
        _ => None,
    };
    converted.unwrap_or(image)
}

/// `pixels` in `from` layout under `profile`, as sRGB in `to` layout with
/// `channels` samples per pixel.
fn convert(
    profile: &ColorProfile,
    from: Layout,
    to: Layout,
    pixels: &[u8],
    channels: usize,
) -> Option<Vec<u8>> {
    let samples = match from {
        Layout::Rgb => 3,
        Layout::Rgba => 4,
        Layout::GrayAlpha => 2,
        _ => 1,
    };
    let mut out = vec![0u8; pixels.len() / samples * channels];
    let result = profile
        .create_transform_8bit(
            from,
            &ColorProfile::new_srgb(),
            to,
            TransformOptions::default(),
        )
        .and_then(|transform| transform.transform(pixels, &mut out));
    match result {
        Ok(()) => Some(out),
        Err(err) => {
            tracing::debug!(error = ?err, "ignoring an ICC profile that cannot be applied");
            None
        }
    }
}

/// Scales down so the long side is at most `max_side` (Lanczos3).
fn fit(image: DynamicImage, max_side: u32) -> DynamicImage {
    let (w, h) = (image.width(), image.height());
    let long = w.max(h);
    if long <= max_side {
        return image;
    }
    let scale = f64::from(max_side) / f64::from(long);
    let side = |n: u32| ((f64::from(n) * scale).round() as u32).clamp(1, max_side);
    image.resize_exact(side(w), side(h), FilterType::Lanczos3)
}

/// JPEG at `JPEG_QUALITY`, no metadata.
fn encode_jpeg(image: &DynamicImage) -> anyhow::Result<Vec<u8>> {
    let rgb = flatten(image);
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY)
        .encode_image(&rgb)
        .context("cannot encode the JPEG")?;
    Ok(out)
}

/// RGB, with any transparency composited onto black.
fn flatten(image: &DynamicImage) -> RgbImage {
    if !image.color().has_alpha() {
        return image.to_rgb8();
    }
    let rgba = image.to_rgba8();
    RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [r, g, b, a] = rgba.get_pixel(x, y).0;
        let over_black = |c: u8| ((u16::from(c) * u16::from(a) + 127) / 255) as u8;
        Rgb([over_black(r), over_black(g), over_black(b)])
    })
}
