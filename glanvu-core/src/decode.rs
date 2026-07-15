// SPDX-License-Identifier: Apache-2.0

//! Decode images into a normalized in-memory frame, and read lightweight metadata.
//!
//! Everything is normalized to 8-bit RGBA (`width * height * 4` bytes). Higher bit depths and HDR
//! are a later concern; for the Phase 1 viewer/batch, RGBA8 is the common denominator.

use std::path::Path;
use std::sync::{Arc, OnceLock};

use resvg::{tiny_skia, usvg};

use crate::error::{Error, Result};
use crate::folder::{is_pdf_path, is_svg_path};
use crate::format::{sniff_pdf, sniff_svg, SourceFormat};
use crate::pdf::PdfDocument;

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
    /// Number of pages, for paginated formats (currently just PDF). `None` for every other
    /// format — there is no notion of "page" for a single raster image or an SVG.
    pub page_count: Option<u32>,
}

/// Decode an image from in-memory bytes into normalized RGBA.
pub fn decode_bytes(bytes: &[u8]) -> Result<DecodedImage> {
    if sniff_svg(bytes) {
        return decode_svg_bytes(bytes, None);
    }
    if sniff_pdf(bytes) {
        return decode_pdf_page0(&PdfDocument::from_bytes(bytes)?);
    }
    let img = image::load_from_memory(bytes).map_err(map_image_error)?;
    Ok(to_decoded(img))
}

/// Decode an image file from disk into normalized RGBA.
pub fn decode_path<P: AsRef<Path>>(path: P) -> Result<DecodedImage> {
    let path = path.as_ref();
    if is_svg_path(path) {
        return decode_svg_bytes(&read_file(path)?, None);
    }
    if is_pdf_path(path) {
        return decode_pdf_page0(&PdfDocument::load(path)?);
    }
    let img = open_reader(path)?.decode().map_err(map_image_error)?;
    Ok(to_decoded(img))
}

/// Rasterize page `page_index` (0-based) of a PDF file at an explicit pixel size.
///
/// The viewer's entry point for both the PDF's initial crisp-size render and page-turn
/// navigation (`ArrowUp`/`ArrowDown` on a multi-page PDF — see D13 in the decision log). Unlike
/// [`decode_svg_at_size`], this always takes an explicit page index: every other PDF decode path
/// in this module (`decode_bytes`, `decode_path`, `decode_thumbnail`, `read_meta_path`) hardcodes
/// page 0 internally. Returns [`Error::UnsupportedFormat`] if `path` isn't PDF.
pub fn decode_pdf_page<P: AsRef<Path>>(
    path: P,
    page_index: usize,
    w: u32,
    h: u32,
) -> Result<DecodedImage> {
    let path = path.as_ref();
    if !is_pdf_path(path) {
        return Err(Error::UnsupportedFormat);
    }
    PdfDocument::load(path)?.render_page(page_index, w.max(1), h.max(1))
}

/// Rasterize page 0 of `doc` at its own intrinsic size — the "just open this file" default every
/// generic decode entry point in this module uses for PDF.
fn decode_pdf_page0(doc: &PdfDocument) -> Result<DecodedImage> {
    let (w, h) = doc.page_size(0)?;
    doc.render_page(0, w.round().max(1.0) as u32, h.round().max(1.0) as u32)
}

/// Rasterize an SVG file at an explicit pixel size, ignoring its intrinsic size.
///
/// This is the entry point the viewer uses for the crisp re-raster on zoom/fit/window-resize
/// settle (see D11 in the decision log): the GPU scales the last raster during the gesture itself
/// (free), and once it settles, the viewer calls this to swap in a sharp texture at the new
/// effective on-screen resolution. Returns [`Error::UnsupportedFormat`] if `path` isn't SVG.
pub fn decode_svg_at_size<P: AsRef<Path>>(path: P, w: u32, h: u32) -> Result<DecodedImage> {
    let path = path.as_ref();
    if !is_svg_path(path) {
        return Err(Error::UnsupportedFormat);
    }
    decode_svg_bytes(&read_file(path)?, Some((w.max(1), h.max(1))))
}

/// Decode and resize to a thumbnail that fits within `max_w × max_h`, preserving aspect ratio.
///
/// Uses `thumbnail()` from the `image` crate (nearest/triangle, fast for preview generation).
/// The returned image may be smaller than the requested size if the source is already smaller.
///
/// SVG is the one exception: unlike raster, a vector has no native resolution to cap at, so its
/// thumbnail is rasterized directly at the box size (upscaling included) — crisp, not blurry.
pub fn decode_thumbnail<P: AsRef<Path>>(path: P, max_w: u32, max_h: u32) -> Result<DecodedImage> {
    let path = path.as_ref();
    if is_svg_path(path) {
        let doc = SvgDocument::from_bytes(&read_file(path)?)?;
        let (w, h) = doc.size();
        let (tw, th) = fit_within(w, h, max_w.max(1), max_h.max(1));
        return doc.render_fit(tw, th);
    }
    if is_pdf_path(path) {
        // One thumbnail per PDF *file* (page 0), not one per page — same one-tile-per-file
        // convention as every other format in the grid (see D13 in the decision log).
        let doc = PdfDocument::load(path)?;
        let (w, h) = doc.page_size(0)?;
        let (tw, th) = fit_within(w, h, max_w.max(1), max_h.max(1));
        return doc.render_page(0, tw, th);
    }
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

    if is_svg_path(path) {
        let (w, h) = SvgDocument::from_bytes(&read_file(path)?)?.size();
        return Ok(ImageMeta {
            width: w.round().max(1.0) as u32,
            height: h.round().max(1.0) as u32,
            format: SourceFormat::Svg,
            file_size,
            page_count: None,
        });
    }

    if is_pdf_path(path) {
        let doc = PdfDocument::load(path)?;
        let (w, h) = doc.page_size(0)?;
        return Ok(ImageMeta {
            width: w.round().max(1.0) as u32,
            height: h.round().max(1.0) as u32,
            format: SourceFormat::Pdf,
            file_size,
            page_count: Some(doc.page_count() as u32),
        });
    }

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
        page_count: None,
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

// ── SVG ──────────────────────────────────────────────────────────────────────────────────────
//
// SVG has no fixed-offset header (it's XML text), so there is no cheaper "read the header only"
// path the way raster formats have via `open_reader`'s lazy `ImageReader`: getting dimensions
// (`read_meta_path`) and rasterizing (`decode_svg_bytes`/`decode_thumbnail`) both require a full
// parse. SVGs are small text files in practice, so this is not a concern.

fn read_file(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// A font database with system fonts loaded, shared across every SVG parse in this process.
/// Loading system fonts is a one-time scan (tens of milliseconds); doing it per-file would be
/// wasteful when navigating a folder full of SVGs.
fn svg_fontdb() -> Arc<usvg::fontdb::Database> {
    static DB: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = usvg::fontdb::Database::new();
        db.load_system_fonts();
        Arc::new(db)
    })
    .clone()
}

fn parse_svg(bytes: &[u8]) -> Result<usvg::Tree> {
    let opt = usvg::Options {
        fontdb: svg_fontdb(),
        ..Default::default()
    };
    usvg::Tree::from_data(bytes, &opt).map_err(|e| Error::Decode(format!("SVG parse failed: {e}")))
}

/// Fit `(iw, ih)` within `(max_w, max_h)`, preserving aspect ratio. Unlike raster thumbnailing,
/// this scales *up* as well as down — see `decode_thumbnail`'s doc comment for why. Public so
/// callers outside this crate that render at an intrinsic-aspect-preserving size (the viewer's PDF
/// page-turn render, which must not stretch a page to the window's aspect ratio) can reuse it
/// instead of re-deriving the same fit math.
pub fn fit_within(iw: f32, ih: f32, max_w: u32, max_h: u32) -> (u32, u32) {
    let scale = (max_w as f32 / iw).min(max_h as f32 / ih);
    (
        (iw * scale).round().max(1.0) as u32,
        (ih * scale).round().max(1.0) as u32,
    )
}

/// Copy a rendered tiny-skia pixmap into a normalized `DecodedImage`.
///
/// tiny-skia's pixmap is premultiplied alpha; the rest of the pipeline (wgpu texture upload,
/// `image`-crate encoding) expects straight alpha, same as every other decoder here.
fn pixmap_to_decoded(pixmap: &tiny_skia::Pixmap) -> DecodedImage {
    let (w, h) = (pixmap.width(), pixmap.height());
    let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
    for px in pixmap.pixels() {
        let c = px.demultiply();
        rgba.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    DecodedImage {
        width: w,
        height: h,
        rgba,
    }
}

/// A parsed SVG kept in memory so it can be rasterized many times without re-parsing — the whole
/// image (`render_fit`) or an arbitrary sub-rectangle at an explicit output resolution
/// (`render_region`). The latter is what the viewer's tiled deep-zoom renderer uses: it renders
/// only the visible region at screen resolution instead of capping the whole image at the GPU's
/// max texture size. `usvg::Tree` is `Send + Sync` (all-`Arc` internally), so an
/// `Arc<SvgDocument>` can be handed to a background rasterization thread.
pub struct SvgDocument {
    tree: usvg::Tree,
    width: f32,
    height: f32,
}

impl SvgDocument {
    /// Parse an SVG from in-memory bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let tree = parse_svg(bytes)?;
        let size = tree.size();
        Ok(SvgDocument {
            width: size.width(),
            height: size.height(),
            tree,
        })
    }

    /// Parse an SVG file from disk.
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::from_bytes(&read_file(path.as_ref())?)
    }

    /// Intrinsic size in SVG user units (`usvg` applies the spec 100×100 fallback when neither
    /// width/height nor viewBox are set).
    pub fn size(&self) -> (f32, f32) {
        (self.width, self.height)
    }

    /// Rasterize the whole image into an `out_w × out_h` buffer.
    pub fn render_fit(&self, out_w: u32, out_h: u32) -> Result<DecodedImage> {
        let (out_w, out_h) = (out_w.max(1), out_h.max(1));
        let mut pixmap = tiny_skia::Pixmap::new(out_w, out_h)
            .ok_or_else(|| Error::Decode("invalid SVG raster target size".to_string()))?;
        let scale_x = out_w as f32 / self.width;
        let scale_y = out_h as f32 / self.height;
        resvg::render(
            &self.tree,
            tiny_skia::Transform::from_scale(scale_x, scale_y),
            &mut pixmap.as_mut(),
        );
        Ok(pixmap_to_decoded(&pixmap))
    }

    /// Rasterize the image-space rectangle `(rx, ry, rw, rh)` (SVG user units) into an
    /// `out_w × out_h` buffer. The region origin maps to the buffer's top-left; resvg clips
    /// everything outside the pixmap. Used for tiled deep-zoom rendering.
    pub fn render_region(
        &self,
        rx: f32,
        ry: f32,
        rw: f32,
        rh: f32,
        out_w: u32,
        out_h: u32,
    ) -> Result<DecodedImage> {
        let (out_w, out_h) = (out_w.max(1), out_h.max(1));
        let (rw, rh) = (rw.max(f32::EPSILON), rh.max(f32::EPSILON));
        let mut pixmap = tiny_skia::Pixmap::new(out_w, out_h)
            .ok_or_else(|| Error::Decode("invalid SVG raster target size".to_string()))?;
        let sx = out_w as f32 / rw;
        let sy = out_h as f32 / rh;
        // Scale region→output, then shift so image point (rx, ry) lands at pixmap (0, 0):
        // p_out = p_img * (sx, sy) + (-rx*sx, -ry*sy).
        let transform = tiny_skia::Transform::from_scale(sx, sy).post_translate(-rx * sx, -ry * sy);
        resvg::render(&self.tree, transform, &mut pixmap.as_mut());
        Ok(pixmap_to_decoded(&pixmap))
    }
}

fn decode_svg_bytes(bytes: &[u8], target: Option<(u32, u32)>) -> Result<DecodedImage> {
    let doc = SvgDocument::from_bytes(bytes)?;
    let (w, h) = target.unwrap_or((
        doc.width.round().max(1.0) as u32,
        doc.height.round().max(1.0) as u32,
    ));
    doc.render_fit(w, h)
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

    // ── SVG ──────────────────────────────────────────────────────────────────────────────────

    const SAMPLE_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="6" viewBox="0 0 10 6">
<rect width="10" height="6" fill="#3366ff"/>
</svg>"##;

    fn temp_svg(name: &str, content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("glanvu-decode-svg-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.svg");
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn svg_decode_bytes_uses_intrinsic_size() {
        let img = decode_bytes(SAMPLE_SVG.as_bytes()).unwrap();
        assert_eq!((img.width, img.height), (10, 6));
        assert_eq!(img.rgba.len(), 10 * 6 * 4);
    }

    #[test]
    fn svg_detect_format_by_content() {
        assert_eq!(
            detect_format(SAMPLE_SVG.as_bytes()),
            Some(SourceFormat::Svg)
        );
    }

    #[test]
    fn svg_decode_path_and_meta() {
        let path = temp_svg("path-and-meta", SAMPLE_SVG);

        let img = decode_path(&path).unwrap();
        assert_eq!((img.width, img.height), (10, 6));

        let meta = read_meta_path(&path).unwrap();
        assert_eq!(meta.format, SourceFormat::Svg);
        assert_eq!((meta.width, meta.height), (10, 6));

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn svg_decode_at_explicit_target_size_ignores_intrinsic() {
        let path = temp_svg("target-size", SAMPLE_SVG);

        let img = decode_svg_at_size(&path, 100, 200).unwrap();
        assert_eq!((img.width, img.height), (100, 200));
        assert_eq!(img.rgba.len(), 100 * 200 * 4);

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn svg_decode_at_size_rejects_non_svg_path() {
        let dir = std::env::temp_dir().join("glanvu-decode-svg-test-rejects-non-svg");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("not-svg.png");
        sample().save(&path).unwrap();

        let err = decode_svg_at_size(&path, 50, 50).unwrap_err();
        assert!(matches!(err, Error::UnsupportedFormat));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn svg_thumbnail_upscales_small_intrinsic_size_to_fit_box() {
        // A 10x6 SVG thumbnailed into a much larger box should be rasterized *up* to fit the box
        // (unlike raster thumbnails, which never upscale — see decode_thumbnail's doc comment).
        let path = temp_svg("thumb-upscale", SAMPLE_SVG);

        let thumb = decode_thumbnail(&path, 200, 200).unwrap();
        // 10x6 fit within 200x200 -> scale by 200/10 = 20 -> 200x120 (aspect preserved).
        assert_eq!((thumb.width, thumb.height), (200, 120));

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn svg_malformed_content_errors_not_panic() {
        let err = decode_bytes(b"<svg><rect></svg").unwrap_err();
        assert!(matches!(err, Error::Decode(_)));
    }

    #[test]
    fn svg_default_size_when_no_width_height_or_viewbox() {
        // Per the SVG spec (and usvg's Options::default_size), this falls back to 100x100.
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"></svg>"#;
        let img = decode_bytes(svg.as_bytes()).unwrap();
        assert_eq!((img.width, img.height), (100, 100));
    }

    // ── SvgDocument (tiled deep-zoom rendering) ────────────────────────────────────────────────

    /// 4-quadrant SVG (100×100): TL red, TR green, BL blue, BR yellow — distinct so region
    /// rendering and stitching can be verified by sampling.
    const QUADRANT_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100" viewBox="0 0 100 100">
<rect x="0" y="0" width="50" height="50" fill="#ff0000"/>
<rect x="50" y="0" width="50" height="50" fill="#00ff00"/>
<rect x="0" y="50" width="50" height="50" fill="#0000ff"/>
<rect x="50" y="50" width="50" height="50" fill="#ffff00"/>
</svg>"##;

    fn px(img: &DecodedImage, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * img.width + x) * 4) as usize;
        [
            img.rgba[i],
            img.rgba[i + 1],
            img.rgba[i + 2],
            img.rgba[i + 3],
        ]
    }

    #[test]
    fn svg_document_is_send_and_sync() {
        // The tiled renderer shares an Arc<SvgDocument> with a background rasterization thread.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SvgDocument>();
    }

    #[test]
    fn svg_document_size_matches_intrinsic() {
        let doc = SvgDocument::from_bytes(QUADRANT_SVG.as_bytes()).unwrap();
        assert_eq!(doc.size(), (100.0, 100.0));
    }

    #[test]
    fn render_region_dims_and_content() {
        let doc = SvgDocument::from_bytes(QUADRANT_SVG.as_bytes()).unwrap();
        // Render just the top-right quadrant (green) at 2× resolution.
        let img = doc.render_region(50.0, 0.0, 50.0, 50.0, 100, 100).unwrap();
        assert_eq!((img.width, img.height), (100, 100));
        // Center of the tile should be solidly green.
        assert_eq!(px(&img, 50, 50), [0, 255, 0, 255]);
    }

    #[test]
    fn render_region_quadrants_stitch_to_render_fit() {
        let doc = SvgDocument::from_bytes(QUADRANT_SVG.as_bytes()).unwrap();
        // Full render at 100×100.
        let full = doc.render_fit(100, 100).unwrap();
        // Same coverage via four 50×50 region renders placed into a 100×100 buffer.
        let mut stitched = vec![0u8; 100 * 100 * 4];
        for (qx, qy) in [(0u32, 0u32), (50, 0), (0, 50), (50, 50)] {
            let tile = doc
                .render_region(qx as f32, qy as f32, 50.0, 50.0, 50, 50)
                .unwrap();
            for ty in 0..50u32 {
                for tx in 0..50u32 {
                    let src = (((ty * 50) + tx) * 4) as usize;
                    let dst = ((((qy + ty) * 100) + (qx + tx)) * 4) as usize;
                    stitched[dst..dst + 4].copy_from_slice(&tile.rgba[src..src + 4]);
                }
            }
        }
        // Compare interiors of each quadrant (avoid the 1px anti-aliased seam at the boundary).
        for (sx, sy) in [(25u32, 25u32), (75, 25), (25, 75), (75, 75)] {
            assert_eq!(
                px(&full, sx, sy),
                {
                    let i = ((sy * 100 + sx) * 4) as usize;
                    [
                        stitched[i],
                        stitched[i + 1],
                        stitched[i + 2],
                        stitched[i + 3],
                    ]
                },
                "quadrant sample at ({sx},{sy}) differs between render_fit and stitched tiles"
            );
        }
    }

    #[test]
    fn render_region_degenerate_size_does_not_panic() {
        let doc = SvgDocument::from_bytes(QUADRANT_SVG.as_bytes()).unwrap();
        // Zero region size and zero output size are clamped, not panics.
        let a = doc.render_region(0.0, 0.0, 0.0, 0.0, 0, 0).unwrap();
        assert_eq!((a.width, a.height), (1, 1));
        // A region past the image edge just renders transparent where there's no content.
        let b = doc.render_region(90.0, 90.0, 40.0, 40.0, 32, 32).unwrap();
        assert_eq!((b.width, b.height), (32, 32));
    }

    // ── PDF ──────────────────────────────────────────────────────────────────────────────────
    //
    // These tests need a real PDFium library (see D13 in the decision log): it's fetched
    // separately per platform, not bundled with `cargo test`, so a plain dev/CI environment
    // won't have one. Every test below skips (with a message, not a failure) when
    // `Error::PdfLibraryMissing` comes back, and only asserts real content once a library is
    // actually bound — run with `GLANVU_PDFIUM_LIB` pointed at a local PDFium checkout to
    // exercise the full assertions.

    /// Build a minimal, byte-accurate multi-page PDF in memory (uncompressed, no content streams
    /// — a page with no `/Contents` is legal PDF and renders blank, ISO 32000-1 §7.7.3.3). Byte
    /// offsets are computed programmatically rather than hand-typed, so the cross-reference table
    /// is always correct.
    fn minimal_pdf(pages: &[(f32, f32)]) -> Vec<u8> {
        let mut objs: Vec<String> = vec!["<< /Type /Catalog /Pages 2 0 R >>".to_string()];
        let kids: String = (0..pages.len())
            .map(|i| format!("{} 0 R", 3 + i))
            .collect::<Vec<_>>()
            .join(" ");
        objs.push(format!(
            "<< /Type /Pages /Kids [{kids}] /Count {} >>",
            pages.len()
        ));
        for (w, h) in pages {
            objs.push(format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] /Resources << >> >>"
            ));
        }

        let mut buf = Vec::new();
        buf.extend_from_slice(b"%PDF-1.4\n");
        let mut offsets = vec![0usize]; // object 0 is the free-list head, never referenced.
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

    fn temp_pdf(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("glanvu-decode-pdf-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.pdf");
        std::fs::write(&path, bytes).unwrap();
        path
    }

    /// Parse `bytes` as a PDF, or skip the calling test (with a message) if no PDFium library is
    /// bound in this environment. Panics on any other error, since that would be a real bug.
    fn try_pdf_document(bytes: &[u8]) -> Option<PdfDocument> {
        match PdfDocument::from_bytes(bytes) {
            Ok(doc) => Some(doc),
            Err(Error::PdfLibraryMissing(msg)) => {
                eprintln!("skipping PDF test: {msg}");
                None
            }
            Err(e) => panic!("unexpected error parsing test PDF: {e}"),
        }
    }

    #[test]
    fn pdf_extension_sniff_and_detect() {
        let bytes = minimal_pdf(&[(200.0, 100.0)]);
        assert!(sniff_pdf(&bytes));
        assert_eq!(detect_format(&bytes), Some(SourceFormat::Pdf));
    }

    #[test]
    fn pdf_document_page_count_and_size() {
        let bytes = minimal_pdf(&[(200.0, 100.0), (300.0, 150.0), (50.0, 75.0)]);
        let Some(doc) = try_pdf_document(&bytes) else {
            return;
        };
        assert_eq!(doc.page_count(), 3);
        let (w, h) = doc.page_size(1).unwrap();
        assert!((w - 300.0).abs() < 0.5 && (h - 150.0).abs() < 0.5);
    }

    #[test]
    fn pdf_render_page_dims() {
        let bytes = minimal_pdf(&[(200.0, 100.0)]);
        let Some(doc) = try_pdf_document(&bytes) else {
            return;
        };
        let img = doc.render_page(0, 64, 32).unwrap();
        assert_eq!((img.width, img.height), (64, 32));
        assert_eq!(img.rgba.len(), 64 * 32 * 4);
    }

    #[test]
    fn pdf_render_page_out_of_range_errors_not_panics() {
        let bytes = minimal_pdf(&[(200.0, 100.0)]);
        let Some(doc) = try_pdf_document(&bytes) else {
            return;
        };
        assert!(doc.render_page(5, 10, 10).is_err());
        assert!(doc.page_size(5).is_err());
    }

    #[test]
    fn pdf_decode_path_and_meta() {
        let bytes = minimal_pdf(&[(200.0, 100.0), (200.0, 100.0), (200.0, 100.0)]);
        if try_pdf_document(&bytes).is_none() {
            return;
        }
        let path = temp_pdf("path-and-meta", &bytes);

        let img = decode_path(&path).unwrap();
        assert_eq!((img.width, img.height), (200, 100));

        let meta = read_meta_path(&path).unwrap();
        assert_eq!(meta.format, SourceFormat::Pdf);
        assert_eq!((meta.width, meta.height), (200, 100));
        assert_eq!(meta.page_count, Some(3));

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn pdf_decode_thumbnail_fits_box() {
        let bytes = minimal_pdf(&[(200.0, 100.0)]);
        if try_pdf_document(&bytes).is_none() {
            return;
        }
        let path = temp_pdf("thumb", &bytes);

        // 200x100 fit within 50x50 -> scale by 50/200 = 0.25 -> 50x25 (aspect preserved).
        let thumb = decode_thumbnail(&path, 50, 50).unwrap();
        assert_eq!((thumb.width, thumb.height), (50, 25));

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn pdf_decode_page_turns_pages() {
        let bytes = minimal_pdf(&[(200.0, 100.0), (300.0, 150.0)]);
        if try_pdf_document(&bytes).is_none() {
            return;
        }
        let path = temp_pdf("page-turn", &bytes);

        let page0 = decode_pdf_page(&path, 0, 40, 20).unwrap();
        assert_eq!((page0.width, page0.height), (40, 20));
        let page1 = decode_pdf_page(&path, 1, 40, 20).unwrap();
        assert_eq!((page1.width, page1.height), (40, 20));

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn pdf_decode_page_rejects_non_pdf_path() {
        let dir = std::env::temp_dir().join("glanvu-decode-pdf-test-rejects-non-pdf");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("not-pdf.png");
        sample().save(&path).unwrap();

        let err = decode_pdf_page(&path, 0, 50, 50).unwrap_err();
        assert!(matches!(err, Error::UnsupportedFormat));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
