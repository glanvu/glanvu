// SPDX-License-Identifier: Apache-2.0

//! `glanvu-core` is the reusable engine behind Glanvu: it detects image formats, decodes images
//! into a normalized in-memory RGBA frame, and reads lightweight metadata from a file header.
//!
//! It is deliberately free of any GUI, windowing or GPU code so it can be reused headlessly (the
//! batch CLI) and, later, by other tools. See `WIP/glanvu/doc/plans/glanvu.phase-1-plan.md`.
//!
//! Phase 1 base formats (pure-Rust decoders, no system C libraries): JPEG, PNG, GIF, BMP, TIFF,
//! WebP. SVG (vector, via `resvg`) was added in D11 — see the decision log. PDF (paginated, via
//! the native PDFium library) was added in D13. AVIF and the remaining long-tail formats arrive
//! later via a plugin layer.

mod convert;
mod decode;
mod error;
mod exif;
mod folder;
mod format;
mod pdf;
mod text;

pub use convert::{convert_file, encode_to_file, ConvertOptions, Rotation};
pub use decode::{
    decode_bytes, decode_path, decode_pdf_page, decode_svg_at_size, decode_text_page,
    decode_thumbnail, fit_within, read_meta_path, DecodedImage, ImageMeta, SvgDocument,
};
pub use error::{Error, Result};
pub use exif::{read_exif_bytes, read_exif_path, ExifMeta};
pub use folder::{
    is_csv_path, is_diff_path, is_ini_path, is_json_path, is_md_path, is_pdf_path,
    is_supported_path, is_svg_path, is_text_path, is_toml_path, is_tsv_path, is_xml_path,
    is_yaml_path, list_images,
};
pub use format::{detect_format, SourceFormat};
pub use pdf::PdfDocument;
pub use text::{
    char_width_for_font, line_height_for_font, pretty_format_json, pretty_format_xml, DocMatch,
    PageLayout, TextDocument, DEFAULT_FONT_SIZE, DEFAULT_FONT_SIZE_INDEX, FONT_SIZES,
    MAX_UNWRAPPED_PAGE_WIDTH, TEXT_PAGE_HEIGHT, TEXT_PAGE_WIDTH,
};

/// The crate version, taken from `Cargo.toml` at build time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::VERSION;

    #[test]
    fn version_is_populated() {
        assert!(!VERSION.is_empty());
    }
}
