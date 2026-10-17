//! Chat images (DESIGN.md sections 7 and 12). Each `data:` URL is decoded in
//! full and encoded again before llama-server sees it. llama-server decodes
//! images with stb_image, hands whatever that cannot read to ffmpeg (when one
//! is on PATH), and keeps a full-size copy of every image of a request in
//! memory. So an image goes on only if it decodes here as the type it claims,
//! within 40 MB and 64 megapixels, and then as a PNG or JPEG this crate
//! wrote: a JPEG stays a JPEG (quality 95); PNG, GIF (its first frame) and
//! BMP become PNG. EXIF orientation is applied, since the engine ignores it.

use std::borrow::Cow;
use std::io::Cursor;

use base64::alphabet;
use base64::engine::general_purpose::STANDARD;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::Engine as _;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};

/// Decoded size of one image.
pub(crate) const MAX_IMAGE_BYTES: usize = 40_000_000;
/// Pixels in one image.
pub(crate) const MAX_IMAGE_PIXELS: u64 = 64_000_000;
/// Quality of a JPEG encoded again.
const JPEG_QUALITY: u8 = 95;
/// Standard base64, with or without padding.
const BASE64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

/// The image type of a data URL's subtype.
pub(crate) fn format_of(subtype: &str) -> Option<ImageFormat> {
    match subtype {
        "png" => Some(ImageFormat::Png),
        "jpeg" => Some(ImageFormat::Jpeg),
        "gif" => Some(ImageFormat::Gif),
        "bmp" => Some(ImageFormat::Bmp),
        _ => None,
    }
}

/// The image of `data:image/<subtype>;base64,<data>`, checked and encoded
/// again, as a new data URL. The error says what is wrong with it.
pub(crate) fn reencode(subtype: &str, data: &str) -> Result<String, String> {
    let format = format_of(subtype)
        .ok_or_else(|| format!("image/{subtype} is not accepted; send PNG, JPEG, GIF or BMP"))?;
    if data.len() / 4 * 3 > MAX_IMAGE_BYTES {
        return Err("an image may be at most 40 MB".into());
    }
    let bytes = BASE64
        .decode(data)
        .map_err(|_| "the image data is not valid base64".to_owned())?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err("an image may be at most 40 MB".into());
    }
    if image::guess_format(&bytes).ok() != Some(format) {
        return Err(format!("the image data is not a {subtype} image"));
    }
    let image = decode(&bytes, format)?;
    let (mime, encoded) = encode(&image, format == ImageFormat::Jpeg)
        .map_err(|e| format!("the image could not be encoded again: {e}"))?;
    Ok(format!("data:{mime};base64,{}", STANDARD.encode(encoded)))
}

/// Decodes after checking the size in the header, then applies the EXIF
/// orientation.
fn decode(bytes: &[u8], format: ImageFormat) -> Result<DynamicImage, String> {
    let reader = || {
        let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
        let mut limits = Limits::default();
        // 64 MP of 16-bit RGBA plus working memory.
        limits.max_alloc = Some(1 << 30);
        reader.limits(limits);
        reader
    };
    let (w, h) = reader()
        .into_dimensions()
        .map_err(|e| format!("the image cannot be read: {e}"))?;
    if w == 0 || h == 0 {
        return Err("the image has no pixels".into());
    }
    if u64::from(w) * u64::from(h) > MAX_IMAGE_PIXELS {
        return Err(format!(
            "the image is {w}x{h}; images may have at most {} megapixels",
            MAX_IMAGE_PIXELS / 1_000_000
        ));
    }
    let mut decoder = reader()
        .into_decoder()
        .map_err(|e| format!("the image cannot be read: {e}"))?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder)
        .map_err(|e| format!("the image cannot be decoded: {e}"))?;
    image.apply_orientation(orientation);
    Ok(image)
}

/// A JPEG of a JPEG, otherwise a PNG, with 8 bits per channel.
fn encode(image: &DynamicImage, jpeg: bool) -> image::ImageResult<(&'static str, Vec<u8>)> {
    let mut out = Vec::new();
    let grey = !image.color().has_color();
    if jpeg {
        let mut encoder = JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY);
        if grey {
            encoder.encode_image(&image.to_luma8())?;
        } else {
            encoder.encode_image(&image.to_rgb8())?;
        }
        return Ok(("image/jpeg", out));
    }
    let eight: Cow<'_, DynamicImage> = match image {
        DynamicImage::ImageLuma8(_)
        | DynamicImage::ImageLumaA8(_)
        | DynamicImage::ImageRgb8(_)
        | DynamicImage::ImageRgba8(_) => Cow::Borrowed(image),
        _ => Cow::Owned(match (grey, image.color().has_alpha()) {
            (true, false) => DynamicImage::ImageLuma8(image.to_luma8()),
            (true, true) => DynamicImage::ImageLumaA8(image.to_luma_alpha8()),
            (false, false) => DynamicImage::ImageRgb8(image.to_rgb8()),
            (false, true) => DynamicImage::ImageRgba8(image.to_rgba8()),
        }),
    };
    eight.write_with_encoder(PngEncoder::new(&mut out))?;
    Ok(("image/png", out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GenericImageView, ImageEncoder, Rgb, RgbImage, Rgba, RgbaImage};

    fn b64(bytes: &[u8]) -> String {
        STANDARD.encode(bytes)
    }

    fn encoded(image: &DynamicImage, format: ImageFormat) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        image.write_to(&mut out, format).unwrap();
        out.into_inner()
    }

    fn sample() -> DynamicImage {
        DynamicImage::ImageRgb8(RgbImage::from_fn(6, 4, |x, _| {
            if x < 3 {
                Rgb([250, 10, 10])
            } else {
                Rgb([10, 10, 250])
            }
        }))
    }

    /// The type and the image of a data URL `reencode` made.
    fn open(url: &str) -> (String, DynamicImage) {
        let rest = url.strip_prefix("data:").unwrap();
        let (mime, data) = rest.split_once(";base64,").unwrap();
        let bytes = STANDARD.decode(data).unwrap();
        (mime.to_owned(), image::load_from_memory(&bytes).unwrap())
    }

    #[test]
    fn every_type_comes_out_as_png_or_jpeg() {
        let image = sample();
        for (subtype, format, mime) in [
            ("png", ImageFormat::Png, "image/png"),
            ("jpeg", ImageFormat::Jpeg, "image/jpeg"),
            ("gif", ImageFormat::Gif, "image/png"),
            ("bmp", ImageFormat::Bmp, "image/png"),
        ] {
            let url = reencode(subtype, &b64(&encoded(&image, format))).unwrap();
            let (got, decoded) = open(&url);
            assert_eq!(got, mime, "{subtype}");
            assert_eq!(decoded.dimensions(), (6, 4), "{subtype}");
            let left = decoded.to_rgb8().get_pixel(0, 0).0;
            assert!(left[0] > 200 && left[2] < 60, "{subtype}: {left:?}");
        }
    }

    #[test]
    fn transparency_survives() {
        let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(3, 3, Rgba([1, 2, 3, 0])));
        let url = reencode("png", &b64(&encoded(&image, ImageFormat::Png))).unwrap();
        assert_eq!(open(&url).1.to_rgba8().get_pixel(1, 1).0[3], 0);
    }

    /// SERVER-JS-2 (a): the right leading bytes followed by something else
    /// (here an MP4 box) no longer pass.
    #[test]
    fn a_signature_alone_is_not_an_image() {
        let mut fake_png = b"\x89PNG\r\n\x1a\n".to_vec();
        fake_png.extend_from_slice(b"\0\0\0\x18ftypmp42\0\0\0\0mp42isom");
        assert!(reencode("png", &b64(&fake_png)).is_err());
        let mut fake_bmp = b"BM".to_vec();
        fake_bmp.extend_from_slice(&[0x41; 64]);
        assert!(reencode("bmp", &b64(&fake_bmp)).is_err());
        let fake_jpeg = b"\xff\xd8\xff\xe0 not really a jpeg at all";
        assert!(reencode("jpeg", &b64(fake_jpeg)).is_err());
        // A real image cut short.
        let png = encoded(&sample(), ImageFormat::Png);
        assert!(reencode("png", &b64(&png[..png.len() - 20])).is_err());
    }

    #[test]
    fn the_type_must_match_the_bytes() {
        let jpeg = encoded(&sample(), ImageFormat::Jpeg);
        let e = reencode("png", &b64(&jpeg)).unwrap_err();
        assert!(e.contains("not a png image"), "{e}");
        assert!(reencode("webp", &b64(&jpeg)).is_err());
        assert!(reencode("png", "!!!!").unwrap_err().contains("base64"));
    }

    /// A PNG header that claims `w`x`h`.
    fn png_claiming(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let encoder = PngEncoder::new(&mut out);
        encoder
            .write_image(&[0, 0, 0], 1, 1, image::ExtendedColorType::Rgb8)
            .unwrap();
        // IHDR's width and height sit at bytes 16..24; its CRC at 29..33.
        out[16..20].copy_from_slice(&w.to_be_bytes());
        out[20..24].copy_from_slice(&h.to_be_bytes());
        let crc = png_crc(&out[12..29]);
        out[29..33].copy_from_slice(&crc.to_be_bytes());
        out
    }

    /// CRC-32 as PNG uses it.
    fn png_crc(bytes: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for &b in bytes {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    /// SERVER-JS-2 (b): the 64 megapixel limit is enforced from the header,
    /// before any pixel is decoded.
    #[test]
    fn huge_images_are_refused_from_the_header() {
        let e = reencode("png", &b64(&png_claiming(30_000, 30_000))).unwrap_err();
        assert!(
            e.contains("30000x30000") && e.contains("64 megapixels"),
            "{e}"
        );
        let e = reencode("png", &b64(&png_claiming(8001, 8000))).unwrap_err();
        assert!(e.contains("64 megapixels"), "{e}");
    }

    #[test]
    fn exif_orientation_is_applied() {
        // Orientation 6 (turn 90° clockwise to show): stored 6x4, shown 4x6.
        let jpeg = encoded(&sample(), ImageFormat::Jpeg);
        let mut tiff = b"MM\0\x2a\0\0\0\x08".to_vec();
        tiff.extend_from_slice(&1u16.to_be_bytes());
        tiff.extend_from_slice(&0x0112u16.to_be_bytes());
        tiff.extend_from_slice(&3u16.to_be_bytes());
        tiff.extend_from_slice(&1u32.to_be_bytes());
        tiff.extend_from_slice(&6u16.to_be_bytes());
        tiff.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend(tiff);
        let mut with_exif = jpeg[..2].to_vec();
        with_exif.extend_from_slice(&[0xff, 0xe1]);
        with_exif.extend_from_slice(&u16::try_from(app1.len() + 2).unwrap().to_be_bytes());
        with_exif.extend(app1);
        with_exif.extend_from_slice(&jpeg[2..]);
        let url = reencode("jpeg", &b64(&with_exif)).unwrap();
        assert_eq!(open(&url).1.dimensions(), (4, 6));
    }

    #[test]
    fn padding_is_optional() {
        let png = encoded(&sample(), ImageFormat::Png);
        let unpadded = b64(&png).trim_end_matches('=').to_owned();
        assert!(reencode("png", &unpadded).is_ok());
    }
}
