//! imageops: thumbnails, EXIF orientation, the encode target, previews, the
//! input limits and MIME sniffing.

use std::io::Cursor;

use base64::Engine as _;
use image::{DynamicImage, ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};
use jai_js::imageops::{self, Thumb};

fn encode(img: &DynamicImage, format: ImageFormat) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, format).unwrap();
    out.into_inner()
}

/// Red, green and blue vertical bands, each `band` px wide and `band` px tall.
fn bands(band: u32) -> DynamicImage {
    DynamicImage::ImageRgb8(RgbImage::from_fn(band * 3, band, |x, _| match x / band {
        0 => Rgb([255, 0, 0]),
        1 => Rgb([0, 255, 0]),
        _ => Rgb([0, 0, 255]),
    }))
}

fn pixel(t: &Thumb, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * t.width + x) * 4) as usize;
    [t.data[i], t.data[i + 1], t.data[i + 2], t.data[i + 3]]
}

#[test]
fn thumbnails_are_centre_square_crops_in_every_format() {
    for format in [
        ImageFormat::Png,
        ImageFormat::Jpeg,
        ImageFormat::WebP,
        ImageFormat::Gif,
    ] {
        let bytes = encode(&bands(100), format);
        let t = imageops::thumbnail(&bytes).unwrap();
        assert_eq!(
            (t.width, t.height, t.data.len()),
            (16, 16, 16 * 16 * 4),
            "{format:?}"
        );
        for px in t.data.chunks(4) {
            assert!(
                px[1] > 180 && px[0] < 90 && px[2] < 90 && px[3] == 255,
                "{format:?}: {px:?} is not the green centre band"
            );
        }
    }
}

#[test]
fn thumbnails_keep_alpha() {
    let img = DynamicImage::ImageRgba8(RgbaImage::from_pixel(32, 32, Rgba([255, 0, 0, 0])));
    let t = imageops::thumbnail(&encode(&img, ImageFormat::Png)).unwrap();
    assert!(t.data.chunks(4).all(|px| px[3] == 0));
}

/// The JPEG with an APP1 Exif segment holding `orientation`, right after SOI.
fn with_orientation(jpeg: &[u8], orientation: u16) -> Vec<u8> {
    let mut tiff = b"MM\0\x2a\0\0\0\x08".to_vec();
    tiff.extend_from_slice(&1u16.to_be_bytes());
    tiff.extend_from_slice(&0x0112u16.to_be_bytes());
    tiff.extend_from_slice(&3u16.to_be_bytes());
    tiff.extend_from_slice(&1u32.to_be_bytes());
    tiff.extend_from_slice(&orientation.to_be_bytes());
    tiff.extend_from_slice(&[0, 0]);
    tiff.extend_from_slice(&0u32.to_be_bytes());
    let mut app1 = b"Exif\0\0".to_vec();
    app1.extend(tiff);
    let mut out = jpeg[..2].to_vec();
    out.extend_from_slice(&[0xff, 0xe1]);
    out.extend_from_slice(&u16::try_from(app1.len() + 2).unwrap().to_be_bytes());
    out.extend(app1);
    out.extend_from_slice(&jpeg[2..]);
    out
}

#[test]
fn exif_orientation_is_applied_and_stripped() {
    // Stored 40x20, red above blue; orientation 6 shows it turned 90° clockwise:
    // 20x40, blue on the left, red on the right.
    let stored = DynamicImage::ImageRgb8(RgbImage::from_fn(40, 20, |_, y| {
        if y < 10 {
            Rgb([255, 0, 0])
        } else {
            Rgb([0, 0, 255])
        }
    }));
    let jpeg = with_orientation(&encode(&stored, ImageFormat::Jpeg), 6);

    let target = imageops::prepare_target(&jpeg).unwrap();
    assert_eq!((target.width, target.height), (20, 40));
    let decoded = image::load_from_memory(&target.jpeg).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (20, 40));
    assert!(
        !target.jpeg.windows(6).any(|w| w == b"Exif\0\0"),
        "the target copy keeps no metadata"
    );

    let t = imageops::thumbnail(&jpeg).unwrap();
    let (left, right) = (pixel(&t, 2, 8), pixel(&t, 13, 8));
    assert!(left[2] > 150 && left[0] < 100, "left {left:?}");
    assert!(right[0] > 150 && right[2] < 100, "right {right:?}");

    let preview = image::load_from_memory(&imageops::preview_jpeg(&jpeg, 384).unwrap()).unwrap();
    assert_eq!((preview.width(), preview.height()), (20, 40));
}

#[test]
fn the_target_copy_is_a_jpeg_at_most_1024_px() {
    let big = DynamicImage::ImageRgb8(RgbImage::from_fn(1600, 800, |x, y| {
        Rgb([(x % 256) as u8, (y % 256) as u8, 128])
    }));
    let target = imageops::prepare_target(&encode(&big, ImageFormat::Png)).unwrap();
    assert_eq!(
        (target.width, target.height),
        (1600, 800),
        "the original size sets the aspect"
    );
    assert_eq!(imageops::sniff_mime(&target.jpeg), Some("image/jpeg"));
    let jpeg = image::load_from_memory_with_format(&target.jpeg, ImageFormat::Jpeg).unwrap();
    assert_eq!((jpeg.width(), jpeg.height()), (1024, 512));
    let b64 = target
        .data_url
        .strip_prefix("data:image/jpeg;base64,")
        .expect("a JPEG data URL");
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap(),
        target.jpeg
    );
    assert_eq!(target.thumb, imageops::thumbnail(&target.jpeg).unwrap());

    let small = imageops::prepare_target(&encode(&bands(10), ImageFormat::Png)).unwrap();
    let jpeg = image::load_from_memory(&small.jpeg).unwrap();
    assert_eq!(
        (small.width, small.height, jpeg.width(), jpeg.height()),
        (30, 10, 30, 10),
        "never enlarged"
    );
}

#[test]
fn transparency_is_composited_onto_black() {
    let img = DynamicImage::ImageRgba8(RgbaImage::from_pixel(32, 32, Rgba([255, 255, 255, 0])));
    let target = imageops::prepare_target(&encode(&img, ImageFormat::Png)).unwrap();
    let decoded = image::load_from_memory(&target.jpeg).unwrap().to_rgb8();
    assert!(decoded.pixels().all(|p| p.0.iter().all(|&c| c < 10)));
}

/// A PNG that claims to be `w`x`h` but holds four bytes of image data.
fn png_header(w: u32, h: u32) -> Vec<u8> {
    let chunk = |out: &mut Vec<u8>, kind: &[u8], data: &[u8]| {
        out.extend(u32::try_from(data.len()).unwrap().to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        let mut crc = flate2::Crc::new();
        crc.update(&body);
        out.extend(&body);
        out.extend(crc.sum().to_be_bytes());
    };
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend(w.to_be_bytes());
    ihdr.extend(h.to_be_bytes());
    ihdr.extend([8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &[0x78, 0x9c, 0x03, 0x00]);
    chunk(&mut out, b"IEND", &[]);
    out
}

#[test]
fn limits_are_checked_before_decoding() {
    let too_big = vec![0u8; 41 * 1024 * 1024];
    let e = imageops::thumbnail(&too_big).unwrap_err().to_string();
    assert!(e.contains("40 MB"), "{e}");

    let huge = png_header(9000, 9000);
    let errors = [
        imageops::thumbnail(&huge).unwrap_err().to_string(),
        imageops::prepare_target(&huge).unwrap_err().to_string(),
        imageops::preview_jpeg(&huge, 384).unwrap_err().to_string(),
    ];
    for e in errors {
        assert!(e.contains("64 megapixels"), "{e}");
    }
}

#[test]
fn previews_fit_without_enlarging() {
    let wide = encode(
        &DynamicImage::ImageRgb8(RgbImage::from_pixel(1600, 800, Rgb([10, 20, 30]))),
        ImageFormat::Png,
    );
    let p = image::load_from_memory(&imageops::preview_jpeg(&wide, 384).unwrap()).unwrap();
    assert_eq!((p.width(), p.height()), (384, 192));
    let small = encode(&bands(20), ImageFormat::Png);
    let p = image::load_from_memory(&imageops::preview_jpeg(&small, 384).unwrap()).unwrap();
    assert_eq!((p.width(), p.height()), (60, 20));
    assert!(imageops::preview_jpeg(&small, 0).is_err());
}

#[test]
fn not_an_image_is_an_error() {
    let e = imageops::thumbnail(b"hello world").unwrap_err().to_string();
    assert!(e.contains("not an image"), "{e}");
    assert!(imageops::prepare_target(&[]).is_err());
}

/// 8×8 sRGB red written by sharp (libvips) as Display P3, with its P3 ICC
/// profile: the stored pixels are (234, 51, 34) and (234, 51, 35).
const P3_RED_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAgAAAAICAIAAABLbSncAAABP2lDQ1BpY2MAABiVfZC/SwJxGMY/l5V1WBA5Fhyh0FAQOdRSgRcFQRFqoDadp2agctz3wuZqqiVqibamGiJoaGjSsTEIHCv8AxoKaujHxVcHLahneT88PC/vywPKY94siHYNCkXHjsyHtXgiqXlrePDTTS+DhimsxehcDEAYJWE6dp4feq2iyHk3mjOK6a3amb23O/MWnL4+eApUL/hfajojTOADCJiW7YCiAUMlx5I8C/jNnJEGJQ6M2PFEEpQd6a81+EhyqsHnku1YRAelAmhrLZxq4UJ+Q96Vkt/7MsWVKNAFDCBYJoT+R6azntHRGQPZ1+8eRDY03tjyhaHj3nVfguA9hM99130/cd2vJfAcQ8Xf3N9+gKmypKa3UIbLCVCvmt7wJPSpcLNpGbZRtzxAW3Ydnk+hJwH9t6CufgN5X15BavQKkAAAAAlwSFlzAAAD6AAAA+gBtXtSawAAABFJREFUCJljeGWshBUxDC0JAFuZT8GAIzAPAAAAAElFTkSuQmCC";
const P3_RED_JPEG: &str = "/9j/4gHwSUNDX1BST0ZJTEUAAQEAAAHgbGNtcwQgAABtbnRyUkdCIFhZWiAH4gADABQACQAOAB1hY3NwTVNGVAAAAABzYXdzY3RybAAAAAAAAAAAAAAAAAAA9tYAAQAAAADTLWhhbmSC4qhyi4o/9yU+uZLuJNauAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAApkZXNjAAAA/AAAACRjcHJ0AAABIAAAACJ3dHB0AAABRAAAABRjaGFkAAABWAAAACxyWFlaAAABhAAAABRnWFlaAAABmAAAABRiWFlaAAABrAAAABRyVFJDAAABwAAAACBnVFJDAAABwAAAACBiVFJDAAABwAAAACBtbHVjAAAAAAAAAAEAAAAMZW5VUwAAAAgAAAAcAHMAUAAzAENtbHVjAAAAAAAAAAEAAAAMZW5VUwAAAAYAAAAcAEMAQwAwAABYWVogAAAAAAAA9tYAAQAAAADTLXNmMzIAAAAAAAEMQgAABd7///MlAAAHkwAA/ZD///uh///+TgAAA5oAAMAUWFlaIAAAAAAAAIPfAAA9vwAAAABYWVogAAAAAAAASr8AALE3AAAKtVhZWiAAAAAAAAAoOAAAEQoAAMh4cGFyYQAAAAAAAwAAAAJmaQAA8qcAAA1ZAAAT0AAAClv/2wBDAAIBAQEBAQIBAQECAgICAgQDAgICAgUEBAMEBgUGBgYFBgYGBwkIBgcJBwYGCAsICQoKCgoKBggLDAsKDAkKCgr/2wBDAQICAgICAgUDAwUKBwYHCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgr/wAARCAAIAAgDASIAAhEBAxEB/8QAFQABAQAAAAAAAAAAAAAAAAAAAAf/xAAUEAEAAAAAAAAAAAAAAAAAAAAA/8QAFQEBAQAAAAAAAAAAAAAAAAAACAn/xAAUEQEAAAAAAAAAAAAAAAAAAAAA/9oADAMBAAIRAxEAPwCPgDGro//Z";

fn b64(s: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD.decode(s).unwrap()
}

/// sharp's sRGB for both is (255, 0, 0) and (255, 0, 3).
fn assert_srgb_red(px: [u8; 4], what: &str) {
    assert!(
        px[0] >= 250 && px[1] <= 8 && px[2] <= 8 && px[3] == 255,
        "{what}: {px:?} is not sRGB red"
    );
}

/// SERVER-JS-9: wide-gamut images are converted to sRGB with their ICC
/// profile, as the Node CLI (sharp) and the extension (canvas) do.
#[test]
fn embedded_icc_profiles_are_converted_to_srgb() {
    for (fixture, what) in [(P3_RED_PNG, "png"), (P3_RED_JPEG, "jpeg")] {
        let bytes = b64(fixture);
        let raw = image::load_from_memory(&bytes).unwrap().to_rgb8();
        assert!(
            raw.get_pixel(4, 4).0[1] > 40,
            "{what}: the fixture stores P3 values"
        );

        assert_srgb_red(pixel(&imageops::thumbnail(&bytes).unwrap(), 8, 8), what);
        let target = imageops::prepare_target(&bytes).unwrap();
        assert_srgb_red(pixel(&target.thumb, 8, 8), what);
        let analyst = image::load_from_memory(&target.jpeg).unwrap().to_rgba8();
        assert_srgb_red(analyst.get_pixel(4, 4).0, what);
        let preview = image::load_from_memory(&imageops::preview_jpeg(&bytes, 64).unwrap())
            .unwrap()
            .to_rgba8();
        assert_srgb_red(preview.get_pixel(4, 4).0, what);
    }
}

/// A PNG of `pixels` (grey, 8×8) carrying `icc`.
fn grey_png_with_profile(value: u8, icc: Vec<u8>) -> Vec<u8> {
    use image::ImageEncoder;
    let mut out = Vec::new();
    let mut encoder = image::codecs::png::PngEncoder::new(&mut out);
    encoder.set_icc_profile(icc).unwrap();
    encoder
        .write_image(&[value; 64], 8, 8, image::ExtendedColorType::L8)
        .unwrap();
    out
}

#[test]
fn grey_profiles_and_broken_profiles() {
    // Linear grey 128 (half the light) is sRGB ~188.
    let linear = moxcms::ColorProfile::new_gray_with_gamma(1.0)
        .encode()
        .unwrap();
    let t = imageops::thumbnail(&grey_png_with_profile(128, linear)).unwrap();
    let px = pixel(&t, 8, 8);
    assert!(
        (183..=193).contains(&px[0]) && px[0] == px[1] && px[1] == px[2],
        "{px:?}"
    );

    // A profile that cannot be read leaves the pixels as they are.
    let t =
        imageops::thumbnail(&grey_png_with_profile(128, b"not an icc profile".to_vec())).unwrap();
    assert_eq!(pixel(&t, 8, 8), [128, 128, 128, 255]);
}

/// imageops decodes what DESIGN.md lists, even when the image crate also has
/// BMP for jai-server.
#[test]
fn bmp_is_not_an_input_format() {
    let mut bmp = b"BM".to_vec();
    bmp.extend_from_slice(&58u32.to_le_bytes());
    bmp.extend_from_slice(&[0, 0, 0, 0]);
    bmp.extend_from_slice(&54u32.to_le_bytes());
    bmp.extend_from_slice(&40u32.to_le_bytes());
    bmp.extend_from_slice(&1i32.to_le_bytes());
    bmp.extend_from_slice(&1i32.to_le_bytes());
    bmp.extend_from_slice(&1u16.to_le_bytes());
    bmp.extend_from_slice(&24u16.to_le_bytes());
    bmp.extend_from_slice(&[0; 24]);
    bmp.extend_from_slice(&[0, 0, 255, 0]);
    let e = imageops::thumbnail(&bmp).unwrap_err().to_string();
    assert!(e.contains("not an image in a supported format"), "{e}");
}

#[test]
fn mime_types_by_magic_bytes() {
    let img = bands(4);
    assert_eq!(
        imageops::sniff_mime(&encode(&img, ImageFormat::Png)),
        Some("image/png")
    );
    assert_eq!(
        imageops::sniff_mime(&encode(&img, ImageFormat::Jpeg)),
        Some("image/jpeg")
    );
    assert_eq!(
        imageops::sniff_mime(&encode(&img, ImageFormat::Gif)),
        Some("image/gif")
    );
    assert_eq!(
        imageops::sniff_mime(&encode(&img, ImageFormat::WebP)),
        Some("image/webp")
    );
    let mut bmp = b"BM".to_vec();
    bmp.extend([0u8; 12]);
    bmp.extend(40u32.to_le_bytes());
    assert_eq!(imageops::sniff_mime(&bmp), Some("image/bmp"));
    assert_eq!(imageops::sniff_mime(b"BMx"), None);
    assert_eq!(imageops::sniff_mime(b"hello world!"), None);
    assert_eq!(imageops::sniff_mime(&[]), None);
}
