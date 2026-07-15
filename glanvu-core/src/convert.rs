// SPDX-License-Identifier: Apache-2.0

//! Batch-friendly image conversion: decode a file, optionally resize, and re-encode to a target
//! format. Used by the `glanvu convert` CLI; headless (no GPU).

use std::path::Path;

use crate::decode::{decode_path, map_image_error};
use crate::error::{Error, Result};
use crate::folder::{is_pdf_path, is_svg_path};
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
    let format = target.to_image().ok_or(Error::UnsupportedFormat)?;
    image::DynamicImage::ImageRgba8(rgba_img)
        .save_with_format(output, format)
        .map_err(map_image_error)
}

pub fn convert_file(
    input: &Path,
    output: &Path,
    target: SourceFormat,
    opts: &ConvertOptions,
) -> Result<()> {
    // SVG and PDF have no lazy/streaming decode (see decode.rs); reuse `decode_path`'s branch for
    // each (SVG rasterizes at intrinsic size, PDF rasterizes page 1 at its intrinsic size — see
    // D13 in the decision log) and convert into a `DynamicImage` so crop/rotate/resize below are
    // unchanged for every format. Raster formats keep the existing lazy `ImageReader` path.
    let mut image = if is_svg_path(input) || is_pdf_path(input) {
        let decoded = decode_path(input)?;
        image::RgbaImage::from_raw(decoded.width, decoded.height, decoded.rgba)
            .map(image::DynamicImage::ImageRgba8)
            .ok_or_else(|| Error::Decode("invalid decoded image buffer".to_string()))?
    } else {
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
        reader.decode().map_err(map_image_error)?
    };

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
        _ => {
            let format = target.to_image().ok_or(Error::UnsupportedFormat)?;
            image
                .save_with_format(output, format)
                .map_err(map_image_error)
        }
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

    const SAMPLE_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10" viewBox="0 0 20 10">
<rect width="20" height="10" fill="#ff6633"/>
</svg>"##;

    #[test]
    fn convert_svg_to_png_uses_intrinsic_size() {
        let dir = temp_dir("svg-to-png");
        let src = dir.join("src.svg");
        std::fs::write(&src, SAMPLE_SVG).unwrap();

        let dst = dir.join("out.png");
        convert_file(&src, &dst, SourceFormat::Png, &ConvertOptions::default()).unwrap();

        let meta = crate::read_meta_path(&dst).unwrap();
        assert_eq!(meta.format, SourceFormat::Png);
        assert_eq!((meta.width, meta.height), (20, 10));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn convert_svg_to_png_with_resize() {
        let dir = temp_dir("svg-to-png-resize");
        let src = dir.join("src.svg");
        std::fs::write(&src, SAMPLE_SVG).unwrap();

        let dst = dir.join("out.png");
        let opts = ConvertOptions {
            resize: Some((10, 10)),
            ..Default::default()
        };
        convert_file(&src, &dst, SourceFormat::Png, &opts).unwrap();

        let meta = crate::read_meta_path(&dst).unwrap();
        // 20x10 fit within 10x10 -> 10x5 (aspect preserved), same as any other format.
        assert_eq!((meta.width, meta.height), (10, 5));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn convert_to_svg_is_rejected() {
        let dir = temp_dir("to-svg-rejected");
        let src = dir.join("src.png");
        write_png(&src, 12, 9);

        let dst = dir.join("out.svg");
        let err =
            convert_file(&src, &dst, SourceFormat::Svg, &ConvertOptions::default()).unwrap_err();
        assert!(matches!(err, Error::UnsupportedFormat));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn convert_to_pdf_is_rejected() {
        let dir = temp_dir("to-pdf-rejected");
        let src = dir.join("src.png");
        write_png(&src, 12, 9);

        let dst = dir.join("out.pdf");
        let err =
            convert_file(&src, &dst, SourceFormat::Pdf, &ConvertOptions::default()).unwrap_err();
        assert!(matches!(err, Error::UnsupportedFormat));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A minimal, byte-accurate one-page PDF (see the equivalent, more thoroughly commented
    /// builder in `decode.rs`'s test module — duplicated here rather than shared across a
    /// `#[cfg(test)]`-only boundary between crate modules).
    fn minimal_one_page_pdf(w: f32, h: f32) -> Vec<u8> {
        let objs = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] /Resources << >> >>"),
        ];
        let mut buf = Vec::new();
        buf.extend_from_slice(b"%PDF-1.4\n");
        let mut offsets = vec![0usize];
        for (i, body) in objs.iter().enumerate() {
            offsets.push(buf.len());
            buf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
        }
        let xref_offset = buf.len();
        let total = objs.len() + 1;
        buf.extend_from_slice(format!("xref\n0 {total}\n").as_bytes());
        buf.extend_from_slice(b"0000000000 65535 f \n");
        for &off in &offsets[1..] {
            buf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        buf.extend_from_slice(
            format!("trailer\n<< /Size {total} /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF")
                .as_bytes(),
        );
        buf
    }

    #[test]
    fn convert_pdf_to_png_uses_page_one() {
        // Skips (rather than fails) if no PDFium library is bound in this environment — see the
        // "PDF" test section in decode.rs for why that's the expected default outside packaging.
        let dir = temp_dir("pdf-to-png");
        let src = dir.join("src.pdf");
        std::fs::write(&src, minimal_one_page_pdf(200.0, 100.0)).unwrap();

        let dst = dir.join("out.png");
        match convert_file(&src, &dst, SourceFormat::Png, &ConvertOptions::default()) {
            Ok(()) => {
                let meta = crate::read_meta_path(&dst).unwrap();
                assert_eq!(meta.format, SourceFormat::Png);
                assert_eq!((meta.width, meta.height), (200, 100));
            }
            Err(Error::PdfLibraryMissing(msg)) => {
                eprintln!("skipping: {msg}");
            }
            Err(e) => panic!("unexpected error: {e}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
