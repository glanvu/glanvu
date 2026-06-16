// SPDX-License-Identifier: Apache-2.0

//! `glanvu-core` is the reusable engine behind Glanvu: it detects image formats, decodes images
//! into a normalized in-memory RGBA frame, and reads lightweight metadata from a file header.
//!
//! It is deliberately free of any GUI, windowing or GPU code so it can be reused headlessly (the
//! batch CLI) and, later, by other tools. See `WIP/glanvu/doc/plans/glanvu.phase-1-plan.md`.
//!
//! Phase 1 base formats (pure-Rust decoders, no system C libraries): JPEG, PNG, GIF, BMP, TIFF,
//! WebP. AVIF and the long-tail formats arrive later via a plugin layer.

mod convert;
mod decode;
mod error;
mod folder;
mod format;

pub use convert::{convert_file, encode_to_file, ConvertOptions, Rotation};
pub use decode::{
    decode_bytes, decode_path, decode_thumbnail, read_meta_path, DecodedImage, ImageMeta,
};
pub use error::{Error, Result};
pub use folder::{is_supported_path, list_images};
pub use format::{detect_format, SourceFormat};

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
