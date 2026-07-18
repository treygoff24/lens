use std::fs::File;
use std::io::{BufReader, Cursor, Read, Seek, SeekFrom, Write};
use std::process::Command;

use image::codecs::jpeg::JpegEncoder;
use image::metadata::Orientation;
use image::{DynamicImage, GenericImageView, ImageDecoder, ImageError, ImageReader, Limits};
use tempfile::Builder;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Normalized {
    pub jpeg_bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormalizeSkipReason {
    UnsupportedFormat,
    CorruptImage,
    TooLarge,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormalizeOutput {
    Normalized(Normalized),
    Skip {
        reason: NormalizeSkipReason,
        detail: String,
    },
}

const MAX_LONG_SIDE: u32 = 1600;
const JPEG_PASS_THROUGH_MAX_BYTES: u64 = 3_000_000;
const MAX_RAW_JPEG_BYTES: usize = 6_500_000;
const MAX_INPUT_BYTES: u64 = 64 * 1024 * 1024;
const DECODE_PIXEL_LIMIT: u32 = 12_000;
const DECODE_ALLOCATION_LIMIT: u64 = 64 * 1024 * 1024;

pub fn normalize_image(file: &File, extension: &str, force_reencode: bool) -> NormalizeOutput {
    let input_bytes = match file.metadata() {
        Ok(metadata) => metadata.len(),
        Err(err) => {
            return skip(
                NormalizeSkipReason::CorruptImage,
                format!("failed to inspect image: {err}"),
            );
        }
    };
    if input_bytes > MAX_INPUT_BYTES {
        return skip(NormalizeSkipReason::TooLarge, "image exceeds input limit");
    }

    let ext = extension.trim_start_matches('.').to_ascii_lowercase();
    if ext == "heic" {
        return normalize_with_sips(file, &ext, true)
            .unwrap_or_else(|| skip(NormalizeSkipReason::CorruptImage, "sips failed"));
    }
    if !supported_by_image_crate(&ext) {
        return skip(
            NormalizeSkipReason::UnsupportedFormat,
            "unsupported image format",
        );
    }

    let is_jpeg = matches!(ext.as_str(), "jpg" | "jpeg");
    if is_jpeg && !force_reencode && pass_through_ok(file) {
        match orientation_for_file(file) {
            Ok(Orientation::NoTransforms) => {
                return match read_file(file, MAX_RAW_JPEG_BYTES as u64) {
                    Ok(bytes) if bytes.len() <= MAX_RAW_JPEG_BYTES => {
                        match image_dimensions(file) {
                            Ok(dimensions) => normalized_output(bytes, dimensions),
                            Err(err) => skip(
                                NormalizeSkipReason::CorruptImage,
                                format!("decode failed: {err}"),
                            ),
                        }
                    }
                    Ok(_) => skip(NormalizeSkipReason::TooLarge, "jpeg exceeds payload limit"),
                    Err(err) => skip(NormalizeSkipReason::CorruptImage, err.to_string()),
                };
            }
            Ok(_) => {}
            Err(_) => {}
        }
    }

    match decode_with_orientation(file) {
        Ok((image, orientation)) => encode_resized(image, orientation),
        Err(err @ ImageError::Limits(_)) => skip(
            NormalizeSkipReason::TooLarge,
            format!("decode limit exceeded: {err}"),
        ),
        Err(err) => normalize_with_sips(file, &ext, false).unwrap_or_else(|| {
            skip(
                NormalizeSkipReason::CorruptImage,
                format!("decode failed: {err}"),
            )
        }),
    }
}

fn supported_by_image_crate(ext: &str) -> bool {
    matches!(
        ext,
        "jpg" | "jpeg" | "png" | "webp" | "gif" | "bmp" | "tif" | "tiff"
    )
}

fn pass_through_ok(file: &File) -> bool {
    file.metadata()
        .is_ok_and(|meta| meta.len() <= JPEG_PASS_THROUGH_MAX_BYTES)
        && image_dimensions(file).is_ok_and(|(width, height)| width.max(height) <= MAX_LONG_SIDE)
}

fn orientation_for_file(file: &File) -> image::ImageResult<Orientation> {
    let mut decoder = limited_reader(file)?.into_decoder()?;
    decoder.orientation()
}

fn image_dimensions(file: &File) -> image::ImageResult<(u32, u32)> {
    limited_reader(file)?.into_dimensions()
}

fn decode_with_orientation(file: &File) -> image::ImageResult<(DynamicImage, Orientation)> {
    let mut decoder = limited_reader(file)?.into_decoder()?;
    let orientation = decoder.orientation()?;
    let image = limited_reader(file)?.decode()?;
    Ok((image, orientation))
}

fn limited_reader(file: &File) -> image::ImageResult<ImageReader<BufReader<File>>> {
    let mut reader =
        ImageReader::new(BufReader::new(rewound_clone(file)?)).with_guessed_format()?;
    reader.limits(limits());
    Ok(reader)
}

fn rewound_clone(file: &File) -> std::io::Result<File> {
    let mut cloned = file.try_clone()?;
    cloned.seek(SeekFrom::Start(0))?;
    Ok(cloned)
}

fn read_file(file: &File, limit: u64) -> std::io::Result<Vec<u8>> {
    let size = file.metadata()?.len();
    if size > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            "image exceeds input limit",
        ));
    }
    let mut bytes = Vec::with_capacity(size as usize);
    rewound_clone(file)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            "image exceeds input limit",
        ));
    }
    Ok(bytes)
}

fn limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(DECODE_PIXEL_LIMIT);
    limits.max_image_height = Some(DECODE_PIXEL_LIMIT);
    limits.max_alloc = Some(DECODE_ALLOCATION_LIMIT);
    limits
}

fn encode_resized(mut image: DynamicImage, orientation: Orientation) -> NormalizeOutput {
    image.apply_orientation(orientation);
    let resized = resize_if_needed(image);
    match encode_jpeg(&resized, 80) {
        Ok(bytes) if bytes.len() <= MAX_RAW_JPEG_BYTES => {
            normalized_output(bytes, resized.dimensions())
        }
        Ok(_) => match encode_jpeg(&resized, 60) {
            Ok(bytes) if bytes.len() <= MAX_RAW_JPEG_BYTES => {
                normalized_output(bytes, resized.dimensions())
            }
            Ok(_) => skip(NormalizeSkipReason::TooLarge, "jpeg exceeds payload limit"),
            Err(err) => skip(NormalizeSkipReason::CorruptImage, err.to_string()),
        },
        Err(err) => skip(NormalizeSkipReason::CorruptImage, err.to_string()),
    }
}

fn normalized_output(jpeg_bytes: Vec<u8>, (width, height): (u32, u32)) -> NormalizeOutput {
    NormalizeOutput::Normalized(Normalized {
        jpeg_bytes,
        width,
        height,
    })
}

fn resize_if_needed(image: DynamicImage) -> DynamicImage {
    let (width, height) = image.dimensions();
    if width.max(height) <= MAX_LONG_SIDE {
        return image;
    }
    image.resize(
        MAX_LONG_SIDE,
        MAX_LONG_SIDE,
        image::imageops::FilterType::Lanczos3,
    )
}

fn encode_jpeg(image: &DynamicImage, quality: u8) -> image::ImageResult<Vec<u8>> {
    let rgb = image.to_rgb8();
    let (width, height) = rgb.dimensions();
    let mut out = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut out, quality);
    encoder.encode(&rgb, width, height, image::ExtendedColorType::Rgb8)?;
    Ok(out)
}

fn normalize_with_sips(
    file: &File,
    extension: &str,
    missing_as_unsupported: bool,
) -> Option<NormalizeOutput> {
    if !sips_exists() {
        return Some(skip(
            if missing_as_unsupported {
                NormalizeSkipReason::UnsupportedFormat
            } else {
                NormalizeSkipReason::CorruptImage
            },
            "sips is not available",
        ));
    }

    let mut input = Builder::new()
        .prefix("lens-normalize-input-")
        .suffix(&format!(".{extension}"))
        .tempfile()
        .ok()?;
    if file.metadata().ok()?.len() > MAX_INPUT_BYTES {
        return Some(skip(
            NormalizeSkipReason::TooLarge,
            "image exceeds sips input limit",
        ));
    }
    let copied = std::io::copy(
        &mut rewound_clone(file).ok()?.take(MAX_INPUT_BYTES + 1),
        &mut input,
    )
    .ok()?;
    if copied > MAX_INPUT_BYTES {
        return Some(skip(
            NormalizeSkipReason::TooLarge,
            "image exceeds sips input limit",
        ));
    }
    input.flush().ok()?;
    let mut out = Builder::new()
        .prefix("lens-normalize-output-")
        .suffix(".jpg")
        .tempfile()
        .ok()?;
    let status = Command::new("sips")
        .args([
            "-s",
            "format",
            "jpeg",
            "-s",
            "formatOptions",
            "80",
            "-Z",
            "1600",
        ])
        .arg(input.path())
        .arg("--out")
        .arg(out.path())
        .output()
        .ok()?;
    if !status.status.success() {
        return None;
    }

    out.as_file_mut().seek(SeekFrom::Start(0)).ok()?;
    let mut bytes = Vec::new();
    out.as_file_mut().read_to_end(&mut bytes).ok()?;
    if bytes.len() > MAX_RAW_JPEG_BYTES {
        return Some(skip(
            NormalizeSkipReason::TooLarge,
            "sips jpeg exceeds payload limit",
        ));
    }

    match ImageReader::new(Cursor::new(&bytes)).with_guessed_format() {
        Ok(mut reader) => {
            reader.limits(limits());
            match reader.into_dimensions() {
                Ok(dimensions) => Some(normalized_output(bytes, dimensions)),
                Err(_) => None,
            }
        }
        Err(_) => None,
    }
}

pub(crate) fn sips_exists() -> bool {
    Command::new("sips")
        .arg("--help")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

fn skip(reason: NormalizeSkipReason, detail: impl Into<String>) -> NormalizeOutput {
    NormalizeOutput::Skip {
        reason,
        detail: detail.into(),
    }
}

impl std::fmt::Display for NormalizeSkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NormalizeSkipReason::UnsupportedFormat => f.write_str("unsupported_format"),
            NormalizeSkipReason::CorruptImage => f.write_str("corrupt_image"),
            NormalizeSkipReason::TooLarge => f.write_str("too_large"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::gif::{GifEncoder, Repeat};
    use image::{ImageBuffer, Rgba};
    use std::fs;
    use std::path::Path;

    fn ext(path: &Path) -> String {
        path.extension().unwrap().to_string_lossy().into_owned()
    }

    fn normalize_path(path: &Path, extension: &str, force_reencode: bool) -> NormalizeOutput {
        normalize_image(&File::open(path).unwrap(), extension, force_reencode)
    }

    #[test]
    fn tiny_png_encodes_to_jpeg() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiny.png");
        let img = ImageBuffer::from_pixel(10, 8, Rgba([255u8, 0, 0, 255]));
        img.save(&path).unwrap();

        let out = normalize_path(&path, &ext(&path), false);
        match out {
            NormalizeOutput::Normalized(normalized) => {
                assert_eq!((normalized.width, normalized.height), (10, 8));
                assert!(normalized.jpeg_bytes.starts_with(&[0xff, 0xd8]));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn oversized_input_is_rejected_before_decode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oversized.png");
        File::create(&path)
            .unwrap()
            .set_len(MAX_INPUT_BYTES + 1)
            .unwrap();

        let out = normalize_path(&path, "png", false);

        assert!(matches!(
            out,
            NormalizeOutput::Skip {
                reason: NormalizeSkipReason::TooLarge,
                ..
            }
        ));
    }

    #[test]
    fn oversized_png_resizes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.png");
        let img = ImageBuffer::from_pixel(2000, 1000, Rgba([0u8, 255, 0, 255]));
        img.save(&path).unwrap();

        let out = normalize_path(&path, &ext(&path), false);
        match out {
            NormalizeOutput::Normalized(normalized) => {
                assert_eq!(normalized.width, 1600);
                assert_eq!(normalized.height, 800);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn animated_gif_uses_first_frame() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("anim.gif");
        let file = fs::File::create(&path).unwrap();
        let mut encoder = GifEncoder::new(file);
        encoder.set_repeat(Repeat::Infinite).unwrap();
        let frame = image::Frame::new(ImageBuffer::from_pixel(4, 3, Rgba([1u8, 2, 3, 255])));
        encoder.encode_frame(frame).unwrap();

        let out = normalize_path(&path, &ext(&path), false);
        assert!(matches!(out, NormalizeOutput::Normalized(_)));
    }

    #[test]
    fn random_bytes_jpg_is_corrupt_or_unsupported_without_sips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.jpg");
        fs::write(&path, b"\x13\x99\x00not an image\xff\x00").unwrap();

        let out = normalize_path(&path, &ext(&path), false);
        match out {
            NormalizeOutput::Skip { reason, .. } => {
                assert!(matches!(
                    reason,
                    NormalizeSkipReason::CorruptImage | NormalizeSkipReason::UnsupportedFormat
                ));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn orientation_transform_swaps_dimensions() {
        let mut image =
            DynamicImage::ImageRgba8(ImageBuffer::from_pixel(2, 3, Rgba([0u8, 0, 0, 0])));
        image.apply_orientation(Orientation::Rotate90);
        assert_eq!(image.dimensions(), (3, 2));
    }

    #[test]
    fn f11_pixel_limit_exceeded_maps_to_too_large() {
        let reason = NormalizeSkipReason::TooLarge;
        assert_eq!(reason.to_string(), "too_large");
    }

    fn bmp_header(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(54);
        bytes.extend_from_slice(b"BM");
        bytes.extend_from_slice(&54_u32.to_le_bytes());
        bytes.extend_from_slice(&[0; 4]);
        bytes.extend_from_slice(&54_u32.to_le_bytes());
        bytes.extend_from_slice(&40_u32.to_le_bytes());
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&24_u16.to_le_bytes());
        bytes.extend_from_slice(&[0; 24]);
        bytes
    }

    #[test]
    fn decompression_bomb_dimensions_are_rejected_before_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bomb.bmp");
        fs::write(&path, bmp_header(DECODE_PIXEL_LIMIT + 1, 1)).unwrap();

        let out = normalize_path(&path, "bmp", false);
        assert!(
            matches!(
                &out,
                NormalizeOutput::Skip {
                    reason: NormalizeSkipReason::TooLarge,
                    detail,
                } if detail.contains("decode limit exceeded")
            ),
            "unexpected output: {out:?}"
        );
    }

    #[test]
    fn decompression_bomb_allocation_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bomb.bmp");
        fs::write(&path, bmp_header(6_000, 6_000)).unwrap();

        let out = normalize_path(&path, "bmp", false);
        assert!(
            matches!(
                &out,
                NormalizeOutput::Skip {
                    reason: NormalizeSkipReason::TooLarge,
                    detail,
                } if detail.contains("decode limit exceeded")
            ),
            "unexpected output: {out:?}"
        );
    }

    fn build_exif_app1_orientation_6() -> Vec<u8> {
        // APP1 marker
        let mut segment = vec![0xFF, 0xE1];
        // Payload: "Exif\0\0" + TIFF header (LE) + one IFD entry (Orientation=6)
        let mut payload = Vec::new();
        payload.extend_from_slice(b"Exif\x00\x00");
        // TIFF header (little-endian)
        payload.extend_from_slice(&[0x49, 0x49, 0x2A, 0x00]); // II + magic 42
        payload.extend_from_slice(&[0x08, 0x00, 0x00, 0x00]); // IFD offset = 8
        payload.extend_from_slice(&[0x01, 0x00]);
        // Entry: tag 0x0112 (Orientation), type 3 (SHORT), count 1, value 6
        payload.extend_from_slice(&[0x12, 0x01]);
        payload.extend_from_slice(&[0x03, 0x00]);
        payload.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]);
        payload.extend_from_slice(&[0x06, 0x00, 0x00, 0x00]);
        // Next IFD offset = 0 (no more IFDs)
        payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        // Length field (includes itself, 2 bytes)
        let len = (payload.len() + 2) as u16;
        segment.extend_from_slice(&len.to_be_bytes());
        segment.extend_from_slice(&payload);
        segment
    }

    #[test]
    fn f12_exif_orientation_6_swaps_dimensions_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oriented.jpg");

        let img = ImageBuffer::from_pixel(4, 2, Rgba([200u8, 100, 50, 255]));
        let mut jpeg_bytes = Vec::new();
        {
            let rgb = image::DynamicImage::ImageRgba8(img).to_rgb8();
            let (w, h) = rgb.dimensions();
            let mut encoder = JpegEncoder::new_with_quality(&mut jpeg_bytes, 90);
            encoder
                .encode(&rgb, w, h, image::ExtendedColorType::Rgb8)
                .unwrap();
        }

        assert_eq!(&jpeg_bytes[..2], &[0xFF, 0xD8]);
        let app1 = build_exif_app1_orientation_6();
        let mut spliced = Vec::with_capacity(jpeg_bytes.len() + app1.len());
        spliced.extend_from_slice(&jpeg_bytes[..2]);
        spliced.extend_from_slice(&app1);
        spliced.extend_from_slice(&jpeg_bytes[2..]);
        fs::write(&path, &spliced).unwrap();

        let out = normalize_path(&path, "jpg", false);
        match out {
            NormalizeOutput::Normalized(normalized) => {
                assert_eq!(
                    (normalized.width, normalized.height),
                    (2, 4),
                    "orientation should swap 4x2 to 2x4"
                );
            }
            NormalizeOutput::Skip { reason, detail } => {
                panic!("expected normalized output, got skip: {reason} — {detail}");
            }
        }
    }
}
