// SPDX-License-Identifier: Apache-2.0

//! Batch-friendly image conversion: decode a file, optionally resize, and re-encode to a target
//! format. Used by the `glanvu convert` CLI; headless (no GPU).

use std::path::Path;

use crate::decode::map_image_error;
use crate::error::{Error, Result};
use crate::format::SourceFormat;

/// Resize dimensions above `MAX_RESIZE_DIM` are rejected to prevent runaway memory allocation (a
/// concern both for the CLI and, more critically, for the future hosted API).
pub const MAX_RESIZE_DIM: u32 = 32_768;

/// A fixed-angle clockwise rotation applied during conversion.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum Rotation {
    #[default]
    None,
    Cw90,
    Cw180,
    Cw270,
}

/// Transformations applied during a batch conversion, in pipeline order:
/// decode → `crop` → `rotate` → `resize` → encode (`quality`).
///
/// All fields default to "no-op", so `ConvertOptions::default()` is a plain format conversion.
#[derive(Clone, Copy, Default)]
pub struct ConvertOptions {
    /// Crop a `(x, y, w, h)` region (pixels, on the decoded image) before any other step.
    pub crop: Option<(u32, u32, u32, u32)>,
    /// Fixed-angle rotation.
    pub rotate: Rotation,
    /// Maximum bounding box: scale to fit within `(w, h)` preserving aspect ratio. A zero
    /// dimension is ignored.
    pub resize: Option<(u32, u32)>,
    /// Encoder quality `1..=100`. Only honored for JPEG output; ignored for other formats.
    pub quality: Option<u8>,
}

/// Encode an already-decoded image to a file. Used by the thumbnail disk cache.
pub fn encode_to_file(
    image: &crate::decode::DecodedImage,
    output: &Path,
    target: SourceFormat,
) -> Result<()> {
    let raw = image.rgba.clone();
    let rgba_img = image::RgbaImage::from_raw(image.width, image.height, raw)
        .ok_or_else(|| crate::error::Error::Decode("invalid decoded image buffer".to_string()))?;
    image::DynamicImage::ImageRgba8(rgba_img)
        .save_with_format(output, target.to_image())
        .map_err(map_image_error)
}

pub fn convert_file(
    input: &Path,
    output: &Path,
    target: SourceFormat,
    opts: &ConvertOptions,
) -> Result<()> {
    let reader = image::ImageReader::open(input)
        .map_err(|source| Error::Io {
            path: input.to_path_buf(),
            source,
        })?
        .with_guessed_format()
        .map_err(|source| Error::Io {
            path: input.to_path_buf(),
            source,
        })?;
    let mut image = reader.decode().map_err(map_image_error)?;

    // 1. Crop (pixels on the decoded image), validated against the actual dimensions.
    if let Some((x, y, w, h)) = opts.crop {
        let (iw, ih) = (image.width(), image.height());
        if w == 0 || h == 0 {
            return Err(Error::Decode(
                "crop width and height must be > 0".to_string(),
            ));
        }
        if x.saturating_add(w) > iw || y.saturating_add(h) > ih {
            return Err(Error::Decode(format!(
                "crop region {x},{y} {w}x{h} is outside the {iw}x{ih} image"
            )));
        }
        image = image.crop_imm(x, y, w, h);
    }

    // 2. Rotate.
    image = match opts.rotate {
        Rotation::None => image,
        Rotation::Cw90 => image.rotate90(),
        Rotation::Cw180 => image.rotate180(),
        Rotation::Cw270 => image.rotate270(),
    };

    // 3. Resize (fit within box, aspect preserved).
    if let Some((w, h)) = opts.resize {
        if w > MAX_RESIZE_DIM || h > MAX_RESIZE_DIM {
            return Err(Error::Decode(format!(
                "resize dimensions {w}x{h} exceed the maximum allowed ({MAX_RESIZE_DIM})"
            )));
        }
        if w > 0 && h > 0 {
            image = image.resize(w, h, image::imageops::FilterType::Lanczos3);
        }
    }

    // 4. Encode. JPEG honors quality; other formats use the codec default.
    match (target, opts.quality) {
        (SourceFormat::Jpeg, Some(q)) => {
            let q = q.clamp(1, 100);
            let mut file =
                std::io::BufWriter::new(std::fs::File::create(output).map_err(|source| {
                    Error::Io {
                        path: output.to_path_buf(),
                        source,
                    }
                })?);
            // JPEG has no alpha channel; flatten to RGB before encoding.
            let rgb = image.to_rgb8();
            let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut file, q);
            encoder.encode_image(&rgb).map_err(map_image_error)
        }
        _ => image
            .save_with_format(output, target.to_image())
            .map_err(map_image_error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        // A unique subdir per test: these tests run in parallel and must not share files.
        let dir = std::env::temp_dir().join(format!("glanvu-convert-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_png(path: &Path, w: u32, h: u32) {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([20, 120, 200, 255]));
        image::DynamicImage::ImageRgba8(img).save(path).unwrap();
    }

    #[test]
    fn converts_format_preserving_dimensions() {
        let dir = temp_dir("convert");
        let src = dir.join("src.png");
        write_png(&src, 12, 9);

        let dst = dir.join("out.jpg");
        convert_file(&src, &dst, SourceFormat::Jpeg, &ConvertOptions::default()).unwrap();

        let meta = crate::read_meta_path(&dst).unwrap();
        assert_eq!(meta.format, SourceFormat::Jpeg);
        assert_eq!((meta.width, meta.height), (12, 9));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resize_fits_within_box_keeping_aspect() {
        let dir = temp_dir("resize");
        let src = dir.join("src.png");
        write_png(&src, 20, 10);

        let dst = dir.join("small.png");
        let opts = ConvertOptions {
            resize: Some((10, 10)),
            ..Default::default()
        };
        convert_file(&src, &dst, SourceFormat::Png, &opts).unwrap();

        let meta = crate::read_meta_path(&dst).unwrap();
        // 20x10 fit within 10x10 -> 10x5 (aspect preserved).
        assert_eq!((meta.width, meta.height), (10, 5));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn crop_extracts_region() {
        let dir = temp_dir("crop");
        let src = dir.join("src.png");
        write_png(&src, 40, 30);

        let dst = dir.join("cropped.png");
        let opts = ConvertOptions {
            crop: Some((5, 5, 20, 10)),
            ..Default::default()
        };
        convert_file(&src, &dst, SourceFormat::Png, &opts).unwrap();

        let meta = crate::read_meta_path(&dst).unwrap();
        assert_eq!((meta.width, meta.height), (20, 10));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn crop_out_of_bounds_errors() {
        let dir = temp_dir("crop-oob");
        let src = dir.join("src.png");
        write_png(&src, 40, 30);

        let dst = dir.join("bad.png");
        let opts = ConvertOptions {
            crop: Some((30, 0, 20, 10)), // 30 + 20 > 40
            ..Default::default()
        };
        assert!(convert_file(&src, &dst, SourceFormat::Png, &opts).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotate_swaps_dimensions() {
        let dir = temp_dir("rotate");
        let src = dir.join("src.png");
        write_png(&src, 40, 10);

        let dst = dir.join("rot.png");
        let opts = ConvertOptions {
            rotate: Rotation::Cw90,
            ..Default::default()
        };
        convert_file(&src, &dst, SourceFormat::Png, &opts).unwrap();

        let meta = crate::read_meta_path(&dst).unwrap();
        // 40x10 rotated 90° -> 10x40.
        assert_eq!((meta.width, meta.height), (10, 40));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quality_reduces_jpeg_size() {
        let dir = temp_dir("quality");
        let src = dir.join("src.png");
        // A noisy image so JPEG quality actually matters.
        let mut img = image::RgbaImage::new(128, 128);
        for (x, y, px) in img.enumerate_pixels_mut() {
            let v = ((x * 17 + y * 31) % 256) as u8;
            *px = image::Rgba([v, v.wrapping_mul(3), v.wrapping_add(80), 255]);
        }
        image::DynamicImage::ImageRgba8(img).save(&src).unwrap();

        let low = dir.join("low.jpg");
        let high = dir.join("high.jpg");
        convert_file(
            &src,
            &low,
            SourceFormat::Jpeg,
            &ConvertOptions {
                quality: Some(10),
                ..Default::default()
            },
        )
        .unwrap();
        convert_file(
            &src,
            &high,
            SourceFormat::Jpeg,
            &ConvertOptions {
                quality: Some(95),
                ..Default::default()
            },
        )
        .unwrap();

        let low_size = std::fs::metadata(&low).unwrap().len();
        let high_size = std::fs::metadata(&high).unwrap().len();
        assert!(
            low_size < high_size,
            "q=10 ({low_size}B) should be smaller than q=95 ({high_size}B)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
