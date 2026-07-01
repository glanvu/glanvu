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
    /// A vector format (XML), unlike every other variant here. Input-only: there is no `image`
    /// crate encode target, so `to_image()` returns `None` for it (see D11 in the decision log).
    Svg,
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
            SourceFormat::Svg => "SVG",
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
            "svg" => Some(SourceFormat::Svg),
            _ => None,
        }
    }

    /// The `image` crate format used to encode this `SourceFormat`, if any.
    ///
    /// `None` for `Svg`: it isn't an `image`-crate encode target (no raster→vector conversion).
    pub(crate) fn to_image(self) -> Option<image::ImageFormat> {
        match self {
            SourceFormat::Jpeg => Some(image::ImageFormat::Jpeg),
            SourceFormat::Png => Some(image::ImageFormat::Png),
            SourceFormat::Gif => Some(image::ImageFormat::Gif),
            SourceFormat::Bmp => Some(image::ImageFormat::Bmp),
            SourceFormat::Tiff => Some(image::ImageFormat::Tiff),
            SourceFormat::WebP => Some(image::ImageFormat::WebP),
            SourceFormat::Svg => None,
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

/// Bytes scanned from the start of a file when sniffing for SVG content.
const SVG_SNIFF_WINDOW: usize = 4096;

/// Whether `prefix` looks like SVG content.
///
/// SVG has no magic bytes (it's XML text), so unlike every other format here, detection is a
/// bounded text scan rather than a fixed byte pattern: skip a possible UTF-8 BOM/leading
/// whitespace, then look for an opening `<svg` tag within the first [`SVG_SNIFF_WINDOW`] bytes
/// (tolerating an `<?xml ...?>` prolog and/or `<!DOCTYPE ...>` before it, and any surrounding
/// comments/whitespace).
pub(crate) fn sniff_svg(prefix: &[u8]) -> bool {
    let window = &prefix[..prefix.len().min(SVG_SNIFF_WINDOW)];
    let text = String::from_utf8_lossy(window);
    text.trim_start_matches('\u{feff}').contains("<svg")
}

/// Detect the image format from the leading bytes (magic numbers, or a content sniff for SVG),
/// without decoding.
///
/// Returns `None` if the bytes are not a recognized image or are a format outside the base set.
pub fn detect_format(bytes: &[u8]) -> Option<SourceFormat> {
    image::guess_format(bytes)
        .ok()
        .and_then(SourceFormat::from_image)
        .or_else(|| sniff_svg(bytes).then_some(SourceFormat::Svg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn svg_extension_is_case_insensitive() {
        assert_eq!(SourceFormat::from_extension("svg"), Some(SourceFormat::Svg));
        assert_eq!(SourceFormat::from_extension("SVG"), Some(SourceFormat::Svg));
    }

    #[test]
    fn svg_has_no_encode_target() {
        assert_eq!(SourceFormat::Svg.to_image(), None);
        assert_eq!(SourceFormat::Png.to_image(), Some(image::ImageFormat::Png));
    }

    #[test]
    fn sniff_svg_plain_tag() {
        assert!(sniff_svg(b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>"));
    }

    #[test]
    fn sniff_svg_with_xml_prolog_and_doctype() {
        let svg = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                    <!DOCTYPE svg PUBLIC \"-//W3C//DTD SVG 1.1//EN\">\n\
                    <svg xmlns=\"http://www.w3.org/2000/svg\"></svg>";
        assert!(sniff_svg(svg));
    }

    #[test]
    fn sniff_svg_with_leading_bom() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF]; // UTF-8 BOM
        bytes.extend_from_slice(b"<svg></svg>");
        assert!(sniff_svg(&bytes));
    }

    #[test]
    fn sniff_svg_rejects_non_svg_content() {
        assert!(!sniff_svg(b"\x89PNG\r\n\x1a\n"));
        assert!(!sniff_svg(b"just some plain text, no markup here"));
    }

    #[test]
    fn detect_format_finds_svg_by_content() {
        let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"10\" height=\"6\"></svg>";
        assert_eq!(detect_format(svg), Some(SourceFormat::Svg));
    }
}
