// SPDX-License-Identifier: Apache-2.0

//! PDF support via `pdfium-render` + Google's PDFium, loaded as a native dynamic library at
//! runtime (see D13 in the decision log).
//!
//! Unlike every other decoder in this crate, PDF depends on a native shared library
//! (`libpdfium.dylib`/`.so`/`pdfium.dll`) that Glanvu does not build and cannot vendor into the
//! Rust binary — it's fetched separately per platform and placed next to the executable (see the
//! packaging scripts). Locating it is therefore a first-class, fallible, memoized operation (see
//! [`pdfium_binding`]): a missing library is a normal, clean [`Error::PdfLibraryMissing`], never a
//! panic. Every other format is unaffected by its absence.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use pdfium_render::prelude::{
    PdfDocument as NativeDocument, PdfPage as NativePage, PdfRenderConfig, Pdfium,
};

use crate::decode::DecodedImage;
use crate::error::{Error, Result};

/// Candidate directories to search for the bundled PDFium library, in priority order:
/// - `GLANVU_PDFIUM_LIB`: an explicit override, for local development (running via `cargo run`
///   with a manually-placed library) and advanced/packaging use.
/// - Next to the running executable — covers `cargo install`, the Linux `.tar.gz`/`.deb` layout,
///   and Windows (whose own DLL search order already checks this directory first anyway).
/// - macOS only: `../Frameworks` relative to the executable, i.e. `Contents/Frameworks/` inside
///   the `.app` bundle (`Contents/MacOS/Glanvu` → `Contents/Frameworks/libpdfium.dylib`).
fn candidate_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(p) = std::env::var_os("GLANVU_PDFIUM_LIB") {
        dirs.push(PathBuf::from(p));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            dirs.push(dir.to_path_buf());
            #[cfg(target_os = "macos")]
            dirs.push(dir.join("../Frameworks"));
        }
    }
    dirs
}

/// Bind to the bundled PDFium library, memoized for the process lifetime — binding loads a
/// dynamic library and resolves its symbol table, which isn't free, so this should happen at most
/// once per process. `None` (also memoized) means no usable library was found in any of
/// `candidate_dirs()`; callers surface that as [`Error::PdfLibraryMissing`], never a panic, so a
/// user without the library pays one fast "not found" lookup per process, not one per PDF opened.
fn pdfium_binding() -> Option<&'static Pdfium> {
    static BINDING: OnceLock<Option<Pdfium>> = OnceLock::new();
    BINDING
        .get_or_init(|| {
            candidate_dirs().into_iter().find_map(|dir| {
                let lib_path = Pdfium::pdfium_platform_library_name_at_path(&dir);
                Pdfium::bind_to_library(&lib_path).ok().map(Pdfium::new)
            })
        })
        .as_ref()
}

/// Serializes every call into PDFium across the whole process.
///
/// PDFium documents itself as not thread-safe ("Pdfium makes no guarantees about thread safety
/// and should be assumed not to be thread safe"). `pdfium-render`'s `thread_safe` feature (on by
/// default) claims to guard access with an internal mutex, but a real crash confirmed that isn't
/// airtight in practice: the folder-prefetch worker (`glanvu-viewer-core`, decoding a neighboring
/// PDF in the background) and the main thread (rendering the page just navigated to) each called
/// into PDFium at the same time, corrupting PDFium's process-global font glyph cache (crash inside
/// FreeType's `ft_smooth_render`, called from two different documents' text rendering
/// concurrently) — heap corruption, not a clean panic. `glanvu convert`'s parallel batch pipeline
/// (rayon) converting several PDFs at once would hit the same hazard. Rather than trust the
/// wrapper's internal locking, Glanvu holds its own lock around every `PdfDocument`
/// construction/query/render call, so at most one thread is ever inside PDFium at a time,
/// regardless of how many `PdfDocument`s exist or what thread created them.
static PDFIUM_LOCK: Mutex<()> = Mutex::new(());

fn with_pdfium<T>(f: impl FnOnce() -> T) -> T {
    let _guard = PDFIUM_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    f()
}

fn library_missing_error() -> Error {
    Error::PdfLibraryMissing(
        "no PDFium library found next to the executable (or via GLANVU_PDFIUM_LIB) — see \
         README's \"PDF support\" section for how to install it"
            .to_string(),
    )
}

/// A parsed PDF kept in memory so pages can be rendered without re-parsing — mirrors
/// [`crate::SvgDocument`]'s role for the vector pipeline (D11), adapted for pagination:
/// `page_count()` and `render_page(index, ...)` instead of a single `render_fit`. There is no
/// region/tile-render analog (D12's viewport tiling stays SVG-only — see D13 in the decision log).
///
/// Every constructor re-parses from disk/bytes (like `SvgDocument` today), which is more
/// expensive for PDF (xref table walk) than for SVG (XML tokenize) — callers that page through
/// the same document repeatedly (the viewer) should keep one `PdfDocument` alive across page
/// turns rather than reloading per page.
pub struct PdfDocument {
    // `Option` (not a bare `NativeDocument`) so `Drop` below can take it out and drop it while
    // still holding `PDFIUM_LOCK` — closing a PDFium document handle is itself a PDFium call, and
    // an unguarded drop would reopen exactly the concurrency hazard `PDFIUM_LOCK` exists to close.
    // Always `Some` from construction until `Drop` runs.
    document: Option<NativeDocument<'static>>,
    page_count: usize,
}

impl PdfDocument {
    /// Parse a PDF file from disk.
    ///
    /// `Err(Error::PdfLibraryMissing)` if the native PDFium library isn't available on this
    /// machine — every other format is unaffected by that failure.
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        with_pdfium(|| {
            let pdfium = pdfium_binding().ok_or_else(library_missing_error)?;
            let document = pdfium
                .load_pdf_from_file(path.as_ref(), None)
                .map_err(|e| Error::Decode(format!("PDF parse failed: {e}")))?;
            Self::from_native(document)
        })
    }

    /// Parse a PDF from in-memory bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        with_pdfium(|| {
            let pdfium = pdfium_binding().ok_or_else(library_missing_error)?;
            let document = pdfium
                .load_pdf_from_byte_vec(bytes.to_vec(), None)
                .map_err(|e| Error::Decode(format!("PDF parse failed: {e}")))?;
            Self::from_native(document)
        })
    }

    /// Must only be called while holding [`PDFIUM_LOCK`] (`pages().len()` is itself a PDFium
    /// call) — both callers above already run inside `with_pdfium`.
    fn from_native(document: NativeDocument<'static>) -> Result<Self> {
        let page_count = document.pages().len() as usize;
        Ok(PdfDocument {
            document: Some(document),
            page_count,
        })
    }

    /// Number of pages (`>= 1` for any document `load`/`from_bytes` successfully returned).
    /// Cached at construction time — does not touch PDFium, so it needs no lock.
    pub fn page_count(&self) -> usize {
        self.page_count
    }

    /// Intrinsic size of `page_index` (0-based) in PDF points (72 points = 1 inch).
    pub fn page_size(&self, page_index: usize) -> Result<(f32, f32)> {
        with_pdfium(|| {
            let page = self.page(page_index)?;
            Ok((page.width().value, page.height().value))
        })
    }

    /// Rasterize `page_index` (0-based) into an `out_w × out_h` RGBA buffer, stretched to that
    /// exact size. Callers that want the aspect ratio preserved compute `out_w`/`out_h`
    /// themselves first (the same convention as `SvgDocument::render_fit`). Out-of-range
    /// `page_index` is a clean `Err`, not a panic.
    pub fn render_page(&self, page_index: usize, out_w: u32, out_h: u32) -> Result<DecodedImage> {
        with_pdfium(|| {
            let page = self.page(page_index)?;
            let (out_w, out_h) = (out_w.max(1) as i32, out_h.max(1) as i32);
            let config = PdfRenderConfig::new().set_target_size(out_w, out_h);
            let bitmap = page
                .render_with_config(&config)
                .map_err(|e| Error::Decode(format!("PDF render failed: {e}")))?;
            Ok(DecodedImage {
                width: out_w as u32,
                height: out_h as u32,
                rgba: bitmap.as_rgba_bytes(),
            })
        })
    }

    /// Must only be called while holding [`PDFIUM_LOCK`] — both callers above already run inside
    /// `with_pdfium`. `self.document` is always `Some` until `Drop` runs, and nothing can call
    /// this on a `PdfDocument` that's already being dropped, so the `expect` can't actually fail.
    fn page(&self, page_index: usize) -> Result<NativePage<'static>> {
        self.document
            .as_ref()
            .expect("PdfDocument used after drop")
            .pages()
            .get(page_index as i32)
            .map_err(|_| Error::Decode(format!("page index {page_index} out of bounds")))
    }
}

impl Drop for PdfDocument {
    /// Closing a PDFium document handle is itself a PDFium call — must be serialized like every
    /// other access (see [`PDFIUM_LOCK`]'s doc comment), or dropping a document on one thread
    /// could still corrupt PDFium's shared internal state while another thread is mid-render.
    fn drop(&mut self) {
        let _guard = PDFIUM_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        drop(self.document.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_document_is_send_and_sync() {
        // The viewer keeps a live PdfDocument across page turns and (per D11's precedent) may
        // eventually hand it to a background render worker if dogfooding shows synchronous
        // rendering isn't fast enough — pdfium-render's default `thread_safe` feature makes this
        // safe.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<PdfDocument>();
    }

    #[test]
    fn missing_library_is_clean_error_not_panic() {
        // No PDFium library is bundled with `cargo test` by default — it's fetched separately per
        // platform at packaging time (see the build scripts and README's "PDF support" section),
        // so on a plain contributor machine `PdfDocument` is expected to fail cleanly rather than
        // panic. A `Decode` error (library found, but this one-line stub isn't a valid PDF) is
        // also an acceptable outcome for this test's purpose — proving the failure path is clean
        // either way; only an `Ok` or an unrelated error variant is a genuine failure.
        match PdfDocument::from_bytes(b"%PDF-1.4\n") {
            Err(Error::PdfLibraryMissing(_)) => {}
            Err(Error::Decode(_)) => {}
            Ok(_) => panic!("expected an error for a truncated one-line PDF stub"),
            Err(e) => panic!("expected PdfLibraryMissing or Decode, got: {e}"),
        }
    }
}
