// SPDX-License-Identifier: Apache-2.0

//! Source image formats Glanvu recognizes, and content-based detection.

/// An image container/codec Glanvu can decode.
///
/// This is the Phase 1 base set (all pure-Rust decoders). It is `#[non_exhaustive]` because the
/// long-tail formats (RAW, HEIF, JPEG XL, DICOM, ...) arrive via the plugin layer in later phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SourceFormat {
    Jpeg,
    Png,
    Gif,
    Bmp,
    Tiff,
    WebP,
}

impl SourceFormat {
    /// A short, stable, human-facing name (e.g. for the `info` command).
    pub fn name(self) -> &'static str {
        match self {
            SourceFormat::Jpeg => "JPEG",
            SourceFormat::Png => "PNG",
            SourceFormat::Gif => "GIF",
            SourceFormat::Bmp => "BMP",
            SourceFormat::Tiff => "TIFF",
            SourceFormat::WebP => "WebP",
        }
    }

    /// Parse a target format from an extension-like string (e.g. "jpg", "PNG", "tiff").
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" => Some(SourceFormat::Jpeg),
            "png" => Some(SourceFormat::Png),
            "gif" => Some(SourceFormat::Gif),
            "bmp" => Some(SourceFormat::Bmp),
            "tif" | "tiff" => Some(SourceFormat::Tiff),
            "webp" => Some(SourceFormat::WebP),
            _ => None,
        }
    }

    /// The `image` crate format used to encode this `SourceFormat`.
    pub(crate) fn to_image(self) -> image::ImageFormat {
        match self {
            SourceFormat::Jpeg => image::ImageFormat::Jpeg,
            SourceFormat::Png => image::ImageFormat::Png,
            SourceFormat::Gif => image::ImageFormat::Gif,
            SourceFormat::Bmp => image::ImageFormat::Bmp,
            SourceFormat::Tiff => image::ImageFormat::Tiff,
            SourceFormat::WebP => image::ImageFormat::WebP,
        }
    }

    /// Map an `image::ImageFormat` to a Glanvu `SourceFormat`, if it is in our base set.
    pub(crate) fn from_image(format: image::ImageFormat) -> Option<Self> {
        match format {
            image::ImageFormat::Jpeg => Some(SourceFormat::Jpeg),
            image::ImageFormat::Png => Some(SourceFormat::Png),
            image::ImageFormat::Gif => Some(SourceFormat::Gif),
            image::ImageFormat::Bmp => Some(SourceFormat::Bmp),
            image::ImageFormat::Tiff => Some(SourceFormat::Tiff),
            image::ImageFormat::WebP => Some(SourceFormat::WebP),
            _ => None,
        }
    }
}

/// Detect the image format from the leading bytes (magic numbers), without decoding.
///
/// Returns `None` if the bytes are not a recognized image or are a format outside the base set.
pub fn detect_format(bytes: &[u8]) -> Option<SourceFormat> {
    image::guess_format(bytes)
        .ok()
        .and_then(SourceFormat::from_image)
}
