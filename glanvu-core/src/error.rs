// SPDX-License-Identifier: Apache-2.0

//! Error type for `glanvu-core`.

use std::path::PathBuf;

/// Errors that can occur while reading, detecting or decoding an image.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// An I/O failure while accessing a file on disk.
    #[error("I/O error for {path}: {source}")]
    Io {
        /// The path that was being accessed.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The bytes/file are not a recognized or supported image format.
    #[error("unsupported or unrecognized image format")]
    UnsupportedFormat,

    /// The format was recognized but the data could not be decoded.
    #[error("failed to decode image: {0}")]
    Decode(String),

    /// A PDF file was opened, but the bundled PDFium native library could not be located or
    /// bound on this machine (see D13 in the decision log). PDF support is simply unavailable;
    /// every other format is unaffected.
    #[error("PDF support unavailable: the PDFium library could not be loaded ({0})")]
    PdfLibraryMissing(String),
}

/// Convenience alias for results from this crate.
pub type Result<T> = std::result::Result<T, Error>;
