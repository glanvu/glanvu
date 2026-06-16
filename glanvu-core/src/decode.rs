// SPDX-License-Identifier: Apache-2.0

//! Decode images into a normalized in-memory frame, and read lightweight metadata.
//!
//! Everything is normalized to 8-bit RGBA (`width * height * 4` bytes). Higher bit depths and HDR
//! are a later concern; for the Phase 1 viewer/batch, RGBA8 is the common denominator.

use std::path::Path;

use crate::error::{Error, Result};
use crate::format::SourceFormat;

/// A decoded image, normalized to 8-bit RGBA.
#[derive(Debug, Clone)]
pub struct DecodedImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Pixel data, row-major, 4 bytes (R, G, B, A) per pixel. Length is `width * height * 4`.
    pub rgba: Vec<u8>,
}

/// Lightweight metadata, readable from the file header without a full decode.
#[derive(Debug, Clone)]
pub struct ImageMeta {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Detected source format.
    pub format: SourceFormat,
    /// File size in bytes.
    pub file_size: u64,
}

/// Decode an image from in-memory bytes into normalized RGBA.
pub fn decode_bytes(bytes: &[u8]) -> Result<DecodedImage> {
    let img = image::load_from_memory(bytes).map_err(map_image_error)?;
    Ok(to_decoded(img))
}

/// Decode an image file from disk into normalized RGBA.
pub fn decode_path<P: AsRef<Path>>(path: P) -> Result<DecodedImage> {
    let path = path.as_ref();
    let img = open_reader(path)?.decode().map_err(map_image_error)?;
    Ok(to_decoded(img))
}

/// Decode and resize to a thumbnail that fits within `max_w × max_h`, preserving aspect ratio.
///
/// Uses `thumbnail()` from the `image` crate (nearest/triangle, fast for preview generation).
/// The returned image may be smaller than the requested size if the source is already smaller.
pub fn decode_thumbnail<P: AsRef<Path>>(path: P, max_w: u32, max_h: u32) -> Result<DecodedImage> {
    let path = path.as_ref();
    let img = open_reader(path)?.decode().map_err(map_image_error)?;
    let thumb = img.thumbnail(max_w.max(1), max_h.max(1));
    Ok(to_decoded(thumb))
}

/// Read width/height/format/size from a file header, without decoding the pixels.
pub fn read_meta_path<P: AsRef<Path>>(path: P) -> Result<ImageMeta> {
    let path = path.as_ref();
    let file_size = std::fs::metadata(path)
        .map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?
        .len();

    let reader = open_reader(path)?;
    let format = reader
        .format()
        .and_then(SourceFormat::from_image)
        .ok_or(Error::UnsupportedFormat)?;
    let (width, height) = reader.into_dimensions().map_err(map_image_error)?;

    Ok(ImageMeta {
        width,
        height,
        format,
        file_size,
    })
}

/// Open an `image` reader for a path, guessing the format from content (not just the extension).
fn open_reader(path: &Path) -> Result<image::ImageReader<std::io::BufReader<std::fs::File>>> {
    image::ImageReader::open(path)
        .map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?
        .with_guessed_format()
        .map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })
}

fn to_decoded(img: image::DynamicImage) -> DecodedImage {
    let rgba = img.to_rgba8();
    DecodedImage {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    }
}

pub(crate) fn map_image_error(err: image::ImageError) -> Error {
    match err {
        image::ImageError::Unsupported(_) => Error::UnsupportedFormat,
        other => Error::Decode(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::detect_format;
    use image::{DynamicImage, ImageFormat, RgbaImage};

    /// Build a tiny 3x2 test image with varied pixels.
    fn sample() -> DynamicImage {
        let mut img = RgbaImage::new(3, 2);
        for (i, px) in img.pixels_mut().enumerate() {
            let v = (i as u8).wrapping_mul(40);
            *px = image::Rgba([v, 255 - v, v / 2, 255]);
        }
        DynamicImage::ImageRgba8(img)
    }

    /// Encode the sample to a given format, or `None` if `image` can't encode it here.
    fn encode(format: ImageFormat) -> Option<Vec<u8>> {
        let mut buf = std::io::Cursor::new(Vec::new());
        sample()
            .write_to(&mut buf, format)
            .ok()
            .map(|()| buf.into_inner())
    }

    #[test]
    fn roundtrip_decodes_base_formats() {
        let formats = [
            ImageFormat::Png,
            ImageFormat::Jpeg,
            ImageFormat::Gif,
            ImageFormat::Bmp,
            ImageFormat::Tiff,
            ImageFormat::WebP,
        ];
        let mut decoded_any = false;
        for fmt in formats {
            let Some(bytes) = encode(fmt) else { continue };
            decoded_any = true;
            let img = decode_bytes(&bytes).unwrap_or_else(|e| panic!("decode {fmt:?} failed: {e}"));
            assert_eq!((img.width, img.height), (3, 2), "dims for {fmt:?}");
            assert_eq!(img.rgba.len(), 3 * 2 * 4, "rgba length for {fmt:?}");
        }
        assert!(
            decoded_any,
            "no base format could be encoded for the round-trip test"
        );
    }

    #[test]
    fn detect_matches_encoded_format() {
        let bytes = encode(ImageFormat::Png).expect("PNG must be encodable");
        assert_eq!(detect_format(&bytes), Some(SourceFormat::Png));
    }

    #[test]
    fn corrupt_bytes_error_not_panic() {
        let err = decode_bytes(b"definitely not an image").unwrap_err();
        matches!(err, Error::UnsupportedFormat | Error::Decode(_))
            .then_some(())
            .expect("garbage input should be a clean error");
    }
}
