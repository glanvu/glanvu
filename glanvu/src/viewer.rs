// SPDX-License-Identifier: Apache-2.0

//! Phase 1 viewer: winit window + wgpu textured-quad pipeline + glyphon path overlay.
//!
//! Folder navigation and prefetch live in `nav::FolderNav`; this module owns only the GPU state
//! (`Gpu`), the view/transform state (`ViewState`), and the winit event loop (`App`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytemuck::{Pod, Zeroable};
use glam::{Mat4, Vec3};
use glyphon::{
    Attrs, Buffer as TextBuffer, Cache as GlyphCache, Color, FontSystem, Metrics, Resolution,
    Shaping, SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport, Weight, Wrap,
};
use wgpu::util::DeviceExt;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

use arboard::Clipboard;
use glanvu_core::DecodedImage;

use glanvu_viewer_core::explorer::{
    ExplorerState, FONT as EXPLORER_FONT, LINE_H as EXPLORER_LINE_H, PANEL_W,
};
use glanvu_viewer_core::grid::{GridState, CELL_H, CELL_W, GAP, MARGIN, SEL_OUTSET};
use glanvu_viewer_core::nav::{locate, FolderNav};
use glanvu_viewer_core::thumb::{ThumbnailCache, THUMB_H, THUMB_W};

/// Maximum tile uniform buffers for the grid renderer (pool pre-allocated at GPU init).
/// Each visible tile needs up to 3 draw slots (bg + selection ring + thumbnail).
const TILE_POOL: usize = 384;

/// Small pool of MVP uniform buffers for compositing the SVG deep-zoom viewport quad (a dedicated
/// pool so it never clashes with the grid/explorer's `tile_bufs`). Only one is used at a time.
const SVG_TILE_POOL: usize = 4;

/// How long the path overlay stays visible after an action.
const OVERLAY_DURATION: Duration = Duration::from_millis(2000);

/// Debounce before re-rasterizing an SVG at its new effective on-screen resolution, after
/// zoom/fit/window-size settles. The GPU keeps scaling the last raster during the gesture itself
/// (free); this just controls how long "at rest" means before paying for a sharp re-raster. See
/// D11 in the decision log.
const SVG_RERENDER_DEBOUNCE: Duration = Duration::from_millis(200);

/// A request to re-rasterize an SVG: `(generation, path, target_w, target_h)`. `generation` lets
/// the receiver discard results from a superseded request (the user zoomed again before the
/// previous re-raster finished) instead of applying a stale texture.
type SvgRerenderRequest = (u64, PathBuf, u32, u32);
/// The background worker's reply: `(generation, path, decode result)`.
type SvgRerenderResult = (u64, PathBuf, glanvu_core::Result<DecodedImage>);

/// How often to poll for a completed background SVG re-raster while one is in flight (mirrors
/// the grid-thumbnail-polling interval below).
const SVG_RERENDER_POLL: Duration = Duration::from_millis(30);

/// A single-slot "latest request wins" mailbox for the SVG re-raster worker.
///
/// This is deliberately *not* an `mpsc` queue: a queue would keep every superseded request (each
/// zoom/fit settle while a re-raster is still running) and the single worker thread would have to
/// churn through all of them in order before it could even look at the newest one — a backlog
/// that stays on CPU and competes with, say, opening the next image, even though every stale job
/// but the last is wasted work. With a single slot, posting a new request overwrites whatever
/// hasn't been picked up yet, so the worker only ever computes the most recent one.
struct SvgRerenderMailbox {
    slot: std::sync::Mutex<Option<SvgRerenderRequest>>,
    cv: std::sync::Condvar,
}

impl SvgRerenderMailbox {
    fn new() -> Self {
        SvgRerenderMailbox {
            slot: std::sync::Mutex::new(None),
            cv: std::sync::Condvar::new(),
        }
    }

    /// Replace the pending request (dropping whatever hasn't been picked up yet) and wake the
    /// worker.
    fn post(&self, req: SvgRerenderRequest) {
        if let Ok(mut slot) = self.slot.lock() {
            *slot = Some(req);
        }
        self.cv.notify_one();
    }

    /// Block until a request is posted, then take it (clearing the slot). Returns `None` only if
    /// the mutex is poisoned (a panic elsewhere while holding it) — the worker exits cleanly.
    fn take_blocking(&self) -> Option<SvgRerenderRequest> {
        let mut slot = self.slot.lock().ok()?;
        while slot.is_none() {
            slot = self.cv.wait(slot).ok()?;
        }
        slot.take()
    }
}

/// Spawn the SVG re-raster background worker. Returns the mailbox to post requests to and the
/// reply channel to poll for results — decoding runs off the UI thread so a large/complex SVG
/// can't stall zoom or redraw (mirrors `FolderNav::new`'s prefetch worker in `nav.rs`, which
/// keeps decode work off the UI thread the same way; that one *is* a plain FIFO queue because
/// prefetch requests are cheap and never superseded the way a rapid-fire zoom is).
fn spawn_svg_rerender_worker() -> (Arc<SvgRerenderMailbox>, Receiver<SvgRerenderResult>) {
    let mailbox = Arc::new(SvgRerenderMailbox::new());
    let worker_mailbox = Arc::clone(&mailbox);
    let (res_tx, res_rx) = std::sync::mpsc::channel::<SvgRerenderResult>();
    std::thread::spawn(move || {
        while let Some((gen, path, w, h)) = worker_mailbox.take_blocking() {
            let result = glanvu_core::decode_svg_at_size(&path, w, h);
            if res_tx.send((gen, path, result)).is_err() {
                break;
            }
        }
    });
    (mailbox, res_rx)
}

// ── SVG deep-zoom tile worker ──────────────────────────────────────────────────────────────────
//
// Unlike the whole-image base re-raster (latest-wins mailbox above), tiling needs MANY tiles per
// zoom epoch rendered off the UI thread, so this is a FIFO queue of jobs. Each job carries an
// `Arc<SvgDocument>` (parsed once on load) so tiles render without re-parsing. Stale-epoch jobs are
// dropped on receipt; the queue is cleared when the epoch changes.

/// A tile render job: `(epoch, col, row)` key, image-space region, output pixel size, document.
struct TileJob {
    epoch: u64,
    col: i32,
    row: i32,
    region: (f32, f32, f32, f32),
    out: (u32, u32),
    doc: Arc<glanvu_core::SvgDocument>,
}

/// A rendered tile reply: `(epoch, col, row, result)`.
type TileResult = (u64, i32, i32, glanvu_core::Result<DecodedImage>);

/// FIFO job queue for the tile worker; `clear` drops all pending jobs when the epoch changes.
struct TileQueue {
    jobs: std::sync::Mutex<std::collections::VecDeque<TileJob>>,
    cv: std::sync::Condvar,
}

impl TileQueue {
    fn new() -> Self {
        TileQueue {
            jobs: std::sync::Mutex::new(std::collections::VecDeque::new()),
            cv: std::sync::Condvar::new(),
        }
    }

    fn push(&self, job: TileJob) {
        if let Ok(mut q) = self.jobs.lock() {
            q.push_back(job);
        }
        self.cv.notify_one();
    }

    /// Drop all pending jobs (called when the zoom epoch changes; in-flight jobs still reply but
    /// are discarded by epoch on receipt).
    fn clear(&self) {
        if let Ok(mut q) = self.jobs.lock() {
            q.clear();
        }
    }

    fn pop_blocking(&self) -> Option<TileJob> {
        let mut q = self.jobs.lock().ok()?;
        while q.is_empty() {
            q = self.cv.wait(q).ok()?;
        }
        q.pop_front()
    }
}

fn spawn_tile_worker() -> (Arc<TileQueue>, Receiver<TileResult>) {
    let queue = Arc::new(TileQueue::new());
    let worker_queue = Arc::clone(&queue);
    let (res_tx, res_rx) = std::sync::mpsc::channel::<TileResult>();
    std::thread::spawn(move || {
        while let Some(job) = worker_queue.pop_blocking() {
            let (rx, ry, rw, rh) = job.region;
            let result = job.doc.render_region(rx, ry, rw, rh, job.out.0, job.out.1);
            if res_tx.send((job.epoch, job.col, job.row, result)).is_err() {
                break;
            }
        }
    });
    (queue, res_rx)
}

/// Whether to print timing diagnostics (set `GLANVU_PERF=1`). Off by default so runs stay quiet.
fn perf_logging() -> bool {
    std::env::var_os("GLANVU_PERF").is_some()
}

// ---------------------------------------------------------------------------
// GPU shaders
// ---------------------------------------------------------------------------

const WATERMARK_BYTES: &[u8] = include_bytes!("../../assets/AppIcon.png");

/// Version label shown under the watermark in the empty state.
const VERSION_LABEL: &str = concat!("v", env!("CARGO_PKG_VERSION"));

const KOFI_URL: &str = "https://ko-fi.com/juanyque";
const GITHUB_SPONSORS_URL: &str = "https://github.com/sponsors/juanyque";
/// Donate line, rendered blue and centered. The two link labels are roughly balanced in width so
/// a click split at the line's horizontal center cleanly separates Ko-fi (left) from Sponsors (right).
const DONATE_LINE: &str = "\u{2665}  Ko-fi Support  \u{00b7}  GitHub Sponsors";
/// About overlay body (white). The donate line is rendered separately (blue, centered) below it.
const ABOUT_HEAD: &str = concat!(
    "Glanvu  v",
    env!("CARGO_PKG_VERSION"),
    "\n\nFast, keyboard-driven, cross-platform image viewer\nand converter.\n\nApache-2.0 License",
);

/// A clickable donate-link zone (physical px), computed during render so the click handler does no
/// geometry of its own. A click inside `[x0,x1] × [y0,y1]` opens Ko-fi if `cx < split_x`, else Sponsors.
#[derive(Clone, Copy)]
struct DonateHit {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    split_x: f32,
}

/// A minimal single-line text editor (used by the inline rename field). Operates on `char`s so
/// cursor moves and edits never split a UTF-8 boundary.
struct TextInput {
    chars: Vec<char>,
    cursor: usize, // 0..=chars.len()
}

impl TextInput {
    fn new(s: &str) -> Self {
        let chars: Vec<char> = s.chars().collect();
        let cursor = chars.len();
        Self { chars, cursor }
    }
    fn text(&self) -> String {
        self.chars.iter().collect()
    }
    fn insert(&mut self, c: char) {
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
    }
    fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            self.insert(c);
        }
    }
    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }
    fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }
    fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }
    fn right(&mut self) {
        if self.cursor < self.chars.len() {
            self.cursor += 1;
        }
    }
    fn home(&mut self) {
        self.cursor = 0;
    }
    fn end(&mut self) {
        self.cursor = self.chars.len();
    }
    /// The text with a caret marker inserted at the cursor, for display in the overlay.
    fn display_with_caret(&self) -> String {
        let mut s: String = self.chars[..self.cursor].iter().collect();
        s.push('|');
        s.extend(self.chars[self.cursor..].iter());
        s
    }
}

/// In-progress quick-open search ("find by name", `F` or `/`).
///
/// Two presentations share this state. In **single** view it drives a floating modal list of the
/// top matches. In **grid** view it drives a *live filter*: the grid shows only the matching
/// thumbnails (re-packed), with a search bar across the top and the cursor on the highlighted
/// match. The state is the same; only `limit` (list is capped, filter shows all) and `scroll_y`
/// (filter only) differ.
struct FindState {
    /// The query text editor (same inline editor as rename).
    input: TextInput,
    /// Ranked playlist indices matching the query (best-first), capped to `limit`.
    matches: Vec<usize>,
    /// Highlighted match: index into `matches`.
    sel: usize,
    /// Max matches to keep: `FIND_LIMIT` for the single-view list, `usize::MAX` for the grid filter.
    limit: usize,
    /// Vertical scroll offset for the grid filter (physical px). Unused in single view.
    scroll_y: f32,
}

/// Maximum number of find matches shown in the single-view modal list at once.
const FIND_LIMIT: usize = 8;

impl FindState {
    /// Re-rank `matches` against the current query over `paths`, keeping the highlighted match in
    /// bounds. Consumes and returns `self` so callers can swap it back in one move.
    fn recompute_for(mut self, paths: &[PathBuf]) -> Self {
        let names: Vec<&str> = paths.iter().map(|p| file_name_str(p)).collect();
        self.matches = glanvu_viewer_core::find::search(&self.input.text(), &names, self.limit);
        if self.sel >= self.matches.len() {
            self.sel = self.matches.len().saturating_sub(1);
        }
        self
    }

    /// The playlist index of the highlighted match, if any.
    fn current(&self) -> Option<usize> {
        self.matches.get(self.sel).copied()
    }
}

/// Height (physical px) of the grid-filter search bar for a given DPI scale. Shared by the renderer
/// (`layout_find_bar`) and the scroll math (`find_scroll_to_cursor`) so they stay in agreement.
fn find_bar_height(scale: f32) -> f32 {
    let font = (16.0 * scale).clamp(14.0, 32.0);
    font * 1.4 + 2.0 * (8.0 * scale)
}

/// In-progress grid drag (left button held). Decides between a click (no movement) and a
/// rubber-band marquee (moved past the threshold).
struct GridDrag {
    /// Press position in screen coords.
    start: (f32, f32),
    /// Selection to union the marquee with (the prior selection for Ctrl/Cmd-additive drags).
    base: HashSet<usize>,
    /// Ctrl/Cmd held at press → additive selection.
    additive: bool,
    /// Shift held at press → range-click on release, no marquee.
    range: bool,
    /// Whether the pointer moved far enough to become a marquee.
    moved: bool,
}

const SHADER: &str = r#"
struct Uniforms { mvp: mat4x4<f32> };
@group(0) @binding(0) var<uniform> u: Uniforms;
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@location(0) pos: vec2<f32>, @location(1) uv: vec2<f32>) -> VsOut {
    var out: VsOut;
    out.clip = u.mvp * vec4<f32>(pos, 0.0, 1.0);
    out.uv = uv;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, in.uv);
}
"#;

/// Downsampling blit used to build each mipmap level from the previous one. Draws a single
/// full-screen triangle (no vertex buffer) and copies the bound source texture into the target
/// mip via the linear sampler — a 2× box-ish reduction per level.
const MIP_SHADER: &str = r#"
@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
    var out: VsOut;
    let x = f32((i << 1u) & 2u);
    let y = f32(i & 2u);
    out.uv = vec2<f32>(x, y);
    out.clip = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(src, samp, in.uv);
}
"#;

// ---------------------------------------------------------------------------
// Vertex / uniform types
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vertex {
    pos: [f32; 2],
    uv: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Uniforms {
    mvp: [[f32; 4]; 4],
}

const VERTICES: [Vertex; 4] = [
    Vertex {
        pos: [-0.5, 0.5],
        uv: [0.0, 0.0],
    },
    Vertex {
        pos: [-0.5, -0.5],
        uv: [0.0, 1.0],
    },
    Vertex {
        pos: [0.5, -0.5],
        uv: [1.0, 1.0],
    },
    Vertex {
        pos: [0.5, 0.5],
        uv: [1.0, 0.0],
    },
];
const INDICES: [u16; 6] = [0, 1, 2, 0, 2, 3];

// ---------------------------------------------------------------------------
// Transform helpers
// ---------------------------------------------------------------------------

/// Pan/zoom/rotation state, independent of the GPU.
struct ViewState {
    fit: bool,
    zoom: f32,
    pan: (f32, f32),
    quarter_turns: u32,
}

impl ViewState {
    fn fit() -> Self {
        ViewState {
            fit: true,
            zoom: 1.0,
            pan: (0.0, 0.0),
            quarter_turns: 0,
        }
    }
}

/// Effective on-screen scale in screen-px per image-px: the fit-to-window base × user `zoom`.
/// Shared by `mvp`, the visible-region math, and the SVG tile grid so they stay consistent.
fn image_scale(img: (u32, u32), win: (f32, f32), st: &ViewState) -> f32 {
    let (iw, ih) = (img.0.max(1) as f32, img.1.max(1) as f32);
    let (win_w, win_h) = (win.0.max(1.0), win.1.max(1.0));
    // Rotation by an odd number of quarter-turns swaps which image axis maps to which window axis.
    let (bw, bh) = if st.quarter_turns % 2 == 1 {
        (ih, iw)
    } else {
        (iw, ih)
    };
    let base = if st.fit {
        (win_w / bw).min(win_h / bh)
    } else {
        1.0
    };
    base * st.zoom
}

/// The fit-to-window scale (screen-px per image-px when the whole image just fills the window),
/// independent of `zoom`/`fit`. This is the resolution the whole-image base layer is rendered at,
/// and the reference for deciding when to switch to viewport tiles (`image_scale` beyond it means
/// the base can't provide screen resolution for the zoomed-in view).
fn fit_scale(img: (u32, u32), win: (f32, f32), quarter_turns: u32) -> f32 {
    let (iw, ih) = (img.0.max(1) as f32, img.1.max(1) as f32);
    let (win_w, win_h) = (win.0.max(1.0), win.1.max(1.0));
    let (bw, bh) = if quarter_turns % 2 == 1 {
        (ih, iw)
    } else {
        (iw, ih)
    };
    (win_w / bw).min(win_h / bh)
}

/// MVP matrix mapping the unit quad to clip space (image transform, y-up center origin).
fn mvp(img: (u32, u32), win: (f32, f32), st: &ViewState) -> [[f32; 4]; 4] {
    let (iw, ih) = (img.0.max(1) as f32, img.1.max(1) as f32);
    let (win_w, win_h) = (win.0.max(1.0), win.1.max(1.0));
    let scale = image_scale(img, win, st);
    let model = Mat4::from_scale(Vec3::new(iw * scale, ih * scale, 1.0));
    let rot = Mat4::from_rotation_z(st.quarter_turns as f32 * std::f32::consts::FRAC_PI_2);
    let trans = Mat4::from_translation(Vec3::new(st.pan.0, st.pan.1, 0.0));
    let proj = Mat4::from_scale(Vec3::new(2.0 / win_w, 2.0 / win_h, 1.0));
    (proj * trans * rot * model).to_cols_array_2d()
}

/// Fraction the visible rect is padded by when choosing the SVG viewport render region, so small
/// pans stay within the already-rendered texture (free) instead of triggering a re-render.
const SVG_VP_PAD: f32 = 0.20;

/// Cap on the SVG viewport render scale (output px per image px). resvg rasterizes the whole tree
/// — every filter/blur — at the target scale, and blur cost grows ~scale⁴, so an uncapped deep
/// zoom on a filter-heavy SVG takes tens of seconds for one render. Above this cap we render at
/// the cap and let the GPU magnify (blurs upscale smoothly; only ultra-fine edges soften), which
/// keeps a render to ~1s. Below it (up to ~`cap/fit`× zoom) rendering is at true screen resolution.
const SVG_MAX_RENDER_SCALE: f32 = 10.0;

/// Expand `rect` (image px) by `pad` on each side, clamped to the image bounds `[0,iw]×[0,ih]`.
fn pad_region(rect: (f32, f32, f32, f32), pad: f32, img: (u32, u32)) -> (f32, f32, f32, f32) {
    let (iw, ih) = (img.0.max(1) as f32, img.1.max(1) as f32);
    let (x, y, w, h) = rect;
    let (px, py) = (w * pad, h * pad);
    let x0 = (x - px).max(0.0);
    let y0 = (y - py).max(0.0);
    let x1 = (x + w + px).min(iw);
    let y1 = (y + h + py).min(ih);
    (x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0))
}

/// Whether `outer` fully contains `inner` (both `(x, y, w, h)` in image px), with a tiny epsilon.
fn region_covers(outer: (f32, f32, f32, f32), inner: (f32, f32, f32, f32)) -> bool {
    let e = 0.5;
    outer.0 <= inner.0 + e
        && outer.1 <= inner.1 + e
        && outer.0 + outer.2 + e >= inner.0 + inner.2
        && outer.1 + outer.3 + e >= inner.1 + inner.3
}

/// The axis-aligned image-space rectangle `(x, y, w, h)` (image px) currently visible on screen.
///
/// Inverts the `mvp` transform for the four window corners and takes their bounding box; because
/// rotation is always a multiple of 90°, the visible region stays axis-aligned in image space, so
/// the bounding box is exact. Clamped to `[0, iw] × [0, ih]`.
fn visible_image_rect(img: (u32, u32), win: (f32, f32), st: &ViewState) -> (f32, f32, f32, f32) {
    let (iw, ih) = (img.0.max(1) as f32, img.1.max(1) as f32);
    let (win_w, win_h) = (win.0.max(1.0), win.1.max(1.0));
    let s = image_scale(img, win, st).max(f32::EPSILON);
    // Inverse rotation (undo `rot` from the forward chain).
    let inv = -(st.quarter_turns as f32 * std::f32::consts::FRAC_PI_2);
    let (ca, sa) = (inv.cos(), inv.sin());
    let corners = [
        (-win_w / 2.0, -win_h / 2.0),
        (win_w / 2.0, -win_h / 2.0),
        (win_w / 2.0, win_h / 2.0),
        (-win_w / 2.0, win_h / 2.0),
    ];
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for (cx, cy) in corners {
        // Undo translate (pan), then rotation, then the center/y-flip/scale of the model.
        let (mx, my) = (cx - st.pan.0, cy - st.pan.1);
        let (rx, ry) = (mx * ca - my * sa, mx * sa + my * ca);
        let fx = rx / s + iw / 2.0;
        let fy = ih / 2.0 - ry / s;
        x0 = x0.min(fx);
        x1 = x1.max(fx);
        y0 = y0.min(fy);
        y1 = y1.max(fy);
    }
    let x0 = x0.clamp(0.0, iw);
    let y0 = y0.clamp(0.0, ih);
    let x1 = x1.clamp(0.0, iw);
    let y1 = y1.clamp(0.0, ih);
    (x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0))
}

/// A composited SVG viewport render: its MVP and the cache key of its texture (in `svg_tile_tex`).
struct SvgTileDraw {
    mvp: [[f32; 4]; 4],
    key: (u64, i32, i32),
}

/// MVP for a single tile's quad, placing image-space sub-rect `tile` using the *same* view /
/// projection as `mvp` so tiles line up exactly on top of the whole-image base layer.
fn tile_mvp(img: (u32, u32), tile: (f32, f32, f32, f32), win: (f32, f32), st: &ViewState) -> [[f32; 4]; 4] {
    let (iw, ih) = (img.0.max(1) as f32, img.1.max(1) as f32);
    let (win_w, win_h) = (win.0.max(1.0), win.1.max(1.0));
    let s = image_scale(img, win, st);
    let (rx, ry, rw, rh) = tile;
    // Tile center in the model's center-origin, y-up pixel space.
    let cx = (rx + rw / 2.0 - iw / 2.0) * s;
    let cy = (ih / 2.0 - (ry + rh / 2.0)) * s;
    let model = Mat4::from_translation(Vec3::new(cx, cy, 0.0))
        * Mat4::from_scale(Vec3::new(rw * s, rh * s, 1.0));
    let rot = Mat4::from_rotation_z(st.quarter_turns as f32 * std::f32::consts::FRAC_PI_2);
    let trans = Mat4::from_translation(Vec3::new(st.pan.0, st.pan.1, 0.0));
    let proj = Mat4::from_scale(Vec3::new(2.0 / win_w, 2.0 / win_h, 1.0));
    (proj * trans * rot * model).to_cols_array_2d()
}

/// Ideal initial window size (logical px) for an image: capped to 1600x1000, scaled to 90%,
/// never smaller than 320x240. Used both when creating the window and when resizing to a newly
/// opened image (Open With / drop from the empty state).
fn ideal_window_size(iw: u32, ih: u32) -> (f32, f32) {
    let (iw, ih) = (iw.max(1) as f32, ih.max(1) as f32);
    let s = (1600.0 / iw).min(1000.0 / ih).min(1.0) * 0.9;
    ((iw * s).max(320.0), (ih * s).max(240.0))
}

/// MVP for a screen-space rectangle (top-left origin, y-down pixel coords).
fn rect_mvp(w: f32, h: f32, sx: f32, sy: f32, win_w: f32, win_h: f32) -> [[f32; 4]; 4] {
    let (win_w, win_h) = (win_w.max(1.0), win_h.max(1.0));
    let cx = sx + w / 2.0 - win_w / 2.0;
    let cy = win_h / 2.0 - (sy + h / 2.0);
    let proj = Mat4::from_scale(Vec3::new(2.0 / win_w, 2.0 / win_h, 1.0));
    (proj * Mat4::from_translation(Vec3::new(cx, cy, 0.0)) * Mat4::from_scale(Vec3::new(w, h, 1.0)))
        .to_cols_array_2d()
}

// ---------------------------------------------------------------------------
// GPU helpers
// ---------------------------------------------------------------------------

fn build_texture_bind(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    srgb: bool,
    image: &DecodedImage,
) -> wgpu::BindGroup {
    let format = if srgb {
        wgpu::TextureFormat::Rgba8UnormSrgb
    } else {
        wgpu::TextureFormat::Rgba8Unorm
    };
    let extent = wgpu::Extent3d {
        width: image.width.max(1),
        height: image.height.max(1),
        depth_or_array_layers: 1,
    };
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("glanvu texture"),
        size: extent,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &image.rgba,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(4 * image.width.max(1)),
            rows_per_image: Some(image.height.max(1)),
        },
        extent,
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("texture bind"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    })
}

/// Number of mipmap levels for a `w × h` texture: `floor(log2(max(w, h))) + 1`.
fn mip_level_count(w: u32, h: u32) -> u32 {
    32 - w.max(h).max(1).leading_zeros()
}

/// Like [`build_texture_bind`], but for the main image: builds a full mipmap chain so the image
/// stays crisp when minified to fit the window or zoomed out (the sampler is trilinear). Small
/// textures (≤1 px in either axis) get a single level and skip generation. `mip_pipeline` is the
/// downsampling blit pipeline created in `Gpu::new`.
fn build_image_texture_bind(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    mip_pipeline: &wgpu::RenderPipeline,
    srgb: bool,
    image: &DecodedImage,
) -> wgpu::BindGroup {
    let format = if srgb {
        wgpu::TextureFormat::Rgba8UnormSrgb
    } else {
        wgpu::TextureFormat::Rgba8Unorm
    };
    let (w, h) = (image.width.max(1), image.height.max(1));
    let levels = mip_level_count(w, h);
    let extent = wgpu::Extent3d {
        width: w,
        height: h,
        depth_or_array_layers: 1,
    };
    // RENDER_ATTACHMENT is needed so `generate_mipmaps` can render into levels 1..n.
    let usage = wgpu::TextureUsages::TEXTURE_BINDING
        | wgpu::TextureUsages::COPY_DST
        | wgpu::TextureUsages::RENDER_ATTACHMENT;
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("glanvu image texture"),
        size: extent,
        mip_level_count: levels,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage,
        view_formats: &[],
    });
    // Upload the full-resolution pixels into mip level 0.
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &image.rgba,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(4 * w),
            rows_per_image: Some(h),
        },
        extent,
    );
    if levels > 1 {
        generate_mipmaps(device, queue, mip_pipeline, layout, sampler, &texture, levels);
    }
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("image texture bind"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    })
}

/// Fill mip levels `1..levels` by rendering each one from the previous level with the
/// downsampling blit pipeline (a full-screen triangle sampled through the linear `sampler`).
fn generate_mipmaps(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    mip_pipeline: &wgpu::RenderPipeline,
    src_layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    texture: &wgpu::Texture,
    levels: u32,
) {
    let mut encoder =
        device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("mipmaps") });
    // Per-level views, each covering exactly one mip level.
    let views: Vec<wgpu::TextureView> = (0..levels)
        .map(|level| {
            texture.create_view(&wgpu::TextureViewDescriptor {
                base_mip_level: level,
                mip_level_count: Some(1),
                ..Default::default()
            })
        })
        .collect();
    for target in 1..levels as usize {
        let src_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("mip src bind"),
            layout: src_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&views[target - 1]),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
            ],
        });
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("mip pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &views[target],
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(mip_pipeline);
        pass.set_bind_group(0, &src_bind, &[]);
        pass.draw(0..3, 0..1);
    }
    queue.submit(std::iter::once(encoder.finish()));
}

fn make_uniform_buffer(device: &wgpu::Device) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("glanvu uniforms"),
        size: std::mem::size_of::<Uniforms>() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn make_uniform_bind(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    buf: &wgpu::Buffer,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("uniform bind"),
        layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: buf.as_entire_binding(),
        }],
    })
}

// ---------------------------------------------------------------------------
// Gpu struct
// ---------------------------------------------------------------------------

/// Computed positions (physical px) for the help overlay's title, two section columns, centered
/// footer block, and donate line. Produced by `layout_help`, consumed by the renderer.
struct HelpLayout {
    bx: f32,
    by: f32,
    bw: f32,
    bh: f32,
    title_x: f32,
    title_y: f32,
    col_top: f32,
    l_keys_x: f32,
    l_desc_x: f32,
    r_keys_x: f32,
    r_desc_x: f32,
    footer_top: f32,
    f_keys_x: f32,
    f_desc_x: f32,
    donate_top: f32,
}

struct Gpu {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    /// Blit pipeline that builds each mipmap level from the previous one (see `generate_mipmaps`).
    mip_pipeline: wgpu::RenderPipeline,
    texture_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniform_buf: wgpu::Buffer,
    uniform_bind: wgpu::BindGroup,
    texture_bind: wgpu::BindGroup,
    vertex_buf: wgpu::Buffer,
    index_buf: wgpu::Buffer,
    // Overlay: dark backing box + glyphon text.
    box_uniform_buf: wgpu::Buffer,
    box_uniform_bind: wgpu::BindGroup,
    box_texture_bind: wgpu::BindGroup,
    font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    text_renderer: TextRenderer,
    text_buffer: TextBuffer,
    overlay_text: String,
    overlay_line_h: f32,
    // Centered status overlay (slideshow on/off, etc.) — separate buf/text to avoid conflicts.
    status_uniform_buf: wgpu::Buffer,
    status_uniform_bind: wgpu::BindGroup,
    status_text_buf: TextBuffer,
    status_overlay_text: String,
    // Centered help overlay (two-column keyboard cheatsheet, toggled with H).
    // `help_keys_buf` holds the left column (keys + title); `help_desc_buf` the right column
    // (descriptions). Two buffers so the columns align by real font metrics, not by padding spaces.
    help_uniform_buf: wgpu::Buffer,
    help_uniform_bind: wgpu::BindGroup,
    // Help overlay = centered title + two side-by-side section columns (left/right) + a centered
    // footer block. Each column/footer is a keys buffer (rich text, accent-coloured headers) and a
    // description buffer.
    help_title_buf: TextBuffer,
    help_l_keys_buf: TextBuffer,
    help_l_desc_buf: TextBuffer,
    help_r_keys_buf: TextBuffer,
    help_r_desc_buf: TextBuffer,
    help_f_keys_buf: TextBuffer,
    help_f_desc_buf: TextBuffer,
    help_layout_scale: f32,
    help_line_h: f32,
    /// Line count of the taller of the two columns, and of the footer block (for box height +
    /// donate placement).
    help_col_lines: usize,
    help_footer_lines: usize,
    // Confirmation overlay (D key: set/unset default app). Single-column centered text.
    confirm_uniform_buf: wgpu::Buffer,
    confirm_uniform_bind: wgpu::BindGroup,
    confirm_text_buf: TextBuffer,
    confirm_layout_cache: String,
    confirm_line_h: f32,
    // Grid-filter search bar (find by name): full-width strip across the top of the grid.
    findbar_uniform_buf: wgpu::Buffer,
    findbar_uniform_bind: wgpu::BindGroup,
    findbar_text_buf: TextBuffer,
    findbar_layout_cache: String,
    // Grid renderer: pre-allocated pool of per-tile uniform bufs + bind groups.
    tile_bufs: Vec<wgpu::Buffer>,
    tile_binds: Vec<wgpu::BindGroup>,
    // Solid-color textures for grid UI elements.
    cell_bg_bind: wgpu::BindGroup,     // dark cell background
    sel_bind: wgpu::BindGroup,         // selection ring (blue)
    marquee_bind: wgpu::BindGroup,     // rubber-band rectangle fill (translucent blue)
    placeholder_bind: wgpu::BindGroup, // thumbnail not yet ready (mid-grey)
    // Date overlay (bottom-right, mirrors the path overlay on the left).
    date_uniform_buf: wgpu::Buffer,
    date_uniform_bind: wgpu::BindGroup,
    date_text_buf: TextBuffer,
    date_overlay_text: String,
    date_overlay_line_h: f32,
    // Watermark logo shown in the empty state (app icon, low opacity, centered).
    watermark_bind: wgpu::BindGroup,
    watermark_uniform_buf: wgpu::Buffer,
    watermark_uniform_bind: wgpu::BindGroup,
    // Version label under the watermark (empty state). Rebuilt only when the DPI scale changes.
    version_text_buf: TextBuffer,
    version_layout_scale: f32,
    // Donate text ("♥ Ko-fi Support · GitHub Sponsors") — reused in empty state, help & about.
    donate_text_buf: TextBuffer,
    donate_layout_scale: f32,
    /// Clickable donate-link zones from the last render (≥0; empty state and an overlay can coexist).
    donate_hits: Vec<DonateHit>,
    // About overlay (A key): centered box with version, license, and donate links.
    about_uniform_buf: wgpu::Buffer,
    about_uniform_bind: wgpu::BindGroup,
    about_text_buf: TextBuffer,
    about_layout_scale: f32,
    about_line_h: f32,
    // Info overlay (I key): translucent panel, top-left. The body text is supplied by App
    // (which owns the nav/metadata) via `render(.., info)`, mirroring the status overlay.
    info_uniform_buf: wgpu::Buffer,
    info_uniform_bind: wgpu::BindGroup,
    info_text_buf: TextBuffer,
    info_layout_scale: f32,
    info_line_h: f32,
    info_text_cache: String,
    // GPU-uploaded thumbnails keyed by path.
    thumb_binds: std::collections::HashMap<PathBuf, wgpu::BindGroup>,
    // SVG deep-zoom tiles: cached tile textures keyed by (epoch, col, row) with a last-used tick
    // for LRU eviction, plus a dedicated pool of per-tile MVP uniform buffers for compositing.
    svg_tile_tex: std::collections::HashMap<(u64, i32, i32), (wgpu::BindGroup, u64)>,
    svg_tile_mvp_bufs: Vec<wgpu::Buffer>,
    svg_tile_mvp_binds: Vec<wgpu::BindGroup>,
    // Explorer panel: per-item text buffers (index 0 = header, 1..n = entries) + solid textures.
    explorer_item_bufs: Vec<TextBuffer>,
    explorer_text_cache: String,
    explorer_layout_scale: f32,
    /// Intrinsic panel width (widest label + padding, in physical px). Depends only on text +
    /// scale, so it is cached in the rebuild block and re-clamped against the live window each frame.
    explorer_intrinsic_w: f32,
    /// Panel width after clamping the intrinsic width to the current window (recomputed per frame).
    explorer_computed_panel_w: f32,
    explorer_bg_bind: wgpu::BindGroup,
    explorer_header_bind: wgpu::BindGroup,
}

impl Gpu {
    async fn new(window: Arc<Window>, image: &DecodedImage) -> Gpu {
        let t0 = Instant::now();
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let surface = instance
            .create_surface(window.clone())
            .expect("create surface");

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::default(),
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .expect("no suitable GPU adapter");

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("glanvu device"),
                required_limits: wgpu::Limits::default(),
                ..Default::default()
            })
            .await
            .expect("request device");

        let size = window.inner_size();
        let config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .expect("surface not supported by this adapter");
        surface.configure(&device, &config);
        let srgb = config.format.is_srgb();

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("glanvu sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            // Trilinear: blend across the mipmap chain when the image is minified to fit/zoom-out,
            // so downscaled large images stay crisp instead of aliasing from a single texel tap.
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });

        let uniform_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("uniform layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let texture_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("texture layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        // Mipmap-generation blit pipeline. Its bind group (source mip + sampler) has the same
        // shape as `texture_layout`, so we reuse that layout for the source. The color target is
        // the image texture's own format so downsampling is gamma-correct for sRGB textures.
        let image_format = if srgb {
            wgpu::TextureFormat::Rgba8UnormSrgb
        } else {
            wgpu::TextureFormat::Rgba8Unorm
        };
        let mip_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("glanvu mip shader"),
            source: wgpu::ShaderSource::Wgsl(MIP_SHADER.into()),
        });
        let mip_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("glanvu mip pipeline layout"),
            bind_group_layouts: &[Some(&texture_layout)],
            immediate_size: 0,
        });
        let mip_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("glanvu mip pipeline"),
            layout: Some(&mip_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &mip_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &mip_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: image_format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let uniform_buf = make_uniform_buffer(&device);
        let uniform_bind = make_uniform_bind(&device, &uniform_layout, &uniform_buf);
        let texture_bind = build_image_texture_bind(
            &device,
            &queue,
            &texture_layout,
            &sampler,
            &mip_pipeline,
            srgb,
            image,
        );

        let box_uniform_buf = make_uniform_buffer(&device);
        let box_uniform_bind = make_uniform_bind(&device, &uniform_layout, &box_uniform_buf);
        let black = DecodedImage {
            width: 1,
            height: 1,
            rgba: vec![0, 0, 0, 170],
        };
        let box_texture_bind =
            build_texture_bind(&device, &queue, &texture_layout, &sampler, srgb, &black);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("glanvu shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("glanvu pipeline layout"),
            bind_group_layouts: &[Some(&uniform_layout), Some(&texture_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("glanvu pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Vertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("glanvu vertices"),
            contents: bytemuck::cast_slice(&VERTICES),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let index_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("glanvu indices"),
            contents: bytemuck::cast_slice(&INDICES),
            usage: wgpu::BufferUsages::INDEX,
        });

        let mut font_system = FontSystem::new();
        let swash_cache = SwashCache::new();
        let glyph_cache = GlyphCache::new(&device);
        let viewport = Viewport::new(&device, &glyph_cache);
        let mut atlas = TextAtlas::new(&device, &queue, &glyph_cache, config.format);
        let text_renderer =
            TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);
        let text_buffer = TextBuffer::new(&mut font_system, Metrics::new(14.0, 18.0));
        let status_text_buf = TextBuffer::new(&mut font_system, Metrics::new(20.0, 26.0));

        if perf_logging() {
            eprintln!(
                "glanvu: gpu init in {:.1} ms (one-time)",
                t0.elapsed().as_secs_f64() * 1000.0
            );
        }

        // Grid: pool of per-tile uniform buffers + bind groups.
        let mut tile_bufs = Vec::with_capacity(TILE_POOL);
        let mut tile_binds = Vec::with_capacity(TILE_POOL);
        let ul = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tile uniform layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        for _ in 0..TILE_POOL {
            let buf = make_uniform_buffer(&device);
            let bind = make_uniform_bind(&device, &ul, &buf);
            tile_bufs.push(buf);
            tile_binds.push(bind);
        }
        // Dedicated MVP-uniform pool for compositing SVG deep-zoom tiles (reuses the same layout).
        let mut svg_tile_mvp_bufs = Vec::with_capacity(SVG_TILE_POOL);
        let mut svg_tile_mvp_binds = Vec::with_capacity(SVG_TILE_POOL);
        for _ in 0..SVG_TILE_POOL {
            let buf = make_uniform_buffer(&device);
            let bind = make_uniform_bind(&device, &ul, &buf);
            svg_tile_mvp_bufs.push(buf);
            svg_tile_mvp_binds.push(bind);
        }
        // Solid-color textures: dark bg, blue selection, mid-grey placeholder.
        let cell_bg = DecodedImage {
            width: 1,
            height: 1,
            rgba: vec![35, 35, 37, 255],
        };
        let cell_bg_bind =
            build_texture_bind(&device, &queue, &texture_layout, &sampler, srgb, &cell_bg);
        let sel = DecodedImage {
            width: 1,
            height: 1,
            rgba: vec![60, 120, 220, 230],
        };
        let sel_bind = build_texture_bind(&device, &queue, &texture_layout, &sampler, srgb, &sel);
        // Rubber-band fill: same blue but mostly transparent so thumbnails show through.
        let marquee = DecodedImage {
            width: 1,
            height: 1,
            rgba: vec![80, 140, 230, 64],
        };
        let marquee_bind =
            build_texture_bind(&device, &queue, &texture_layout, &sampler, srgb, &marquee);
        let ph = DecodedImage {
            width: 1,
            height: 1,
            rgba: vec![70, 70, 72, 255],
        };
        let placeholder_bind =
            build_texture_bind(&device, &queue, &texture_layout, &sampler, srgb, &ph);

        // Status overlay (centered slideshow message) — needs its own uniform buf/bind.
        let status_uniform_buf = make_uniform_buffer(&device);
        let explorer_bg_bind = {
            // Near-opaque dark bg: 248/255 ≈ 97% opacity.
            let bg = DecodedImage {
                width: 1,
                height: 1,
                rgba: vec![18, 20, 26, 248],
            };
            build_texture_bind(&device, &queue, &texture_layout, &sampler, srgb, &bg)
        };
        let explorer_header_bind = {
            // Solid dark-blue header bar for the current-directory label.
            let bg = DecodedImage {
                width: 1,
                height: 1,
                rgba: vec![35, 65, 145, 255],
            };
            build_texture_bind(&device, &queue, &texture_layout, &sampler, srgb, &bg)
        };
        let uniform_layout_solo =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("solo uniform layout"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });
        let status_uniform_bind =
            make_uniform_bind(&device, &uniform_layout_solo, &status_uniform_buf);
        let help_uniform_buf = make_uniform_buffer(&device);
        let help_uniform_bind = make_uniform_bind(&device, &uniform_layout_solo, &help_uniform_buf);
        let help_title_buf = TextBuffer::new(&mut font_system, Metrics::new(15.0, 22.0));
        let help_l_keys_buf = TextBuffer::new(&mut font_system, Metrics::new(15.0, 22.0));
        let help_l_desc_buf = TextBuffer::new(&mut font_system, Metrics::new(15.0, 22.0));
        let help_r_keys_buf = TextBuffer::new(&mut font_system, Metrics::new(15.0, 22.0));
        let help_r_desc_buf = TextBuffer::new(&mut font_system, Metrics::new(15.0, 22.0));
        let help_f_keys_buf = TextBuffer::new(&mut font_system, Metrics::new(15.0, 22.0));
        let help_f_desc_buf = TextBuffer::new(&mut font_system, Metrics::new(15.0, 22.0));
        let confirm_uniform_buf = make_uniform_buffer(&device);
        let confirm_uniform_bind =
            make_uniform_bind(&device, &uniform_layout_solo, &confirm_uniform_buf);
        let confirm_text_buf = TextBuffer::new(&mut font_system, Metrics::new(15.0, 22.0));
        let findbar_uniform_buf = make_uniform_buffer(&device);
        let findbar_uniform_bind =
            make_uniform_bind(&device, &uniform_layout_solo, &findbar_uniform_buf);
        let findbar_text_buf = TextBuffer::new(&mut font_system, Metrics::new(16.0, 22.0));
        let donate_text_buf = TextBuffer::new(&mut font_system, Metrics::new(13.0, 17.0));
        let about_uniform_buf = make_uniform_buffer(&device);
        let about_uniform_bind =
            make_uniform_bind(&device, &uniform_layout_solo, &about_uniform_buf);
        let about_text_buf = TextBuffer::new(&mut font_system, Metrics::new(15.0, 22.0));

        let info_uniform_buf = make_uniform_buffer(&device);
        let info_uniform_bind =
            make_uniform_bind(&device, &uniform_layout_solo, &info_uniform_buf);
        let info_text_buf = TextBuffer::new(&mut font_system, Metrics::new(15.0, 22.0));

        // Date overlay (bottom-right counterpart to the path overlay).
        let date_uniform_buf = make_uniform_buffer(&device);
        let date_uniform_bind = make_uniform_bind(&device, &uniform_layout_solo, &date_uniform_buf);
        let date_text_buf = TextBuffer::new(&mut font_system, Metrics::new(14.0, 18.0));

        // Watermark: decode app icon, bake ~22% opacity, upload as texture.
        let watermark_bind = {
            let mut img =
                glanvu_core::decode_bytes(WATERMARK_BYTES).unwrap_or_else(|_| DecodedImage {
                    width: 1,
                    height: 1,
                    rgba: vec![0, 0, 0, 0],
                });
            for px in img.rgba.chunks_mut(4) {
                px[3] = (px[3] as f32 * 0.22) as u8;
            }
            build_texture_bind(&device, &queue, &texture_layout, &sampler, false, &img)
        };
        let watermark_uniform_buf = make_uniform_buffer(&device);
        let watermark_uniform_bind =
            make_uniform_bind(&device, &uniform_layout_solo, &watermark_uniform_buf);

        // Version label buffer (text + size filled in by `layout_version` on first empty-state draw).
        let version_text_buf = TextBuffer::new(&mut font_system, Metrics::new(14.0, 18.0));

        Gpu {
            surface,
            device,
            queue,
            config,
            pipeline,
            mip_pipeline,
            texture_layout,
            sampler,
            uniform_buf,
            uniform_bind,
            texture_bind,
            vertex_buf,
            index_buf,
            box_uniform_buf,
            box_uniform_bind,
            box_texture_bind,
            font_system,
            swash_cache,
            viewport,
            atlas,
            text_renderer,
            text_buffer,
            overlay_text: String::new(),
            overlay_line_h: 18.0,
            status_uniform_buf,
            status_uniform_bind,
            status_text_buf,
            status_overlay_text: String::new(),
            help_uniform_buf,
            help_uniform_bind,
            help_title_buf,
            help_l_keys_buf,
            help_l_desc_buf,
            help_r_keys_buf,
            help_r_desc_buf,
            help_f_keys_buf,
            help_f_desc_buf,
            help_layout_scale: -1.0,
            help_line_h: 22.0,
            help_col_lines: 0,
            help_footer_lines: 0,
            findbar_uniform_buf,
            findbar_uniform_bind,
            findbar_text_buf,
            findbar_layout_cache: String::new(),
            confirm_uniform_buf,
            confirm_uniform_bind,
            confirm_text_buf,
            confirm_layout_cache: String::new(),
            confirm_line_h: 22.0,
            tile_bufs,
            tile_binds,
            cell_bg_bind,
            sel_bind,
            marquee_bind,
            placeholder_bind,
            date_uniform_buf,
            date_uniform_bind,
            date_text_buf,
            date_overlay_text: String::new(),
            date_overlay_line_h: 18.0,
            watermark_bind,
            watermark_uniform_buf,
            watermark_uniform_bind,
            version_text_buf,
            version_layout_scale: -1.0,
            donate_text_buf,
            donate_layout_scale: -1.0,
            donate_hits: Vec::new(),
            about_uniform_buf,
            about_uniform_bind,
            about_text_buf,
            about_layout_scale: -1.0,
            about_line_h: 22.0,
            info_uniform_buf,
            info_uniform_bind,
            info_text_buf,
            info_layout_scale: -1.0,
            info_line_h: 22.0,
            info_text_cache: String::new(),
            thumb_binds: std::collections::HashMap::new(),
            svg_tile_tex: std::collections::HashMap::new(),
            svg_tile_mvp_bufs,
            svg_tile_mvp_binds,
            explorer_item_bufs: Vec::new(),
            explorer_text_cache: String::new(),
            explorer_layout_scale: -1.0,
            explorer_intrinsic_w: PANEL_W,
            explorer_computed_panel_w: PANEL_W,
            explorer_bg_bind,
            explorer_header_bind,
        }
    }

    fn set_image(&mut self, image: &DecodedImage) {
        self.texture_bind = build_image_texture_bind(
            &self.device,
            &self.queue,
            &self.texture_layout,
            &self.sampler,
            &self.mip_pipeline,
            self.config.format.is_srgb(),
            image,
        );
    }

    fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width > 0 && size.height > 0 {
            self.config.width = size.width;
            self.config.height = size.height;
            self.surface.configure(&self.device, &self.config);
        }
    }

    fn layout_overlay(
        &mut self,
        text: &str,
        scale: f32,
        win_w: f32,
        win_h: f32,
    ) -> (f32, f32, f32, f32, f32, f32) {
        if self.overlay_text != text {
            self.overlay_text = text.to_string();
            let font = (13.0 * scale).clamp(12.0, 30.0);
            self.overlay_line_h = font * 1.35;
            let mut buf = TextBuffer::new(
                &mut self.font_system,
                Metrics::new(font, self.overlay_line_h),
            );
            buf.set_size(&mut self.font_system, None, None);
            buf.set_text(
                &mut self.font_system,
                text,
                &Attrs::new(),
                Shaping::Advanced,
                None,
            );
            buf.shape_until_scroll(&mut self.font_system, false);
            self.text_buffer = buf;
        }
        let pad = 8.0 * scale;
        let margin = 12.0 * scale;
        let text_w = self
            .text_buffer
            .layout_runs()
            .fold(0.0_f32, |m, r| m.max(r.line_w));
        let bw = (text_w + 2.0 * pad)
            .min(win_w - 2.0 * margin)
            .max(2.0 * pad);
        let bh = self.overlay_line_h + 2.0 * pad;
        let bx = margin;
        let by = (win_h - margin - bh).max(0.0);
        (bx, by, bw, bh, bx + pad, by + pad)
    }

    /// Lay out the date overlay (bottom-right, mirrors the path overlay).
    fn layout_date_overlay(
        &mut self,
        text: &str,
        scale: f32,
        win_w: f32,
        win_h: f32,
    ) -> (f32, f32, f32, f32, f32, f32) {
        if self.date_overlay_text != text {
            self.date_overlay_text = text.to_string();
            let font = (13.0 * scale).clamp(12.0, 30.0);
            self.date_overlay_line_h = font * 1.35;
            let mut buf = TextBuffer::new(
                &mut self.font_system,
                Metrics::new(font, self.date_overlay_line_h),
            );
            buf.set_size(&mut self.font_system, None, None);
            buf.set_text(
                &mut self.font_system,
                text,
                &Attrs::new(),
                Shaping::Advanced,
                None,
            );
            buf.shape_until_scroll(&mut self.font_system, false);
            self.date_text_buf = buf;
        }
        let pad = 8.0 * scale;
        let margin = 12.0 * scale;
        let text_w = self
            .date_text_buf
            .layout_runs()
            .fold(0.0_f32, |m, r| m.max(r.line_w));
        let bw = (text_w + 2.0 * pad)
            .min(win_w - 2.0 * margin)
            .max(2.0 * pad);
        let bh = self.date_overlay_line_h + 2.0 * pad;
        let bx = (win_w - margin - bw).max(0.0); // right-aligned
        let by = (win_h - margin - bh).max(0.0);
        (bx, by, bw, bh, bx + pad, by + pad)
    }

    /// Lay out the version label centered under the watermark (empty state). No background box.
    /// Returns `(text_x, text_y)` in physical pixels. The buffer is rebuilt only when `scale`
    /// changes; the label text itself is static.
    fn layout_version(&mut self, scale: f32, win_w: f32, win_h: f32) -> (f32, f32) {
        if (self.version_layout_scale - scale).abs() > f32::EPSILON {
            self.version_layout_scale = scale;
            let font = (14.0 * scale).clamp(12.0, 28.0);
            let mut buf = TextBuffer::new(&mut self.font_system, Metrics::new(font, font * 1.3));
            buf.set_wrap(&mut self.font_system, Wrap::None);
            buf.set_size(&mut self.font_system, None, None);
            buf.set_text(
                &mut self.font_system,
                VERSION_LABEL,
                &Attrs::new(),
                Shaping::Basic,
                None,
            );
            buf.shape_until_scroll(&mut self.font_system, false);
            self.version_text_buf = buf;
        }
        let text_w = self
            .version_text_buf
            .layout_runs()
            .fold(0.0_f32, |m, r| m.max(r.line_w));
        // Watermark is a centered square sized to 40% of the shorter window dimension.
        let logo_size = (win_w.min(win_h) * 0.40).max(64.0);
        let logo_bottom = ((win_h - logo_size) / 2.0).round() + logo_size;
        let left = ((win_w - text_w) / 2.0).max(0.0);
        let top = logo_bottom + 16.0 * scale;
        (left, top)
    }

    /// Lay out the donate line ("♥ Ko-fi · GitHub Sponsors") centered below the version label.
    /// Rebuilds the buffer only when `scale` changes. Returns `(text_x, text_y)`.
    fn layout_donate(&mut self, scale: f32, win_w: f32, win_h: f32) -> (f32, f32) {
        if (self.donate_layout_scale - scale).abs() > f32::EPSILON {
            self.donate_layout_scale = scale;
            let font = (13.0 * scale).clamp(11.0, 26.0);
            let mut buf = TextBuffer::new(&mut self.font_system, Metrics::new(font, font * 1.3));
            buf.set_wrap(&mut self.font_system, Wrap::None);
            buf.set_size(&mut self.font_system, None, None);
            buf.set_text(
                &mut self.font_system,
                DONATE_LINE,
                &Attrs::new(),
                Shaping::Basic,
                None,
            );
            buf.shape_until_scroll(&mut self.font_system, false);
            self.donate_text_buf = buf;
        }
        let text_w = self
            .donate_text_buf
            .layout_runs()
            .fold(0.0_f32, |m, r| m.max(r.line_w));
        let version_font = (14.0 * scale).clamp(12.0, 28.0);
        let version_lh = version_font * 1.3;
        let logo_size = (win_w.min(win_h) * 0.40).max(64.0);
        let logo_bottom = ((win_h - logo_size) / 2.0).round() + logo_size;
        let version_y = logo_bottom + 16.0 * scale;
        let top = version_y + version_lh + 6.0 * scale;
        let left = ((win_w - text_w) / 2.0).max(0.0);
        (left, top)
    }

    /// Number of text lines in the About body (head), used for box height and donate placement.
    const ABOUT_HEAD_LINES: usize = {
        // Count '\n' + 1. `const`-friendly manual count.
        let bytes = ABOUT_HEAD.as_bytes();
        let mut n = 1usize;
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'\n' {
                n += 1;
            }
            i += 1;
        }
        n
    };

    /// Lay out the About overlay (A key). The body (head) is white; the donate line is rendered
    /// separately (blue, centered) below it. Returns `(box_x, box_y, box_w, box_h, text_x, text_y)`.
    fn layout_about(&mut self, scale: f32, win_w: f32, win_h: f32) -> (f32, f32, f32, f32, f32, f32) {
        if (self.about_layout_scale - scale).abs() > f32::EPSILON {
            self.about_layout_scale = scale;
            let font = (15.0 * scale).clamp(13.0, 30.0);
            self.about_line_h = font * 1.5;
            let mut buf = TextBuffer::new(
                &mut self.font_system,
                Metrics::new(font, self.about_line_h),
            );
            buf.set_wrap(&mut self.font_system, Wrap::None);
            buf.set_size(&mut self.font_system, None, None);
            buf.set_text(
                &mut self.font_system,
                ABOUT_HEAD,
                &Attrs::new(),
                Shaping::Basic,
                None,
            );
            buf.shape_until_scroll(&mut self.font_system, false);
            self.about_text_buf = buf;
        }
        let pad_h = 40.0 * scale;
        let pad_v = 28.0 * scale;
        let head_w = self
            .about_text_buf
            .layout_runs()
            .fold(0.0_f32, |m, r| m.max(r.line_w));
        let donate_w = self
            .donate_text_buf
            .layout_runs()
            .fold(0.0_f32, |m, r| m.max(r.line_w));
        let text_w = head_w.max(donate_w);
        // Rows: head lines + one blank gap + one donate row.
        let rows = (Self::ABOUT_HEAD_LINES + 2) as f32;
        let bw = (text_w + 2.0 * pad_h).min(win_w - 40.0);
        let bh = (rows * self.about_line_h + 2.0 * pad_v).min(win_h - 40.0);
        let bx = ((win_w - bw) / 2.0).max(0.0);
        let by = ((win_h - bh) / 2.0).max(0.0);
        (bx, by, bw, bh, bx + pad_h, by + pad_v)
    }

    /// Lay out the info overlay (I key): translucent box anchored top-left, left-aligned text.
    /// `text` is the metadata body supplied by App. Reshapes only when the text or scale changes.
    /// Returns `(box_x, box_y, box_w, box_h, text_x, text_y)`.
    fn layout_info(
        &mut self,
        text: &str,
        scale: f32,
        win_w: f32,
        win_h: f32,
    ) -> (f32, f32, f32, f32, f32, f32) {
        if self.info_text_cache != text
            || (self.info_layout_scale - scale).abs() > f32::EPSILON
        {
            self.info_text_cache = text.to_string();
            self.info_layout_scale = scale;
            let font = (15.0 * scale).clamp(13.0, 30.0);
            self.info_line_h = font * 1.5;
            let mut buf =
                TextBuffer::new(&mut self.font_system, Metrics::new(font, self.info_line_h));
            buf.set_wrap(&mut self.font_system, Wrap::None);
            buf.set_size(&mut self.font_system, None, None);
            buf.set_text(
                &mut self.font_system,
                text,
                &Attrs::new(),
                Shaping::Basic,
                None,
            );
            buf.shape_until_scroll(&mut self.font_system, false);
            self.info_text_buf = buf;
        }
        let pad_h = 16.0 * scale;
        let pad_v = 12.0 * scale;
        let margin = 16.0 * scale;
        let text_w = self
            .info_text_buf
            .layout_runs()
            .fold(0.0_f32, |m, r| m.max(r.line_w));
        let rows = text.lines().count().max(1) as f32;
        let bw = (text_w + 2.0 * pad_h).min(win_w - 2.0 * margin);
        let bh = (rows * self.info_line_h + 2.0 * pad_v).min(win_h - 2.0 * margin);
        (margin, margin, bw, bh, margin + pad_h, margin + pad_v)
    }

    /// Lay out the centered status overlay (slideshow message, etc.).
    /// Returns `(box_x, box_y, box_w, box_h, text_x, text_y)` in physical pixels.
    fn layout_status(
        &mut self,
        text: &str,
        scale: f32,
        win_w: f32,
        win_h: f32,
    ) -> (f32, f32, f32, f32, f32, f32) {
        if self.status_overlay_text != text {
            self.status_overlay_text = text.to_string();
            let font = (20.0 * scale).clamp(16.0, 40.0);
            let lh = font * 1.35;
            let mut buf = TextBuffer::new(&mut self.font_system, Metrics::new(font, lh));
            buf.set_size(&mut self.font_system, None, None);
            buf.set_text(
                &mut self.font_system,
                text,
                &Attrs::new(),
                Shaping::Advanced,
                None,
            );
            buf.shape_until_scroll(&mut self.font_system, false);
            self.status_text_buf = buf;
        }
        let pad_h = 12.0 * scale;
        let pad_v = 10.0 * scale;
        let text_w = self
            .status_text_buf
            .layout_runs()
            .fold(0.0_f32, |m, r| m.max(r.line_w));
        let lh = self.status_text_buf.metrics().line_height;
        let bw = (text_w + 2.0 * pad_h).max(2.0 * pad_h);
        let bh = lh + 2.0 * pad_v;
        let bx = ((win_w - bw) / 2.0).max(0.0);
        let by = win_h * 0.38;
        (bx, by, bw, bh, bx + pad_h, by + pad_v)
    }

    /// Build one help block (`keys` rich-text buffer + `desc` buffer) from a slice of rows, with a
    /// blank line inserted before each section except the first. Returns the two buffers and the
    /// line count. Section headers render in the accent colour.
    fn build_help_block(&mut self, rows: &[HelpRow], font: f32) -> (TextBuffer, TextBuffer, usize) {
        let accent = Attrs::new()
            .weight(Weight::BOLD)
            .color(Color::rgb(125, 178, 255));
        let plain = Attrs::new();

        let mut lines: Vec<(String, bool, String)> = Vec::with_capacity(rows.len() + 2);
        let mut first_section = true;
        for row in rows {
            match row {
                Section(name) => {
                    if !first_section {
                        lines.push((String::new(), false, String::new()));
                    }
                    first_section = false;
                    lines.push(((*name).to_string(), true, String::new()));
                }
                Keys(k, d) => lines.push(((*k).to_string(), false, (*d).to_string())),
            }
        }
        let line_h = font * 1.5;

        let mut keys = TextBuffer::new(&mut self.font_system, Metrics::new(font, line_h));
        keys.set_wrap(&mut self.font_system, Wrap::None);
        keys.set_size(&mut self.font_system, None, None);
        let spans: Vec<(String, bool)> = lines
            .iter()
            .enumerate()
            .map(|(i, (k, hdr, _))| {
                let s = if i + 1 < lines.len() {
                    format!("{k}\n")
                } else {
                    k.clone()
                };
                (s, *hdr)
            })
            .collect();
        keys.set_rich_text(
            &mut self.font_system,
            spans
                .iter()
                .map(|(s, hdr)| (s.as_str(), if *hdr { accent.clone() } else { plain.clone() })),
            &plain,
            Shaping::Basic,
            None,
        );
        keys.shape_until_scroll(&mut self.font_system, false);

        let desc_str: String = lines
            .iter()
            .map(|(_, _, d)| d.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let mut desc = TextBuffer::new(&mut self.font_system, Metrics::new(font, line_h));
        desc.set_wrap(&mut self.font_system, Wrap::None);
        desc.set_size(&mut self.font_system, None, None);
        desc.set_text(&mut self.font_system, &desc_str, &plain, Shaping::Basic, None);
        desc.shape_until_scroll(&mut self.font_system, false);

        (keys, desc, lines.len())
    }

    /// Lay out the help overlay: centered title, a left and right column of sections side by side,
    /// and a centered footer block below them. Returns the box + every text origin.
    fn layout_help(&mut self, scale: f32, win_w: f32, win_h: f32) -> HelpLayout {
        let font = (15.0 * scale).clamp(13.0, 30.0);
        if (self.help_layout_scale - scale).abs() > f32::EPSILON {
            self.help_layout_scale = scale;
            self.help_line_h = font * 1.5;

            // Title (centered, bold, bright).
            let mut title = TextBuffer::new(&mut self.font_system, Metrics::new(font, font * 1.5));
            title.set_wrap(&mut self.font_system, Wrap::None);
            title.set_size(&mut self.font_system, None, None);
            title.set_rich_text(
                &mut self.font_system,
                [(HELP_TITLE, Attrs::new().weight(Weight::BOLD))],
                &Attrs::new(),
                Shaping::Basic,
                None,
            );
            title.shape_until_scroll(&mut self.font_system, false);
            self.help_title_buf = title;

            let (lk, ld, lc) = self.build_help_block(HELP_LEFT, font);
            self.help_l_keys_buf = lk;
            self.help_l_desc_buf = ld;
            let (rk, rd, rc) = self.build_help_block(HELP_RIGHT, font);
            self.help_r_keys_buf = rk;
            self.help_r_desc_buf = rd;
            let (fk, fd, fc) = self.build_help_block(HELP_FOOTER, font);
            self.help_f_keys_buf = fk;
            self.help_f_desc_buf = fd;
            self.help_col_lines = lc.max(rc);
            self.help_footer_lines = fc;
        }

        let lh = self.help_line_h;
        let pad_h = 30.0 * scale;
        let pad_v = 22.0 * scale;
        let kd_gap = 24.0 * scale; // keys → desc gap within a block
        let col_gap = 52.0 * scale; // left column → right column gap
        let width = |b: &TextBuffer| b.layout_runs().fold(0.0_f32, |m, r| m.max(r.line_w));

        let lkw = width(&self.help_l_keys_buf);
        let ldw = width(&self.help_l_desc_buf);
        let rkw = width(&self.help_r_keys_buf);
        let rdw = width(&self.help_r_desc_buf);
        let fkw = width(&self.help_f_keys_buf);
        let fdw = width(&self.help_f_desc_buf);
        let title_w = width(&self.help_title_buf);

        let lcol_w = lkw + kd_gap + ldw;
        let rcol_w = rkw + kd_gap + rdw;
        let cols_w = lcol_w + col_gap + rcol_w;
        let footer_w = fkw + kd_gap + fdw;
        let content_w = cols_w.max(footer_w).max(title_w);

        // Vertical: title + blank, columns, blank, footer, blank + donate row.
        let total_lines =
            2.0 + self.help_col_lines as f32 + 1.0 + self.help_footer_lines as f32 + 1.0;
        let bw = (content_w + 2.0 * pad_h).min(win_w - 40.0);
        let bh = (total_lines * lh + 2.0 * pad_v).min(win_h - 40.0);
        let bx = ((win_w - bw) / 2.0).max(0.0);
        let by = ((win_h - bh) / 2.0).max(0.0);
        let cx = bx + bw / 2.0;

        let cols_left = cx - cols_w / 2.0;
        let l_keys_x = cols_left;
        let l_desc_x = l_keys_x + lkw + kd_gap;
        let r_keys_x = cols_left + lcol_w + col_gap;
        let r_desc_x = r_keys_x + rkw + kd_gap;

        let title_x = cx - title_w / 2.0;
        let title_y = by + pad_v;
        let col_top = title_y + 2.0 * lh;
        let footer_top = col_top + (self.help_col_lines as f32 + 1.0) * lh;
        let f_keys_x = cx - footer_w / 2.0;
        let f_desc_x = f_keys_x + fkw + kd_gap;
        let donate_top = footer_top + self.help_footer_lines as f32 * lh;

        HelpLayout {
            bx,
            by,
            bw,
            bh,
            title_x,
            title_y,
            col_top,
            l_keys_x,
            l_desc_x,
            r_keys_x,
            r_desc_x,
            footer_top,
            f_keys_x,
            f_desc_x,
            donate_top,
        }
    }

    /// Lay out the confirmation overlay (D key). Single-column centered text.
    /// Returns `(box_x, box_y, box_w, box_h, text_x, text_y)`.
    fn layout_confirm(
        &mut self,
        text: &str,
        scale: f32,
        win_w: f32,
        win_h: f32,
    ) -> (f32, f32, f32, f32, f32, f32) {
        if self.confirm_layout_cache != text {
            self.confirm_layout_cache = text.to_string();
            let font = (15.0 * scale).clamp(13.0, 30.0);
            self.confirm_line_h = font * 1.5;
            let mut buf = TextBuffer::new(
                &mut self.font_system,
                Metrics::new(font, self.confirm_line_h),
            );
            buf.set_wrap(&mut self.font_system, Wrap::None);
            buf.set_size(&mut self.font_system, None, None);
            buf.set_text(
                &mut self.font_system,
                text,
                &Attrs::new(),
                Shaping::Basic,
                None,
            );
            buf.shape_until_scroll(&mut self.font_system, false);
            self.confirm_text_buf = buf;
        }
        let pad_h = 36.0 * scale;
        let pad_v = 24.0 * scale;
        let text_w = self
            .confirm_text_buf
            .layout_runs()
            .fold(0.0_f32, |m, r| m.max(r.line_w));
        let lines = text.lines().count().max(1) as f32;
        let bw = (text_w + 2.0 * pad_h).min(win_w - 40.0);
        let bh = (lines * self.confirm_line_h + 2.0 * pad_v).min(win_h - 40.0);
        let bx = ((win_w - bw) / 2.0).max(0.0);
        let by = ((win_h - bh) / 2.0).max(0.0);
        (bx, by, bw, bh, bx + pad_h, by + pad_v)
    }

    /// Lay out the grid-filter search bar: a full-width strip across the top, text left-aligned.
    /// Returns `(box_x, box_y, box_w, box_h, text_x, text_y)`. Height must match `find_bar_height`.
    fn layout_find_bar(&mut self, text: &str, scale: f32, win_w: f32) -> (f32, f32, f32, f32, f32, f32) {
        if self.findbar_layout_cache != text {
            self.findbar_layout_cache = text.to_string();
            let font = (16.0 * scale).clamp(14.0, 32.0);
            let mut buf = TextBuffer::new(&mut self.font_system, Metrics::new(font, font * 1.4));
            buf.set_wrap(&mut self.font_system, Wrap::None);
            buf.set_size(&mut self.font_system, None, None);
            buf.set_text(&mut self.font_system, text, &Attrs::new(), Shaping::Advanced, None);
            buf.shape_until_scroll(&mut self.font_system, false);
            self.findbar_text_buf = buf;
        }
        let pad_h = 14.0 * scale;
        let pad_v = 8.0 * scale;
        let bh = find_bar_height(scale);
        (0.0, 0.0, win_w, bh, pad_h, pad_v)
    }

    /// Render one frame in single-image mode.
    ///
    /// All text overlays (path overlay, status message, explorer panel) are prepared in ONE
    /// `text_renderer.prepare()` call so no overlay replaces another. All box draws happen before
    /// the single `text_renderer.render()` call at the end of the pass, so the pipeline is only
    /// switched once from ours to glyphon's.
    ///
    /// The raw-pointer trick in the prepare section borrows `text_buffer`, `status_text_buf`, and
    /// `explorer_text_buf` (read-only) while `font_system`, `atlas`, and `swash_cache` are borrowed
    /// mutably — all distinct fields, so it is sound. The `unsafe_code = deny` workspace lint is
    /// selectively lifted here because this is the canonical solution to the Rust "can't borrow
    /// disjoint fields via &mut self" limitation in this specific GPU context.
    #[allow(unsafe_code)]
    #[allow(clippy::too_many_arguments)]
    fn render(
        &mut self,
        uniforms: Uniforms,
        overlay: Option<&str>,
        date_overlay: Option<&str>,
        status: Option<&str>,
        explorer: Option<&ExplorerState>,
        help: bool,
        confirm: Option<&str>,
        about: bool,
        info: Option<&str>,
        scale: f32,
        logo: bool,
        svg_tiles: &[SvgTileDraw],
    ) -> bool {
        self.queue
            .write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(&uniforms));

        // Pre-write the MVP uniform for each composited SVG tile (bounded by the pool).
        let svg_tile_count = svg_tiles.len().min(self.svg_tile_mvp_bufs.len());
        for (i, tile) in svg_tiles.iter().take(svg_tile_count).enumerate() {
            self.queue.write_buffer(
                &self.svg_tile_mvp_bufs[i],
                0,
                bytemuck::bytes_of(&Uniforms { mvp: tile.mvp }),
            );
        }

        let (win_w, win_h) = (self.config.width as f32, self.config.height as f32);

        // Donate-link hit zones are recomputed from scratch each frame.
        self.donate_hits.clear();

        // ---- Collect positions for box draws and text areas ----

        // Path overlay (bottom-left). Suppressed while the info panel is open — the path is
        // already shown there, so the bottom-left overlay would be redundant.
        let mut show_overlay_box = false;
        let mut overlay_coords = (0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32);
        if let Some(text) = overlay.filter(|_| info.is_none()) {
            overlay_coords = self.layout_overlay(text, scale, win_w, win_h);
            let (bx, by, bw, bh, _, _) = overlay_coords;
            self.queue.write_buffer(
                &self.box_uniform_buf,
                0,
                bytemuck::bytes_of(&Uniforms {
                    mvp: rect_mvp(bw, bh, bx, by, win_w, win_h),
                }),
            );
            show_overlay_box = true;
        }

        // Date overlay (bottom-right).
        let mut show_date_box = false;
        let mut date_coords = (0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32);
        if let Some(text) = date_overlay {
            date_coords = self.layout_date_overlay(text, scale, win_w, win_h);
            let (bx, by, bw, bh, _, _) = date_coords;
            self.queue.write_buffer(
                &self.date_uniform_buf,
                0,
                bytemuck::bytes_of(&Uniforms {
                    mvp: rect_mvp(bw, bh, bx, by, win_w, win_h),
                }),
            );
            show_date_box = true;
        }

        // Status message (centered).
        let mut show_status_box = false;
        let mut status_coords = (0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32);
        if let Some(text) = status {
            status_coords = self.layout_status(text, scale, win_w, win_h);
            let (bx, by, bw, bh, _, _) = status_coords;
            self.queue.write_buffer(
                &self.status_uniform_buf,
                0,
                bytemuck::bytes_of(&Uniforms {
                    mvp: rect_mvp(bw, bh, bx, by, win_w, win_h),
                }),
            );
            show_status_box = true;
        }

        // Donate line buffer (blue, centered) — shared by the empty state, help footer, and About.
        let donate_pos = self.layout_donate(scale, win_w, win_h);
        let donate_text_w = self
            .donate_text_buf
            .layout_runs()
            .fold(0.0_f32, |m, r| m.max(r.line_w));
        let don_lh = (13.0 * scale).clamp(11.0, 26.0) * 1.3;
        // Centered horizontal origin (window center == any centered box center).
        let donate_left = ((win_w - donate_text_w) / 2.0).max(0.0);
        // Push a clickable hit zone (left half → Ko-fi, right half → Sponsors) for a donate line at `top`.
        macro_rules! push_donate_hit {
            ($top:expr) => {{
                let top = $top;
                self.donate_hits.push(DonateHit {
                    x0: donate_left,
                    y0: top,
                    x1: donate_left + donate_text_w,
                    y1: top + don_lh,
                    split_x: donate_left + donate_text_w / 2.0,
                });
            }};
        }

        // Donate footer for help overlay — declared here, positioned inside the help block below.
        let mut show_help_donate = false;
        let mut help_donate_coords = (0.0_f32, 0.0_f32);

        // Help overlay (centered title + two section columns + centered footer block).
        let mut show_help_box = false;
        let mut help_layout: Option<HelpLayout> = None;
        if help {
            let hl = self.layout_help(scale, win_w, win_h);
            self.queue.write_buffer(
                &self.help_uniform_buf,
                0,
                bytemuck::bytes_of(&Uniforms {
                    mvp: rect_mvp(hl.bw, hl.bh, hl.bx, hl.by, win_w, win_h),
                }),
            );
            show_help_box = true;
            // Donate footer sits one row below the footer block (its row is reserved in the box).
            let donate_top = hl.donate_top + (self.help_line_h - don_lh) / 2.0;
            help_donate_coords = (donate_left, donate_top);
            show_help_donate = !about; // hide if obscured by the about overlay
            if show_help_donate {
                push_donate_hit!(donate_top);
            }
            help_layout = Some(hl);
        }

        // Confirmation overlay (D key: set/unset default app).
        let mut show_confirm_box = false;
        let mut confirm_coords = (0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32);
        if let Some(text) = confirm {
            confirm_coords = self.layout_confirm(text, scale, win_w, win_h);
            let (bx, by, bw, bh, _, _) = confirm_coords;
            self.queue.write_buffer(
                &self.confirm_uniform_buf,
                0,
                bytemuck::bytes_of(&Uniforms {
                    mvp: rect_mvp(bw, bh, bx, by, win_w, win_h),
                }),
            );
            show_confirm_box = true;
        }

        // Watermark logo (empty state only): centered square, 40% of the shorter dimension.
        let mut show_version = false;
        let mut version_coords = (0.0_f32, 0.0_f32);
        if logo {
            let size = (win_w.min(win_h) * 0.40).max(64.0);
            let sx = ((win_w - size) / 2.0).round();
            let sy = ((win_h - size) / 2.0).round();
            self.queue.write_buffer(
                &self.watermark_uniform_buf,
                0,
                bytemuck::bytes_of(&Uniforms {
                    mvp: rect_mvp(size, size, sx, sy, win_w, win_h),
                }),
            );
            version_coords = self.layout_version(scale, win_w, win_h);
            show_version = true;
        }

        // Donate text (empty state, below version label).
        let show_donate = logo;
        let donate_coords = donate_pos; // position computed above via layout_donate
        if show_donate {
            push_donate_hit!(donate_coords.1);
        }

        // About overlay box (A key). The donate line is rendered separately (blue, centered) in the
        // row below the head + one blank gap row.
        let mut show_about_box = false;
        let mut about_coords = (0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32);
        let mut about_donate_coords = (0.0_f32, 0.0_f32);
        if about {
            about_coords = self.layout_about(scale, win_w, win_h);
            let (bx, by, bw, bh, _, ty) = about_coords;
            self.queue.write_buffer(
                &self.about_uniform_buf,
                0,
                bytemuck::bytes_of(&Uniforms {
                    mvp: rect_mvp(bw, bh, bx, by, win_w, win_h),
                }),
            );
            show_about_box = true;
            let about_donate_top = ty
                + (Self::ABOUT_HEAD_LINES as f32 + 1.0) * self.about_line_h
                + (self.about_line_h - don_lh) / 2.0;
            about_donate_coords = (donate_left, about_donate_top);
            push_donate_hit!(about_donate_top);
        }

        // Info overlay box (I key): translucent panel, top-left, current image's metadata.
        let mut show_info_box = false;
        let mut info_coords = (0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32);
        if let Some(info_text) = info {
            info_coords = self.layout_info(info_text, scale, win_w, win_h);
            let (bx, by, bw, bh, _, _) = info_coords;
            self.queue.write_buffer(
                &self.info_uniform_buf,
                0,
                bytemuck::bytes_of(&Uniforms {
                    mvp: rect_mvp(bw, bh, bx, by, win_w, win_h),
                }),
            );
            show_info_box = true;
        }

        // Explorer panel (left side).
        let mut show_explorer = false;
        let explorer_sel_y;
        let mut explorer_sel_visible = false;
        let mut explorer_panel_w = self.explorer_computed_panel_w; // current best estimate
        if let Some(exp) = explorer {
            // 1. Rebuild TextBuffers first (Wrap::None → layout_runs gives actual line widths).
            let text = exp.panel_text();
            if self.explorer_text_cache != text
                || (self.explorer_layout_scale - scale).abs() > f32::EPSILON
            {
                self.explorer_text_cache = text;
                self.explorer_layout_scale = scale;
                self.explorer_item_bufs.clear();
                let mut make_buf = |label: &str| {
                    let mut buf = TextBuffer::new(
                        &mut self.font_system,
                        Metrics::new(EXPLORER_FONT * scale, EXPLORER_LINE_H * scale),
                    );
                    buf.set_wrap(&mut self.font_system, Wrap::None);
                    buf.set_size(&mut self.font_system, None, None);
                    buf.set_text(
                        &mut self.font_system,
                        label,
                        &Attrs::new(),
                        Shaping::Basic,
                        None,
                    );
                    buf.shape_until_scroll(&mut self.font_system, false);
                    buf
                };
                // Index 0: header.
                let hbuf = make_buf(&format!("{}/", exp.dir_label()));
                self.explorer_item_bufs.push(hbuf);
                // Index 1..n: entries.
                for entry in exp.entries() {
                    let buf = make_buf(&entry.label);
                    self.explorer_item_bufs.push(buf);
                }
                // Intrinsic width from real font metrics (no guesswork). max_lw is already in
                // physical pixels (Metrics are scaled), so the padding must be scaled too.
                let max_lw = self
                    .explorer_item_bufs
                    .iter()
                    .flat_map(|b| b.layout_runs())
                    .map(|r| r.line_w)
                    .fold(0.0_f32, f32::max);
                self.explorer_intrinsic_w = max_lw + 20.0 * scale;
            }
            // Clamp the cached intrinsic width to the live window EVERY frame: at least 180 logical
            // px, at most 70% of the window. Done outside the rebuild block so a stale window size
            // captured during a monitor switch (ScaleFactorChanged fires before Resized) self-
            // corrects on the next frame instead of freezing a wrong width.
            self.explorer_computed_panel_w =
                self.explorer_intrinsic_w.clamp(180.0 * scale, win_w * 0.70);
            explorer_panel_w = self.explorer_computed_panel_w;

            // 2. Write geometry uniforms with the actual panel width.
            // Panel background (tile_bufs[0]).
            self.queue.write_buffer(
                &self.tile_bufs[0],
                0,
                bytemuck::bytes_of(&Uniforms {
                    mvp: rect_mvp(explorer_panel_w, win_h, 0.0, 0.0, win_w, win_h),
                }),
            );
            // Header blue bar (tile_bufs[2]).
            self.queue.write_buffer(
                &self.tile_bufs[2],
                0,
                bytemuck::bytes_of(&Uniforms {
                    mvp: rect_mvp(
                        explorer_panel_w,
                        (EXPLORER_LINE_H + 8.0) * scale,
                        0.0,
                        0.0,
                        win_w,
                        win_h,
                    ),
                }),
            );
            // Selection highlight (tile_bufs[1]).
            explorer_sel_y = exp.entry_y(exp.sel) * scale;
            explorer_sel_visible = explorer_sel_y + EXPLORER_LINE_H * scale > 0.0
                && explorer_sel_y < win_h
                && !exp.entries().is_empty();
            if explorer_sel_visible {
                self.queue.write_buffer(
                    &self.tile_bufs[1],
                    0,
                    bytemuck::bytes_of(&Uniforms {
                        mvp: rect_mvp(
                            explorer_panel_w - 4.0 * scale,
                            EXPLORER_LINE_H * scale,
                            2.0 * scale,
                            explorer_sel_y,
                            win_w,
                            win_h,
                        ),
                    }),
                );
            }
            show_explorer = true;
        }

        // ---- ONE combined text prepare() call ----
        let need_text = show_overlay_box
            || show_date_box
            || show_status_box
            || show_explorer
            || show_help_box
            || show_confirm_box
            || show_version
            || show_donate
            || show_help_donate
            || show_about_box
            || show_info_box;
        let text_ok = if need_text {
            self.viewport.update(
                &self.queue,
                Resolution {
                    width: self.config.width,
                    height: self.config.height,
                },
            );
            // Build TextArea list using field-level borrows to satisfy the borrow checker.
            let (o_left, o_top, o_bx, o_by, o_bw, o_bh);
            let (s_left, s_top, s_bx, s_by, s_bw, s_bh);
            if show_overlay_box {
                let (bx, by, bw, bh, tx, ty) = overlay_coords;
                (o_left, o_top, o_bx, o_by, o_bw, o_bh) = (tx, ty, bx, by, bw, bh);
            } else {
                (o_left, o_top, o_bx, o_by, o_bw, o_bh) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
            }
            if show_status_box {
                let (bx, by, bw, bh, tx, ty) = status_coords;
                (s_left, s_top, s_bx, s_by, s_bw, s_bh) = (tx, ty, bx, by, bw, bh);
            } else {
                (s_left, s_top, s_bx, s_by, s_bw, s_bh) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
            }
            // Borrow text buffers via raw pointers before the mutable borrows below.
            // SAFETY: each pointer targets a distinct field from font_system/atlas/swash_cache.
            let p_buf = &self.text_buffer as *const TextBuffer;
            let d_buf = &self.date_text_buf as *const TextBuffer;
            let s_buf = &self.status_text_buf as *const TextBuffer;
            let h_title = &self.help_title_buf as *const TextBuffer;
            let h_lk = &self.help_l_keys_buf as *const TextBuffer;
            let h_ld = &self.help_l_desc_buf as *const TextBuffer;
            let h_rk = &self.help_r_keys_buf as *const TextBuffer;
            let h_rd = &self.help_r_desc_buf as *const TextBuffer;
            let h_fk = &self.help_f_keys_buf as *const TextBuffer;
            let h_fd = &self.help_f_desc_buf as *const TextBuffer;
            let c_buf = &self.confirm_text_buf as *const TextBuffer;
            let v_buf = &self.version_text_buf as *const TextBuffer;
            let donate_ptr = &self.donate_text_buf as *const TextBuffer;
            let about_ptr = &self.about_text_buf as *const TextBuffer;
            let info_ptr = &self.info_text_buf as *const TextBuffer;
            // explorer_item_bufs is a Vec; get raw pointers to each element.
            let e_ptrs: Vec<*const TextBuffer> = self
                .explorer_item_bufs
                .iter()
                .map(|b| b as *const _)
                .collect();
            let mut areas: Vec<TextArea> = Vec::with_capacity(
                show_overlay_box as usize + show_status_box as usize + e_ptrs.len(),
            );
            if show_overlay_box {
                areas.push(TextArea {
                    buffer: unsafe { &*p_buf },
                    left: o_left,
                    top: o_top,
                    scale: 1.0,
                    bounds: TextBounds {
                        left: o_bx as i32,
                        top: o_by as i32,
                        right: (o_bx + o_bw) as i32,
                        bottom: (o_by + o_bh) as i32,
                    },
                    default_color: Color::rgb(240, 240, 240),
                    custom_glyphs: &[],
                });
            }
            if show_status_box {
                areas.push(TextArea {
                    buffer: unsafe { &*s_buf },
                    left: s_left,
                    top: s_top,
                    scale: 1.0,
                    bounds: TextBounds {
                        left: s_bx as i32,
                        top: s_by as i32,
                        right: (s_bx + s_bw) as i32,
                        bottom: (s_by + s_bh) as i32,
                    },
                    default_color: Color::rgb(255, 255, 255),
                    custom_glyphs: &[],
                });
            }
            if show_date_box {
                let (bx, by, bw, bh, tx, ty) = date_coords;
                areas.push(TextArea {
                    buffer: unsafe { &*d_buf },
                    left: tx,
                    top: ty,
                    scale: 1.0,
                    bounds: TextBounds {
                        left: bx as i32,
                        top: by as i32,
                        right: (bx + bw) as i32,
                        bottom: (by + bh) as i32,
                    },
                    default_color: Color::rgb(200, 200, 200),
                    custom_glyphs: &[],
                });
            }
            if let Some(hl) = &help_layout {
                let bounds = TextBounds {
                    left: hl.bx as i32,
                    top: hl.by as i32,
                    right: (hl.bx + hl.bw) as i32,
                    bottom: (hl.by + hl.bh) as i32,
                };
                let bright = Color::rgb(245, 245, 250); // title + keys
                let dim = Color::rgb(180, 185, 195); // descriptions
                // Helper to push a (buffer, left, top, color) text area within the help box bounds.
                let mut push = |ptr: *const TextBuffer, left: f32, top: f32, color: Color| {
                    areas.push(TextArea {
                        buffer: unsafe { &*ptr },
                        left,
                        top,
                        scale: 1.0,
                        bounds,
                        default_color: color,
                        custom_glyphs: &[],
                    });
                };
                push(h_title, hl.title_x, hl.title_y, bright);
                push(h_lk, hl.l_keys_x, hl.col_top, bright);
                push(h_ld, hl.l_desc_x, hl.col_top, dim);
                push(h_rk, hl.r_keys_x, hl.col_top, bright);
                push(h_rd, hl.r_desc_x, hl.col_top, dim);
                push(h_fk, hl.f_keys_x, hl.footer_top, bright);
                push(h_fd, hl.f_desc_x, hl.footer_top, dim);
            }
            if show_confirm_box {
                let (bx, by, bw, bh, tx, ty) = confirm_coords;
                areas.push(TextArea {
                    buffer: unsafe { &*c_buf },
                    left: tx,
                    top: ty,
                    scale: 1.0,
                    bounds: TextBounds {
                        left: bx as i32,
                        top: by as i32,
                        right: (bx + bw) as i32,
                        bottom: (by + bh) as i32,
                    },
                    default_color: Color::rgb(240, 240, 245),
                    custom_glyphs: &[],
                });
            }
            if show_version {
                let (tx, ty) = version_coords;
                areas.push(TextArea {
                    buffer: unsafe { &*v_buf },
                    left: tx,
                    top: ty,
                    scale: 1.0,
                    bounds: TextBounds {
                        left: 0,
                        top: 0,
                        right: win_w as i32,
                        bottom: win_h as i32,
                    },
                    default_color: Color::rgb(130, 130, 140),
                    custom_glyphs: &[],
                });
            }
            if show_explorer {
                // Index 0 = header (white), 1..n = entries with per-kind colors.
                // Colors: parent/dir = soft blue, current image = amber, other images = light grey.
                let panel_right = explorer_panel_w as i32;
                let explorer = explorer.unwrap(); // show_explorer is true, so explorer is Some
                for (idx, ptr) in e_ptrs.iter().enumerate() {
                    let color = if idx == 0 {
                        // Header
                        Color::rgb(255, 255, 255)
                    } else {
                        let entry = &explorer.entries()[idx - 1];
                        match entry.kind {
                            glanvu_viewer_core::explorer::EntryKind::Parent
                            | glanvu_viewer_core::explorer::EntryKind::Dir => {
                                Color::rgb(100, 165, 255)
                            }
                            glanvu_viewer_core::explorer::EntryKind::Image => {
                                if entry.label.starts_with("> ") {
                                    Color::rgb(255, 220, 80) // current image: amber
                                } else {
                                    Color::rgb(200, 200, 205) // other images: light grey
                                }
                            }
                        }
                    };
                    // y position: 0 = header at top, 1..n = entries below EXPLORER_LINE_H+8.
                    // All coordinates are in physical pixels (Metrics are scaled).
                    let top = if idx == 0 {
                        8.0 * scale // header padding within the header bar
                    } else {
                        explorer.entry_y(idx - 1) * scale
                    };
                    if idx > 0 && (top + EXPLORER_LINE_H * scale < 0.0 || top > win_h) {
                        continue; // not visible
                    }
                    areas.push(TextArea {
                        buffer: unsafe { &**ptr },
                        left: 8.0 * scale,
                        top,
                        scale: 1.0,
                        bounds: TextBounds {
                            left: 0,
                            top: if idx == 0 {
                                0
                            } else {
                                ((EXPLORER_LINE_H + 8.0) * scale) as i32
                            },
                            right: panel_right,
                            bottom: win_h as i32,
                        },
                        default_color: color,
                        custom_glyphs: &[],
                    });
                }
            }
            if show_donate {
                let (tx, ty) = donate_coords;
                areas.push(TextArea {
                    buffer: unsafe { &*donate_ptr },
                    left: tx,
                    top: ty,
                    scale: 1.0,
                    bounds: TextBounds {
                        left: 0,
                        top: 0,
                        right: win_w as i32,
                        bottom: win_h as i32,
                    },
                    default_color: Color::rgb(130, 190, 255),
                    custom_glyphs: &[],
                });
            }
            if show_help_donate {
                let (tx, ty) = help_donate_coords;
                areas.push(TextArea {
                    buffer: unsafe { &*donate_ptr },
                    left: tx,
                    top: ty,
                    scale: 1.0,
                    bounds: TextBounds {
                        left: 0,
                        top: 0,
                        right: win_w as i32,
                        bottom: win_h as i32,
                    },
                    default_color: Color::rgb(160, 205, 255),
                    custom_glyphs: &[],
                });
            }
            if show_about_box {
                let (_, _, _, _, tx, ty) = about_coords;
                areas.push(TextArea {
                    buffer: unsafe { &*about_ptr },
                    left: tx,
                    top: ty,
                    scale: 1.0,
                    bounds: TextBounds {
                        left: 0,
                        top: 0,
                        right: win_w as i32,
                        bottom: win_h as i32,
                    },
                    default_color: Color::rgb(240, 240, 245),
                    custom_glyphs: &[],
                });
                // Donate line: blue, centered, in the row below the About body.
                let (dtx, dty) = about_donate_coords;
                areas.push(TextArea {
                    buffer: unsafe { &*donate_ptr },
                    left: dtx,
                    top: dty,
                    scale: 1.0,
                    bounds: TextBounds {
                        left: 0,
                        top: 0,
                        right: win_w as i32,
                        bottom: win_h as i32,
                    },
                    default_color: Color::rgb(130, 190, 255),
                    custom_glyphs: &[],
                });
            }
            if show_info_box {
                let (bx, by, bw, bh, tx, ty) = info_coords;
                areas.push(TextArea {
                    buffer: unsafe { &*info_ptr },
                    left: tx,
                    top: ty,
                    scale: 1.0,
                    bounds: TextBounds {
                        left: bx as i32,
                        top: by as i32,
                        right: (bx + bw) as i32,
                        bottom: (by + bh) as i32,
                    },
                    default_color: Color::rgb(235, 235, 240),
                    custom_glyphs: &[],
                });
            }
            self.text_renderer
                .prepare(
                    &self.device,
                    &self.queue,
                    &mut self.font_system,
                    &mut self.atlas,
                    &self.viewport,
                    areas,
                    &mut self.swash_cache,
                )
                .is_ok()
        } else {
            false
        };

        // ---- Acquire surface and record draw commands ----
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return false;
            }
            _ => return false,
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("glanvu encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("glanvu pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.07,
                            g: 0.07,
                            b: 0.08,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_vertex_buffer(0, self.vertex_buf.slice(..));
            pass.set_index_buffer(self.index_buf.slice(..), wgpu::IndexFormat::Uint16);

            // 1. Image (whole-image base layer).
            pass.set_bind_group(0, &self.uniform_bind, &[]);
            pass.set_bind_group(1, &self.texture_bind, &[]);
            pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);

            // 1a. SVG deep-zoom tiles composited on top of the base for the visible region.
            for (i, tile) in svg_tiles.iter().take(svg_tile_count).enumerate() {
                if let Some((tex_bind, _)) = self.svg_tile_tex.get(&tile.key) {
                    pass.set_bind_group(0, &self.svg_tile_mvp_binds[i], &[]);
                    pass.set_bind_group(1, tex_bind, &[]);
                    pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
                }
            }

            // 1b. Watermark logo (empty state only).
            if logo {
                pass.set_bind_group(0, &self.watermark_uniform_bind, &[]);
                pass.set_bind_group(1, &self.watermark_bind, &[]);
                pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
            }

            // 2. Explorer panel bg + header bar + selection (drawn on top of image, under text).
            if show_explorer {
                // Panel background.
                pass.set_bind_group(0, &self.tile_binds[0], &[]);
                pass.set_bind_group(1, &self.explorer_bg_bind, &[]);
                pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
                // Header blue bar.
                pass.set_bind_group(0, &self.tile_binds[2], &[]);
                pass.set_bind_group(1, &self.explorer_header_bind, &[]);
                pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
                // Selection highlight.
                if explorer_sel_visible {
                    pass.set_bind_group(0, &self.tile_binds[1], &[]);
                    pass.set_bind_group(1, &self.sel_bind, &[]);
                    pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
                }
            }

            // 3. Path overlay box (bottom-left) + date overlay box (bottom-right).
            if show_overlay_box {
                pass.set_bind_group(0, &self.box_uniform_bind, &[]);
                pass.set_bind_group(1, &self.box_texture_bind, &[]);
                pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
            }
            if show_date_box {
                pass.set_bind_group(0, &self.date_uniform_bind, &[]);
                pass.set_bind_group(1, &self.box_texture_bind, &[]);
                pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
            }

            // 4. Status box.
            if show_status_box {
                pass.set_bind_group(0, &self.status_uniform_bind, &[]);
                pass.set_bind_group(1, &self.box_texture_bind, &[]);
                pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
            }

            // 5. Help box.
            if show_help_box {
                pass.set_bind_group(0, &self.help_uniform_bind, &[]);
                pass.set_bind_group(1, &self.explorer_bg_bind, &[]); // near-opaque dark bg
                pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
            }

            // 6. Confirmation overlay box (D key).
            if show_confirm_box {
                pass.set_bind_group(0, &self.confirm_uniform_bind, &[]);
                pass.set_bind_group(1, &self.explorer_bg_bind, &[]); // same dark bg
                pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
            }

            // 6b. About overlay box (A key).
            if show_about_box {
                pass.set_bind_group(0, &self.about_uniform_bind, &[]);
                pass.set_bind_group(1, &self.explorer_bg_bind, &[]); // same dark bg
                pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
            }

            // 6c. Info overlay box (I key) — top-left, same translucent dark bg as help/about.
            if show_info_box {
                pass.set_bind_group(0, &self.info_uniform_bind, &[]);
                pass.set_bind_group(1, &self.explorer_bg_bind, &[]);
                pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
            }

            // 7. ONE glyphon render call for ALL text (explorer + overlay + status + help + confirm).
            if text_ok {
                let _ = self
                    .text_renderer
                    .render(&self.atlas, &self.viewport, &mut pass);
            }
        }
        self.queue.submit(std::iter::once(encoder.finish()));
        frame.present();
        if text_ok {
            self.atlas.trim();
        }
        true
    }

    // --- Grid renderer ------------------------------------------------------

    /// Upload a thumbnail to the GPU and store the bind group keyed by path.
    pub fn upload_thumb(&mut self, path: PathBuf, img: &DecodedImage) {
        let bind = build_texture_bind(
            &self.device,
            &self.queue,
            &self.texture_layout,
            &self.sampler,
            self.config.format.is_srgb(),
            img,
        );
        self.thumb_binds.insert(path, bind);
    }

    // --- SVG deep-zoom viewport texture -------------------------------------

    /// Upload the freshly-rendered viewport texture, keyed by its request generation.
    fn upload_svg_tile(&mut self, key: (u64, i32, i32), img: &DecodedImage, gen: u64) {
        let bind = build_texture_bind(
            &self.device,
            &self.queue,
            &self.texture_layout,
            &self.sampler,
            self.config.format.is_srgb(),
            img,
        );
        self.svg_tile_tex.insert(key, (bind, gen));
    }

    /// Drop the cached viewport texture (on image change or when zooming back out to fit).
    fn clear_svg_tiles(&mut self) {
        self.svg_tile_tex.clear();
    }

    /// Render the thumbnail grid. Returns whether a frame was presented.
    // Raw-pointer borrow of confirm_text_buf during text prepare, mirroring `render`.
    #[allow(unsafe_code)]
    pub fn render_grid(
        &mut self,
        paths: &[PathBuf],
        grid: &GridState,
        confirm: Option<&str>,
        status: Option<&str>,
        find_bar: Option<&str>,
        scale: f32,
    ) -> bool {
        let (win_w, win_h) = (self.config.width as f32, self.config.height as f32);
        let n = paths.len();
        // When the find filter is active a search bar occupies the top of the grid; push the tiles
        // down by its height (plus a gap) so the first row is not hidden behind it.
        let top_inset = if find_bar.is_some() {
            find_bar_height(scale) + GAP
        } else {
            0.0
        };

        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return false;
            }
            _ => return false,
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        // Collect visible tiles and write all transforms before the render pass. The cull accounts
        // for `top_inset` (the find bar) so tiles at the top/bottom edges are not wrongly dropped.
        let visible: Vec<usize> = (0..n)
            .filter(|&i| {
                let (_, y) = grid.cell_origin(i, win_w);
                let y = y + top_inset;
                y + CELL_H > 0.0 && y < win_h
            })
            .collect();

        let mut slot = 0usize;
        // For each visible tile we write up to 3 transforms (bg, sel ring, thumb/placeholder).
        // Slot assignment per tile: bg=slot, sel=slot+1 (only if selected), thumb=slot+2.
        struct TileSlots {
            bg: usize,
            sel: Option<usize>,
            content: usize,
        }
        let mut tile_slots: Vec<TileSlots> = Vec::with_capacity(visible.len());

        for &i in &visible {
            let (cx, cy) = grid.cell_origin(i, win_w);
            let cy = cy + top_inset; // shift below the find search bar (0.0 when not filtering)
            // bg quad
            if slot < TILE_POOL {
                self.queue.write_buffer(
                    &self.tile_bufs[slot],
                    0,
                    bytemuck::bytes_of(&Uniforms {
                        mvp: rect_mvp(CELL_W, CELL_H, cx, cy, win_w, win_h),
                    }),
                );
            }
            let bg_slot = slot;
            slot += 1;
            // selection ring: drawn for every selected tile; the cursor gets a thicker ring
            // so it stays distinguishable within a multi-selection.
            let is_cursor = i == grid.sel;
            let ring = is_cursor || grid.selected.contains(&i);
            let sel_slot = if ring && slot < TILE_POOL {
                let out = if is_cursor { SEL_OUTSET * 2.5 } else { SEL_OUTSET };
                self.queue.write_buffer(
                    &self.tile_bufs[slot],
                    0,
                    bytemuck::bytes_of(&Uniforms {
                        mvp: rect_mvp(
                            CELL_W + out * 2.0,
                            CELL_H + out * 2.0,
                            cx - out,
                            cy - out,
                            win_w,
                            win_h,
                        ),
                    }),
                );
                let s = slot;
                slot += 1;
                Some(s)
            } else {
                None
            };
            // thumbnail or placeholder quad (centered within cell)
            let content_slot = slot;
            if slot < TILE_POOL {
                let path = &paths[i];
                let (tw, th) = self
                    .thumb_binds
                    .get(path)
                    .map(|_| (THUMB_W as f32, THUMB_H as f32))
                    .unwrap_or((CELL_W, CELL_H));
                let tx = cx + (CELL_W - tw) / 2.0;
                let ty = cy + (CELL_H - th) / 2.0;
                self.queue.write_buffer(
                    &self.tile_bufs[slot],
                    0,
                    bytemuck::bytes_of(&Uniforms {
                        mvp: rect_mvp(tw, th, tx, ty, win_w, win_h),
                    }),
                );
                slot += 1;
            }
            tile_slots.push(TileSlots {
                bg: bg_slot,
                sel: sel_slot,
                content: content_slot,
            });
        }

        // Rubber-band rectangle quad (drawn translucent over the tiles).
        let marquee_slot = match grid.marquee {
            Some((x0, y0, x1, y1)) if slot < TILE_POOL => {
                let (lx, ty) = (x0.min(x1), y0.min(y1));
                let (mw, mh) = ((x0 - x1).abs(), (y0 - y1).abs());
                self.queue.write_buffer(
                    &self.tile_bufs[slot],
                    0,
                    bytemuck::bytes_of(&Uniforms {
                        mvp: rect_mvp(mw, mh, lx, ty, win_w, win_h),
                    }),
                );
                let s = slot;
                slot += 1;
                Some(s)
            }
            _ => None,
        };
        let _ = slot; // silence unused-assignment after the last allocation

        // Overlays drawn on top of the grid: the delete-confirmation modal and the transient
        // status toast (e.g. "Sorted by name"). Both are laid out and prepared before the pass.
        // The raw-pointer trick decouples the text-buffer borrows from the `&mut self` borrows
        // taken by `layout_*`/`prepare` (distinct fields, deref'd only during prepare below).
        self.viewport.update(
            &self.queue,
            Resolution {
                width: self.config.width,
                height: self.config.height,
            },
        );
        let mut areas: Vec<TextArea> = Vec::new();

        let mut show_confirm = false;
        if let Some(text) = confirm {
            let (bx, by, bw, bh, tx, ty) = self.layout_confirm(text, scale, win_w, win_h);
            self.queue.write_buffer(
                &self.confirm_uniform_buf,
                0,
                bytemuck::bytes_of(&Uniforms {
                    mvp: rect_mvp(bw, bh, bx, by, win_w, win_h),
                }),
            );
            let c_buf = &self.confirm_text_buf as *const TextBuffer;
            areas.push(TextArea {
                buffer: unsafe { &*c_buf },
                left: tx,
                top: ty,
                scale: 1.0,
                bounds: TextBounds {
                    left: bx as i32,
                    top: by as i32,
                    right: (bx + bw) as i32,
                    bottom: (by + bh) as i32,
                },
                default_color: Color::rgb(240, 240, 245),
                custom_glyphs: &[],
            });
            show_confirm = true;
        }

        let mut show_status = false;
        if let Some(text) = status {
            let (bx, by, bw, bh, tx, ty) = self.layout_status(text, scale, win_w, win_h);
            self.queue.write_buffer(
                &self.status_uniform_buf,
                0,
                bytemuck::bytes_of(&Uniforms {
                    mvp: rect_mvp(bw, bh, bx, by, win_w, win_h),
                }),
            );
            let s_buf = &self.status_text_buf as *const TextBuffer;
            areas.push(TextArea {
                buffer: unsafe { &*s_buf },
                left: tx,
                top: ty,
                scale: 1.0,
                bounds: TextBounds {
                    left: bx as i32,
                    top: by as i32,
                    right: (bx + bw) as i32,
                    bottom: (by + bh) as i32,
                },
                default_color: Color::rgb(240, 240, 245),
                custom_glyphs: &[],
            });
            show_status = true;
        }

        let mut show_find_bar = false;
        if let Some(text) = find_bar {
            let (bx, by, bw, bh, tx, ty) = self.layout_find_bar(text, scale, win_w);
            self.queue.write_buffer(
                &self.findbar_uniform_buf,
                0,
                bytemuck::bytes_of(&Uniforms {
                    mvp: rect_mvp(bw, bh, bx, by, win_w, win_h),
                }),
            );
            let f_buf = &self.findbar_text_buf as *const TextBuffer;
            areas.push(TextArea {
                buffer: unsafe { &*f_buf },
                left: tx,
                top: ty,
                scale: 1.0,
                bounds: TextBounds {
                    left: bx as i32,
                    top: by as i32,
                    right: (bx + bw) as i32,
                    bottom: (by + bh) as i32,
                },
                default_color: Color::rgb(240, 240, 245),
                custom_glyphs: &[],
            });
            show_find_bar = true;
        }

        if !areas.is_empty() {
            let _ = self.text_renderer.prepare(
                &self.device,
                &self.queue,
                &mut self.font_system,
                &mut self.atlas,
                &self.viewport,
                areas,
                &mut self.swash_cache,
            );
        }

        // Single render pass: clear + draw all tiles.
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("glanvu grid encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("glanvu grid pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.10,
                            g: 0.10,
                            b: 0.11,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_vertex_buffer(0, self.vertex_buf.slice(..));
            pass.set_index_buffer(self.index_buf.slice(..), wgpu::IndexFormat::Uint16);

            for (idx, &i) in visible.iter().enumerate() {
                let ts = &tile_slots[idx];
                if ts.bg >= TILE_POOL {
                    continue;
                }

                // 1. Cell background.
                pass.set_bind_group(0, &self.tile_binds[ts.bg], &[]);
                pass.set_bind_group(1, &self.cell_bg_bind, &[]);
                pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);

                // 2. Selection ring (selected tile only).
                if let Some(ss) = ts.sel {
                    pass.set_bind_group(0, &self.tile_binds[ss], &[]);
                    pass.set_bind_group(1, &self.sel_bind, &[]);
                    pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
                }

                // 3. Thumbnail or placeholder.
                if ts.content < TILE_POOL {
                    let path = &paths[i];
                    let tex = self.thumb_binds.get(path).unwrap_or(&self.placeholder_bind);
                    pass.set_bind_group(0, &self.tile_binds[ts.content], &[]);
                    pass.set_bind_group(1, tex, &[]);
                    pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
                }
            }

            // Rubber-band rectangle (translucent fill) over the tiles.
            if let Some(ms) = marquee_slot {
                pass.set_bind_group(0, &self.tile_binds[ms], &[]);
                pass.set_bind_group(1, &self.marquee_bind, &[]);
                pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
            }

            // Overlay boxes (delete-confirmation modal + status toast) and their text, on top
            // of the grid.
            if show_confirm {
                pass.set_bind_group(0, &self.confirm_uniform_bind, &[]);
                pass.set_bind_group(1, &self.explorer_bg_bind, &[]); // translucent dark bg
                pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
            }
            if show_status {
                pass.set_bind_group(0, &self.status_uniform_bind, &[]);
                pass.set_bind_group(1, &self.box_texture_bind, &[]); // semi-transparent box bg
                pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
            }
            // Find-filter search bar (top strip), drawn last so it sits above any tile under it.
            if show_find_bar {
                pass.set_bind_group(0, &self.findbar_uniform_bind, &[]);
                pass.set_bind_group(1, &self.explorer_bg_bind, &[]); // near-opaque dark bg
                pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
            }
            if show_confirm || show_status || show_find_bar {
                let _ = self
                    .text_renderer
                    .render(&self.atlas, &self.viewport, &mut pass);
            }
        }
        self.queue.submit(std::iter::once(encoder.finish()));
        frame.present();
        if show_confirm || show_find_bar {
            self.atlas.trim();
        }
        true
    }
}

// ---------------------------------------------------------------------------
// App (winit event handler)
// ---------------------------------------------------------------------------

/// Viewer display mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    /// Waiting for the user to drop or pick a file (launched without arguments).
    Empty,
    /// Single-image view (Phase 1 default).
    Single,
    /// Thumbnail grid (Phase 2).
    Grid,
}

struct App {
    start: Instant,
    nav: FolderNav,
    img_size: (u32, u32),
    state: ViewState,
    mode: ViewMode,
    grid: GridState,
    thumbs: ThumbnailCache,
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    first_frame: bool,
    cursor: PhysicalPosition<f64>,
    dragging: bool,
    overlay_until: Option<Instant>,
    /// When `Some(t)`, the slideshow advances at time `t`.
    slideshow_next: Option<Instant>,
    slideshow_interval: Duration,
    /// Short centered status message (e.g. "Slideshow on") and when to hide it.
    status_text: String,
    status_until: Option<Instant>,
    /// Directory explorer panel; `Some` when open.
    explorer: Option<ExplorerState>,
    /// Whether the centered keyboard-help overlay is shown.
    help_visible: bool,
    /// Whether the About overlay is shown (A key).
    about_visible: bool,
    /// Whether the info panel is shown (I key). Toggled; persists across navigation.
    info_visible: bool,
    /// Current keyboard modifier state (for grid range/toggle selection).
    modifiers: ModifiersState,
    /// In-progress grid drag (rubber-band selection), `None` when the button is up.
    grid_drag: Option<GridDrag>,
    /// Pending default-app confirmation. `Some(true)` = set; `Some(false)` = unset.
    confirm_assoc: Option<bool>,
    /// Pending delete confirmation: the image(s) to move to Trash (Delete/Backspace key).
    /// One path in single view; the grid selection (1..n) in grid view.
    confirm_delete: Option<Vec<PathBuf>>,
    /// Inline rename editor (F2): the editable filename stem. `Some` while typing the new name.
    rename: Option<TextInput>,
    /// Pending rename confirmation: `(old_path, new_path)` awaiting Enter.
    confirm_rename: Option<(PathBuf, PathBuf)>,
    /// Quick-open search (`/` or Ctrl/Cmd+F). `Some` while the find modal is open.
    find: Option<FindState>,
    /// Last grid click: (timestamp, cell_index) for double-click detection.
    last_grid_click: Option<(Instant, usize)>,
    /// Current sort order.
    sort_mode: glanvu_viewer_core::nav::SortMode,
    /// Cached mtime string for the date overlay (updated when the path overlay fires).
    date_text: String,
    /// Whether the current image is SVG — gates the crisp re-raster-on-settle behavior (D11).
    current_is_svg: bool,
    /// When `Some(t)`, the current SVG is re-rasterized at its new effective on-screen resolution
    /// at time `t`. Reset on every zoom/fit/window-resize change while viewing an SVG.
    svg_rerender_at: Option<Instant>,
    /// Generation of the most recently *issued* SVG re-raster request. Bumped each time one is
    /// sent to the background worker so a late reply from a superseded request can be dropped.
    svg_rerender_gen: u64,
    /// Whether a background SVG re-raster is currently running (drives the fast poll interval).
    svg_rerender_inflight: bool,
    /// Mailbox for the SVG re-raster worker thread — decoding/rasterizing runs off the UI thread
    /// so a large or complex SVG doesn't stall zoom/redraw, and a single-slot mailbox (not a
    /// queue) means switching images or zooming again never leaves a backlog of stale jobs
    /// running behind the scenes (see the performance note in the decision log, D11 follow-up).
    svg_rerender_mailbox: Arc<SvgRerenderMailbox>,
    svg_rerender_rx: Receiver<SvgRerenderResult>,
    // ── SVG deep-zoom: single viewport render ──
    //
    // When zoomed in past fit, render ONLY the visible region (+ a pad margin) at screen resolution
    // in a single pass, composited over the fit-resolution base. One render per settle — NOT a tile
    // grid: resvg re-renders the whole tree (all filters/gradients) per call regardless of clip, so
    // a grid would multiply that cost by the tile count (catastrophic for filter-heavy SVGs).
    // Panning within the padded region is free; panning beyond it (or zooming) re-renders.
    /// Parsed current SVG (kept in memory so the viewport renders without re-parsing). `None` for
    /// raster images.
    svg_doc: Option<Arc<glanvu_core::SvgDocument>>,
    /// Image-space region + scale of the currently CACHED viewport texture (in `gpu.svg_tile_tex`,
    /// keyed by `svg_vp_key`). `None` until the first viewport render arrives.
    svg_vp_region: Option<(f32, f32, f32, f32)>,
    svg_vp_scale: f32,
    svg_vp_key: (u64, i32, i32),
    /// Region + scale of the in-flight viewport render (matched to its worker reply by generation).
    svg_vp_pending: Option<(f32, f32, f32, f32, f32)>,
    /// Generation of the most recent viewport render request (worker replies carry it in `epoch`).
    svg_vp_gen: u64,
    /// Last on-screen scale seen, and when it last changed — a scale change re-renders only after
    /// settling (debounced); panning (same scale) re-renders immediately when coverage is lost.
    svg_vp_last_scale: f32,
    svg_vp_settled_at: Instant,
    tile_queue: Arc<TileQueue>,
    tile_rx: Receiver<TileResult>,
}

/// Static keyboard cheatsheet shown by the help overlay (H).
/// Text for the D-key confirmation overlay. `set` = true → set as default; false → unset.
fn confirm_overlay_text(set: bool) -> &'static str {
    if set {
        "Set Glanvu as default?\n\njpg  jpeg  png  gif\nbmp  tif   tiff webp  svg\n\nEnter = confirm   Esc = cancel"
    } else {
        "Restore previous defaults?\n\nGlanvu won't open images\nby default anymore.\n\nEnter = confirm   Esc = cancel"
    }
}

const HELP_TITLE: &str = "Glanvu — keyboard shortcuts";

/// A row of the help cheatsheet: either a section header or a `keys → description` pair.
enum HelpRow {
    /// Section title, rendered in an accent colour with a blank line before it.
    Section(&'static str),
    /// `(keys, description)` shown across the two columns.
    Keys(&'static str, &'static str),
}

use HelpRow::{Keys, Section};

/// The help overlay lays out three blocks: a left column ([`HELP_LEFT`]) and right column
/// ([`HELP_RIGHT`]) of sections side by side, with [`HELP_FOOTER`] centered below them. The layout
/// inserts a blank line before each section automatically.
const HELP_LEFT: &[HelpRow] = &[
    Section("Navigate"),
    Keys("Arrows", "previous · next image"),
    Keys("Home / End", "first · last image"),
    Keys("F  ·  /", "find by name"),
    Keys("Enter", "directory explorer"),
    Keys("Tab / G", "thumbnail grid"),
    Section("View"),
    Keys("+ / − / wheel", "zoom in · out"),
    Keys("drag", "pan"),
    Keys("0  ·  1", "fit  ·  actual size (1:1)"),
    Keys("T", "turn (rotate) 90°"),
    Keys("Space / F11", "fullscreen"),
    Keys("S", "slideshow"),
];

const HELP_RIGHT: &[HelpRow] = &[
    Section("Organize"),
    Keys("O", "sort order (name / date)"),
    Keys("I", "image info"),
    Keys("R", "rename"),
    Keys("C  ·  Shift+C", "copy image · copy path"),
    Keys("F5", "refresh from disk"),
    Keys("Del / Backspace", "move to Trash"),
    Section("Grid selection"),
    Keys("Shift+click / arrows", "select range"),
    Keys("Ctrl/⌘+click · Space", "toggle one"),
    Keys("Ctrl/⌘+A · drag", "select all · rubber-band"),
];

const HELP_FOOTER: &[HelpRow] = &[
    Section("App"),
    Keys("D  ·  U", "set · restore default app"),
    Keys("A", "about Glanvu"),
    Keys("H / ?", "show / hide this help"),
    Keys("Esc / Q", "close · quit"),
];

fn mtime_string(path: &std::path::PathBuf) -> Option<String> {
    let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    let dt = chrono::DateTime::<chrono::Local>::from(mtime);
    Some(dt.format("%Y-%m-%d  %H:%M").to_string())
}

/// The file name of `p` as a string, or "(unknown)" — used in overlay/status text.
fn file_name_str(p: &Path) -> &str {
    p.file_name().and_then(|n| n.to_str()).unwrap_or("(unknown)")
}

/// Human-readable byte count (e.g. "1.4 MB"). Used by the info overlay.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

impl App {
    fn redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn flash_overlay(&mut self) {
        self.overlay_until = Some(Instant::now() + OVERLAY_DURATION);
        self.date_text = self
            .nav
            .current_path()
            .and_then(mtime_string)
            .unwrap_or_default();
    }

    /// Zoom by `factor` (`zoom *= factor`), keeping the point under the cursor fixed on screen —
    /// mouse-wheel zoom should anchor to the pointer, not the image center. `pan` lives in the
    /// same center-origin, y-up space `mvp()` applies it in (see `CursorMoved`'s drag-pan, which
    /// uses this same convention); the fixed-point adjustment is the standard
    /// `pan' = p + (pan - p) * factor` (rotation and `fit`'s base scale cancel out of the
    /// derivation, since `pan` is applied after both in the transform chain).
    fn zoom_at_cursor(&mut self, factor: f32) {
        let (win_w, win_h) = self.win_size();
        let px = self.cursor.x as f32 - win_w / 2.0;
        let py = win_h / 2.0 - self.cursor.y as f32;
        self.state.zoom *= factor;
        self.state.pan.0 = px + (self.state.pan.0 - px) * factor;
        self.state.pan.1 = py + (self.state.pan.1 - py) * factor;
        self.schedule_svg_rerender();
    }

    /// Schedule a crisp SVG re-raster once zoom/fit/window-size settles, if the current image is
    /// SVG. No-op for raster formats — the GPU already scales those textures smoothly. Called
    /// from every zoom/fit/resize mutation site; see `about_to_wait` for the debounced re-raster.
    fn schedule_svg_rerender(&mut self) {
        if self.current_is_svg {
            self.svg_rerender_at = Some(Instant::now() + SVG_RERENDER_DEBOUNCE);
        }
    }

    /// (Re)load the parsed SVG document for the current image and invalidate the tile cache. Called
    /// whenever the current image changes. `None` for raster images.
    fn refresh_svg_doc(&mut self) {
        self.svg_doc = if self.current_is_svg {
            self.nav
                .current_path()
                .and_then(|p| glanvu_core::SvgDocument::load(p).ok())
                .map(Arc::new)
        } else {
            None
        };
        // New image → the cached viewport render is meaningless. Reset viewport state.
        self.svg_vp_region = None;
        self.svg_vp_pending = None;
        self.svg_vp_last_scale = 0.0;
        self.tile_queue.clear();
        if let Some(g) = self.gpu.as_mut() {
            g.clear_svg_tiles();
        }
    }

    /// The SVG deep-zoom viewport quad to composite this frame: a draw for the currently CACHED
    /// viewport texture (if any), placed at its image-space region over the base. Pure/read-only —
    /// the scheduling of new renders lives in `plan_svg_viewport` (timer-driven from
    /// `about_to_wait`), so it fires even when the app is idle after a zoom settles.
    fn svg_tile_draws(&self, win: (f32, f32)) -> Vec<SvgTileDraw> {
        if !self.current_is_svg {
            return Vec::new();
        }
        match self.svg_vp_region {
            Some(region) => vec![SvgTileDraw {
                mvp: tile_mvp(self.img_size, region, win, &self.state),
                key: self.svg_vp_key,
            }],
            None => Vec::new(),
        }
    }

    /// Decide whether to (re)render the SVG deep-zoom viewport, and enqueue it if so. Runs every
    /// `about_to_wait` (not just on draw) so a render fires after the zoom-settle debounce even
    /// while idle. Returns a wakeup deadline while debouncing so the loop re-checks after settle.
    ///
    /// One render per settle — never a tile grid: resvg re-renders the whole tree per call
    /// regardless of clip, so tiling would multiply that cost by the tile count.
    fn plan_svg_viewport(&mut self, win: (f32, f32)) -> Option<Instant> {
        if !self.current_is_svg {
            self.discard_svg_viewport();
            return None;
        }
        let Some(doc) = self.svg_doc.clone() else {
            self.discard_svg_viewport();
            return None;
        };
        let img = self.img_size;
        let s = image_scale(img, win, &self.state);

        // Active only when zoomed in past fit — otherwise the fit-resolution base covers everything.
        let fit = fit_scale(img, win, self.state.quarter_turns);
        if s <= fit * 1.05 {
            self.discard_svg_viewport();
            self.svg_vp_last_scale = 0.0;
            return None;
        }

        // Debounce re-render on scale change (a zoom gesture shows the stretched base meanwhile);
        // pan keeps the same scale so its coverage-driven re-render fires immediately.
        if (self.svg_vp_last_scale - s).abs() > f32::EPSILON {
            self.svg_vp_last_scale = s;
            self.svg_vp_settled_at = Instant::now();
        }
        let elapsed = self.svg_vp_settled_at.elapsed();
        if elapsed < SVG_RERENDER_DEBOUNCE {
            // Wake up once the scale settles so this runs again to enqueue.
            return Some(self.svg_vp_settled_at + SVG_RERENDER_DEBOUNCE);
        }

        let vis = visible_image_rect(img, win, &self.state);
        let cached_ok = match self.svg_vp_region {
            Some(r) => (self.svg_vp_scale - s).abs() <= f32::EPSILON && region_covers(r, vis),
            None => false,
        };
        let pending_ok = match self.svg_vp_pending {
            Some((x, y, w, h, ps)) => {
                (ps - s).abs() <= f32::EPSILON && region_covers((x, y, w, h), vis)
            }
            None => false,
        };
        if !cached_ok && !pending_ok {
            let target = pad_region(vis, SVG_VP_PAD, img);
            let max_dim = self
                .gpu
                .as_ref()
                .map(|g| g.device.limits().max_texture_dimension_2d)
                .unwrap_or(8192);
            // Render at the true on-screen scale, but capped (blur cost ~scale⁴): above the cap the
            // texture is rendered coarser and the GPU magnifies it (see `SVG_MAX_RENDER_SCALE`).
            let render_scale = s.min(SVG_MAX_RENDER_SCALE);
            let ow = ((target.2 * render_scale).round() as u32).clamp(1, max_dim);
            let oh = ((target.3 * render_scale).round() as u32).clamp(1, max_dim);
            self.svg_vp_gen = self.svg_vp_gen.wrapping_add(1);
            self.svg_vp_pending = Some((target.0, target.1, target.2, target.3, s));
            self.tile_queue.clear();
            self.tile_queue.push(TileJob {
                epoch: self.svg_vp_gen,
                col: 0,
                row: 0,
                region: (target.0, target.1, target.2, target.3),
                out: (ow, oh),
                doc,
            });
        }
        None
    }

    /// Drop any cached/pending SVG viewport render (image changed, or zoomed back out to fit).
    fn discard_svg_viewport(&mut self) {
        if self.svg_vp_region.is_some() || self.svg_vp_pending.is_some() {
            self.tile_queue.clear();
            if let Some(g) = self.gpu.as_mut() {
                g.clear_svg_tiles();
            }
            self.redraw();
        }
        self.svg_vp_region = None;
        self.svg_vp_pending = None;
    }

    fn toggle_slideshow(&mut self) {
        if self.slideshow_next.is_some() {
            self.slideshow_next = None;
            self.show_status("Slideshow stop");
        } else {
            self.slideshow_next = Some(Instant::now() + self.slideshow_interval);
            self.show_status("Slideshow start");
        }
    }

    fn show_status(&mut self, text: &str) {
        self.status_text = text.to_string();
        self.status_until = Some(Instant::now() + Duration::from_secs(1));
    }

    fn copy_image_to_clipboard(&mut self) {
        let Some(img) = self.nav.current_image() else {
            return;
        };
        let data = arboard::ImageData {
            width: img.width as usize,
            height: img.height as usize,
            bytes: std::borrow::Cow::Borrowed(&img.rgba),
        };
        match Clipboard::new().and_then(|mut cb| cb.set_image(data)) {
            Ok(()) => self.show_status("Image copied"),
            Err(_) => self.show_status("Copy failed"),
        }
        self.redraw();
    }

    fn copy_path_to_clipboard(&mut self) {
        let Some(path) = self.nav.current_path() else {
            return;
        };
        let text = path.display().to_string();
        match Clipboard::new().and_then(|mut cb| cb.set_text(text)) {
            Ok(()) => self.show_status("Path copied"),
            Err(_) => self.show_status("Copy failed"),
        }
        self.redraw();
    }

    fn stop_slideshow(&mut self) {
        self.slideshow_next = None;
    }

    /// Compose the info-overlay body for the current image: filename, dimensions, format,
    /// size, modified date. Returns `None` when there is no current image.
    fn build_info_string(&self) -> Option<String> {
        let path = self.nav.current_path()?;
        let mut lines: Vec<String> = Vec::with_capacity(5);
        lines.push(
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("(unknown)")
                .to_string(),
        );
        if let Some(img) = self.nav.current_image() {
            lines.push(format!("{} × {} px", img.width, img.height));
        }
        if let Ok(meta) = glanvu_core::read_meta_path(path) {
            lines.push(meta.format.name().to_string());
            lines.push(human_size(meta.file_size));
        }
        if let Some(dt) = mtime_string(path) {
            lines.push(dt);
        }
        Some(lines.join("\n"))
    }

    /// Body text for the delete confirmation modal. Shows the filename for a single image,
    /// or a count for a group.
    fn delete_confirm_text(&self) -> Option<String> {
        let paths = self.confirm_delete.as_ref()?;
        let body = match paths.as_slice() {
            [] => return None,
            [one] => one
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("this image")
                .to_string(),
            many => format!("{} images", many.len()),
        };
        Some(format!(
            "Move to Trash?\n\n{body}\n\nEnter = delete   Esc = cancel"
        ))
    }

    /// Text of the single active modal overlay (only one can be up at a time), or `None`.
    /// Priority: find → rename editor → rename confirm → delete confirm → set-default confirm.
    fn modal_text(&self) -> Option<String> {
        if let Some(t) = self.find_text() {
            return Some(t);
        }
        if let Some(ed) = &self.rename {
            return Some(format!(
                "Rename\n\n{}\n\nEnter = continue   Esc = cancel",
                ed.display_with_caret()
            ));
        }
        if let Some((old, new)) = &self.confirm_rename {
            let newn = file_name_str(new);
            return Some(if new.exists() {
                format!(
                    "Rename to:\n\n{newn}\n\n{newn} already exists\nand will be moved to Trash\n\nEnter = rename   Esc = cancel"
                )
            } else {
                format!(
                    "Rename?\n\n{}\n→  {newn}\n\nEnter = rename   Esc = cancel",
                    file_name_str(old)
                )
            });
        }
        if let Some(t) = self.delete_confirm_text() {
            return Some(t);
        }
        self.confirm_assoc
            .map(|set| confirm_overlay_text(set).to_string())
    }

    /// Open the inline rename editor for the current image (F2), pre-filled with its stem.
    fn begin_rename(&mut self) {
        let Some(path) = self.nav.current_path() else {
            return;
        };
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        self.rename = Some(TextInput::new(stem));
        self.redraw();
    }

    /// Validate the typed name and move to the confirmation step (or cancel on no-op).
    fn finish_rename_edit(&mut self, new_stem: String) {
        self.rename = None;
        let Some(old) = self.nav.current_path().cloned() else {
            self.redraw();
            return;
        };
        let new_stem = new_stem.trim();
        let old_stem = old.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if new_stem.is_empty() || new_stem == old_stem {
            self.show_status("Rename cancelled");
            self.redraw();
            return;
        }
        // Keep the original extension; only the stem is editable.
        let new_name = match old.extension().and_then(|e| e.to_str()) {
            Some(ext) => format!("{new_stem}.{ext}"),
            None => new_stem.to_string(),
        };
        let new = old.with_file_name(new_name);
        if new == old {
            self.show_status("Rename cancelled");
            self.redraw();
            return;
        }
        self.confirm_rename = Some((old, new));
        self.redraw();
    }

    // --- Quick-open search (find by name) ---

    /// Open find, seeded with an empty query. In single view this is a floating match list (capped
    /// at `FIND_LIMIT`); in grid view it is a live filter showing every match (`usize::MAX`).
    fn begin_find(&mut self) {
        let limit = if self.mode == ViewMode::Grid {
            usize::MAX
        } else {
            FIND_LIMIT
        };
        let st = FindState {
            input: TextInput::new(""),
            matches: Vec::new(),
            sel: 0,
            limit,
            scroll_y: 0.0,
        };
        self.find = Some(st.recompute_for(&self.nav.paths));
        self.redraw();
    }

    /// Re-rank matches for the current query and clamp the highlighted match. In the grid filter
    /// the highlight resets to the best match, so keep it scrolled into view.
    fn find_recompute(&mut self) {
        if let Some(st) = self.find.take() {
            self.find = Some(st.recompute_for(&self.nav.paths));
        }
        if self.mode == ViewMode::Grid {
            self.find_scroll_to_cursor();
        }
    }

    /// Open the currently highlighted match (works from single or grid → single view).
    fn find_open_selected(&mut self) {
        let idx = self.find.as_ref().and_then(|st| st.current());
        self.find = None;
        if let Some(idx) = idx {
            self.enter_single(idx);
        } else {
            self.redraw();
        }
    }

    /// Close the grid filter (Esc): drop the query and return to the full grid, selecting and
    /// scrolling to whichever match was highlighted so the user sees where it is.
    fn find_close_grid(&mut self) {
        let found = self.find.as_ref().and_then(|st| st.current());
        self.find = None;
        if let Some(idx) = found {
            self.grid.select_single(idx);
            let (win_w, win_h) = self.win_size();
            self.grid.scroll_to_sel(self.nav.paths.len(), win_w, win_h);
        }
        self.redraw();
    }

    /// Move the grid-filter cursor by `(dc, dr)` columns/rows over the matches, then scroll to it.
    fn find_grid_move(&mut self, dc: isize, dr: isize) {
        let (win_w, _) = self.win_size();
        let cols = GridState::col_count(win_w) as isize;
        if let Some(st) = self.find.as_mut() {
            let n = st.matches.len() as isize;
            if n == 0 {
                return;
            }
            st.sel = (st.sel as isize + dc + dr * cols).clamp(0, n - 1) as usize;
        }
        self.find_scroll_to_cursor();
        self.redraw();
    }

    /// Keep the highlighted grid-filter tile visible, accounting for the top search bar.
    fn find_scroll_to_cursor(&mut self) {
        let (win_w, win_h) = self.win_size();
        let scale = self
            .window
            .as_ref()
            .map(|w| w.scale_factor() as f32)
            .unwrap_or(1.0);
        let inset = find_bar_height(scale) + GAP;
        let cols = GridState::col_count(win_w).max(1);
        if let Some(st) = self.find.as_mut() {
            let n = st.matches.len();
            if n == 0 {
                st.scroll_y = 0.0;
                return;
            }
            let row = st.sel / cols;
            // Tile position in grid space (before scroll); the renderer adds `inset` on top.
            let cell_top = MARGIN + row as f32 * (CELL_H + GAP) + inset;
            let cell_bot = cell_top + CELL_H;
            if cell_top - st.scroll_y < inset {
                st.scroll_y = cell_top - inset;
            } else if cell_bot - st.scroll_y > win_h - MARGIN {
                st.scroll_y = cell_bot - (win_h - MARGIN);
            }
            let rows = n.div_ceil(cols);
            let content_h = MARGIN * 2.0 + rows as f32 * (CELL_H + GAP) - GAP + inset;
            let max_scroll = (content_h - win_h).max(0.0);
            st.scroll_y = st.scroll_y.clamp(0.0, max_scroll);
        }
    }

    /// Body text of the single-view find modal: header, query line with caret, the ranked match
    /// list (highlighted row marked with `▶`), and key hints. Reuses the shared modal box.
    fn find_text(&self) -> Option<String> {
        let st = self.find.as_ref()?;
        let mut out = format!("Find image\n\n  {}\n\n", st.input.display_with_caret());
        if st.matches.is_empty() {
            out.push_str("(no matches)");
        } else {
            for (row, &idx) in st.matches.iter().enumerate() {
                let name = self
                    .nav
                    .paths
                    .get(idx)
                    .map(|p| file_name_str(p))
                    .unwrap_or("(unknown)");
                let marker = if row == st.sel { "▶ " } else { "   " };
                out.push_str(marker);
                out.push_str(name);
                out.push('\n');
            }
        }
        out.push_str("\n↑↓ select   Enter = open   Esc = cancel");
        Some(out)
    }

    /// Top search-bar text for the grid filter (query with caret + match count).
    fn find_bar_text(&self) -> Option<String> {
        let st = self.find.as_ref()?;
        let n = st.matches.len();
        let word = if n == 1 { "match" } else { "matches" };
        Some(format!(
            "Find:  {}     {n} {word}     ↵ open   Esc clear",
            st.input.display_with_caret()
        ))
    }

    /// Perform the on-disk rename (trashing the target first if it already exists), then update
    /// the playlist so the renamed image stays current.
    fn do_rename(&mut self, old: PathBuf, new: PathBuf) {
        if new.exists() {
            if let Err(e) = trash::delete(&new) {
                self.show_status(&format!("Rename failed: {e}"));
                self.redraw();
                return;
            }
        }
        if let Err(e) = std::fs::rename(&old, &new) {
            self.show_status(&format!("Rename failed: {e}"));
            self.redraw();
            return;
        }
        self.nav.rename_path(&old, &new, self.sort_mode);
        if self.mode == ViewMode::Grid {
            self.update_title_grid();
        } else if let Some(res) = self.nav.show_index(self.nav.index) {
            self.apply_nav_result(res);
        }
        self.show_status(&format!("Renamed to {}", file_name_str(&new)));
        self.redraw();
    }

    /// Move the given image(s) to the system Trash (recycle bin) — never a permanent delete.
    /// Removes them from the playlist, then re-shows: a neighbour in single view, a clamped
    /// cursor in the grid, or the empty state if the folder is now exhausted.
    fn delete_paths_to_trash(&mut self, paths: Vec<PathBuf>) {
        let mut trashed: HashSet<PathBuf> = HashSet::new();
        let mut failed = 0usize;
        for p in &paths {
            match trash::delete(p) {
                Ok(()) => {
                    trashed.insert(p.clone());
                }
                Err(_) => failed += 1,
            }
        }
        if trashed.is_empty() {
            self.show_status("Trash failed");
            self.redraw();
            return;
        }
        let removed = self.nav.remove_paths(&trashed);

        if self.nav.paths.is_empty() {
            self.mode = ViewMode::Empty;
            if let Some(w) = &self.window {
                w.set_title("Glanvu");
            }
            self.redraw();
        } else if self.mode == ViewMode::Grid {
            let n = self.nav.paths.len();
            self.grid.sel = self.grid.sel.min(n - 1);
            self.grid.clear_to_cursor();
            let (win_w, win_h) = self.win_size();
            self.grid.scroll_to_sel(n, win_w, win_h);
            self.update_title_grid();
            self.redraw();
        } else if let Some(res) = self.nav.show_index(self.nav.index) {
            self.apply_nav_result(res);
        }

        let msg = if failed == 0 {
            format!("Moved {removed} to Trash")
        } else {
            format!("Moved {removed} to Trash · {failed} failed")
        };
        self.show_status(&msg);
        self.redraw();
    }

    /// Re-scan the current image's directory and reconcile the playlist with what's on disk
    /// (files added/removed externally — e.g. recovered from Trash, new screenshots). Preserves
    /// the current image when it survives; shows a neighbour or the empty state if it vanished.
    fn rescan_directory(&mut self) {
        let Some(dir) = self
            .nav
            .current_path()
            .and_then(|p| p.parent())
            .map(|p| p.to_path_buf())
        else {
            return;
        };
        let cur_before = self.nav.current_path().cloned();
        let scanned = glanvu_core::list_images(&dir);
        // Path-level diff (files added/removed) + content-level diff (files whose bytes changed,
        // detected by mtime — cheap, the decode cache holds only ~5 images).
        let path_change = self.nav.sync_paths(scanned, self.sort_mode);
        let stale = self.nav.evict_stale();
        if path_change.is_none() && stale.is_empty() {
            return; // nothing changed on disk
        }

        // Content-changed files: drop their GPU thumbnail + forget the cached thumb so the grid
        // regenerates them fresh.
        if !stale.is_empty() {
            if let Some(gpu) = self.gpu.as_mut() {
                for p in &stale {
                    gpu.thumb_binds.remove(p);
                }
            }
            for p in &stale {
                self.thumbs.invalidate(p);
            }
        }

        if self.nav.paths.is_empty() {
            self.mode = ViewMode::Empty;
            if let Some(w) = &self.window {
                w.set_title("Glanvu");
            }
            self.redraw();
            return;
        }

        // The shown image needs a re-decode if its path changed OR its content was evicted.
        let cur_after = self.nav.current_path().cloned();
        let current_dirty =
            cur_after != cur_before || cur_after.as_ref().is_some_and(|p| stale.contains(p));

        match self.mode {
            ViewMode::Grid => {
                let n = self.nav.paths.len();
                self.grid.sel = self.grid.sel.min(n - 1);
                self.grid.clear_to_cursor();
                for p in &self.nav.paths.clone() {
                    self.thumbs.request(p);
                }
                let (win_w, win_h) = self.win_size();
                self.grid.scroll_to_sel(n, win_w, win_h);
                self.update_title_grid();
            }
            ViewMode::Single => {
                if current_dirty {
                    if let Some(res) = self.nav.show_index(self.nav.index) {
                        self.apply_nav_result(res);
                    }
                } else if let Some(p) = self.nav.current_path().cloned() {
                    self.update_title(self.nav.index, self.nav.paths.len(), &p);
                }
            }
            ViewMode::Empty => {}
        }

        let (added, removed) = path_change.unwrap_or((0, 0));
        let mut parts: Vec<String> = Vec::new();
        if added > 0 {
            parts.push(format!("+{added}"));
        }
        if removed > 0 {
            parts.push(format!("−{removed}"));
        }
        if !stale.is_empty() {
            parts.push(format!("~{}", stale.len()));
        }
        self.show_status(&format!("Folder updated  {}", parts.join(" ")));
        self.redraw();
    }

    /// Manual full refresh (F5): drop every cached decode + thumbnail and re-scan the directory,
    /// then re-show everything fresh. The hands-down way to reconcile any external change.
    fn force_refresh(&mut self) {
        self.nav.clear_cache();
        self.thumbs.clear();
        if let Some(gpu) = self.gpu.as_mut() {
            gpu.thumb_binds.clear();
        }
        if let Some(dir) = self
            .nav
            .current_path()
            .and_then(|p| p.parent())
            .map(|p| p.to_path_buf())
        {
            let _ = self.nav.sync_paths(glanvu_core::list_images(&dir), self.sort_mode);
        }
        if self.nav.paths.is_empty() {
            self.mode = ViewMode::Empty;
            if let Some(w) = &self.window {
                w.set_title("Glanvu");
            }
            self.redraw();
            return;
        }
        match self.mode {
            ViewMode::Grid => {
                let n = self.nav.paths.len();
                self.grid.sel = self.grid.sel.min(n - 1);
                self.grid.clear_to_cursor();
                for p in &self.nav.paths.clone() {
                    self.thumbs.request(p);
                }
                let (win_w, win_h) = self.win_size();
                self.grid.scroll_to_sel(n, win_w, win_h);
                self.update_title_grid();
            }
            ViewMode::Single => {
                if let Some(res) = self.nav.show_index(self.nav.index) {
                    self.apply_nav_result(res);
                }
            }
            ViewMode::Empty => {}
        }
        self.show_status("Refreshed");
        self.redraw();
    }

    /// Move the grid cursor by `(dc, dr)` cells, then update the selection: Shift extends the
    /// range from the anchor, otherwise it becomes a single selection.
    fn grid_move(&mut self, dc: isize, dr: isize) {
        let (win_w, win_h) = self.win_size();
        let n = self.nav.paths.len();
        if n == 0 {
            return;
        }
        self.grid.move_sel(dc, dr, n, win_w);
        let to = self.grid.sel;
        if self.modifiers.shift_key() {
            self.grid.select_range(to);
        } else {
            self.grid.select_single(to);
        }
        self.grid.scroll_to_sel(n, win_w, win_h);
        self.redraw();
    }

    /// Jump the grid cursor to an absolute index (Home/End), applying the same Shift/plain
    /// selection rule as `grid_move`.
    fn grid_set_cursor(&mut self, idx: usize) {
        let (win_w, win_h) = self.win_size();
        let n = self.nav.paths.len();
        if n == 0 {
            return;
        }
        let idx = idx.min(n - 1);
        if self.modifiers.shift_key() {
            self.grid.select_range(idx);
        } else {
            self.grid.select_single(idx);
        }
        self.grid.scroll_to_sel(n, win_w, win_h);
        self.redraw();
    }

    /// Cycle the sort order from grid view (O), preserving the multi-selection, cursor, and anchor
    /// across the reorder. The playlist indices change on resort, so everything is remembered by
    /// path and remapped to the new positions afterwards. Thumbnails are keyed by path and survive.
    fn grid_cycle_sort(&mut self) {
        if self.nav.paths.is_empty() {
            return;
        }
        // Remember selection / cursor / anchor by path (indices are about to change).
        let sel_path = self.nav.paths.get(self.grid.sel).cloned();
        let anchor_path = self.nav.paths.get(self.grid.anchor).cloned();
        let selected_paths: Vec<PathBuf> = self
            .grid
            .selected
            .iter()
            .filter_map(|&i| self.nav.paths.get(i).cloned())
            .collect();

        self.sort_mode = self.sort_mode.next();
        self.nav.resort(self.sort_mode);

        // Remap to the new indices by path.
        self.grid.selected = selected_paths
            .iter()
            .filter_map(|p| self.nav.paths.iter().position(|q| q == p))
            .collect();
        if let Some(i) = sel_path.and_then(|p| self.nav.paths.iter().position(|q| *q == p)) {
            self.grid.sel = i;
        }
        self.grid.anchor = anchor_path
            .and_then(|p| self.nav.paths.iter().position(|q| *q == p))
            .unwrap_or(self.grid.sel);

        let (win_w, win_h) = self.win_size();
        self.grid.scroll_to_sel(self.nav.paths.len(), win_w, win_h);
        self.show_status(self.sort_mode.label());
        self.redraw();
    }

    fn open_explorer(&mut self) {
        if let Some(path) = self.nav.current_path() {
            let mut exp = ExplorerState::for_path(&path.clone());
            let (_, win_h) = self.win_size();
            let scale = self
                .window
                .as_ref()
                .map(|w| w.scale_factor() as f32)
                .unwrap_or(1.0);
            exp.scroll_to_sel(win_h / scale);
            self.explorer = Some(exp);
        }
    }

    fn close_explorer(&mut self) {
        self.explorer = None;
    }

    /// Resize the window to the ideal size for the current image (used when opening a file from
    /// the empty state, so "Open With" gets a properly-sized window like `glanvu <file>` does).
    fn fit_window_to_image(&self) {
        if let Some(w) = &self.window {
            let (ww, wh) = ideal_window_size(self.img_size.0, self.img_size.1);
            let _ = w.request_inner_size(LogicalSize::new(ww, wh));
        }
    }

    /// Open a file received from the OS (Finder "Open With", drag-and-drop, Apple Events).
    /// Reloads the folder playlist if the file is in a different directory.
    fn open_file_path(&mut self, path: &std::path::Path) {
        if !glanvu_core::is_supported_path(path) {
            return;
        }
        let was_empty = self.mode == ViewMode::Empty;
        self.mode = ViewMode::Single; // leave Empty state
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));

        // Check if the file is already in the current playlist.
        if let Some(idx) = self.nav.paths.iter().position(|p| p == path) {
            if let Some(res) = self.nav.show_index(idx) {
                self.apply_nav_result(res);
            }
        } else {
            // Different folder — build a new playlist.
            let images = glanvu_core::list_images(&dir);
            let idx = images.iter().position(|p| p == path).unwrap_or(0);
            match glanvu_core::decode_path(path) {
                Ok(img) => {
                    self.nav = FolderNav::new(images, idx, path.to_path_buf(), img);
                    if let Some(res) = self.nav.show_index(idx) {
                        self.apply_nav_result(res);
                    }
                    self.close_explorer();
                }
                Err(e) => {
                    eprintln!("glanvu: cannot open {}: {e}", path.display());
                    return;
                }
            }
        }

        // Opening from the empty state (Open With / drop on launch): size the window to the image,
        // matching `glanvu <file>` behavior. Skip on normal navigation (keeps the user's window size).
        if was_empty {
            self.fit_window_to_image();
        }
    }

    fn enter_grid(&mut self) {
        self.stop_slideshow();
        self.mode = ViewMode::Grid;
        // Start with the current image as the sole selection (fresh each time the grid opens).
        self.grid.select_single(self.nav.index);
        let (win_w, win_h) = self.win_size();
        self.grid.scroll_to_sel(self.nav.paths.len(), win_w, win_h);
        // Request thumbnails for all paths (worker handles dedup).
        for p in &self.nav.paths.clone() {
            self.thumbs.request(p);
        }
        self.update_title_grid();
        // Paint immediately so the grid appears without waiting for a user action
        // (same fix as the single-image blank-first-frame issue on macOS).
        self.draw();
        self.redraw();
    }

    fn enter_single(&mut self, idx: usize) {
        self.mode = ViewMode::Single;
        if let Some(res) = self.nav.show_index(idx) {
            self.apply_nav_result(res);
        }
    }

    fn win_size(&self) -> (f32, f32) {
        self.window
            .as_ref()
            .map(|w| {
                let s = w.inner_size();
                (s.width as f32, s.height as f32)
            })
            .unwrap_or((800.0, 600.0))
    }

    fn update_title_grid(&self) {
        if let Some(w) = &self.window {
            w.set_title(&format!("Glanvu — {} images", self.nav.paths.len()));
        }
    }

    fn toggle_fullscreen(&self) {
        if let Some(w) = &self.window {
            w.set_fullscreen(if w.fullscreen().is_some() {
                None
            } else {
                Some(Fullscreen::Borderless(None))
            });
        }
    }

    fn update_title(&self, index: usize, total: usize, path: &Path) {
        if let Some(w) = &self.window {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            w.set_title(&format!("{name} ({}/{total}) - Glanvu", index + 1));
        }
    }

    fn apply_nav_result(&mut self, res: glanvu_viewer_core::nav::ShowResult) {
        self.img_size = res.img_size;
        self.current_is_svg = glanvu_core::is_svg_path(&res.path);
        if let (Some(gpu), Some(img)) = (self.gpu.as_mut(), self.nav.current_image()) {
            gpu.set_image(img);
        }
        self.state = ViewState::fit();
        // The nav/prefetch decode rasterizes an SVG at its *intrinsic* size, which is blurry when
        // fit into a window larger than that (e.g. opening an SVG in an already-enlarged window).
        // Schedule an immediate crisp re-raster at the actual on-screen size. Raster images are
        // already full-res (mipmaps handle minification), so nothing is pending for them.
        self.svg_rerender_at = self.current_is_svg.then(Instant::now);
        self.refresh_svg_doc();
        self.update_title(res.index, res.total, &res.path);
        self.flash_overlay();

        if perf_logging() {
            let name = res
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let kind = if res.cache_hit {
                "prefetched"
            } else {
                "decoded"
            };
            eprintln!(
                "glanvu: switch to {name} in {:.1} ms ({kind})",
                res.elapsed_ms
            );
        }
        self.draw();
    }

    fn draw(&mut self) {
        // Drain any newly generated thumbnails and upload them to the GPU.
        // We clone the images to avoid the borrow-checker conflict between `self.thumbs`
        // and `self.gpu`. Thumbnails are small (THUMB_W×THUMB_H×4 bytes ≈ 75 KB); cloning
        // is acceptable here and happens at most once per thumbnail per session.
        if self.thumbs.drain() {
            let to_upload: Vec<(PathBuf, DecodedImage)> = self
                .nav
                .paths
                .iter()
                .filter(|p| {
                    self.thumbs.get(p).is_some()
                        && !self
                            .gpu
                            .as_ref()
                            .map(|g| g.thumb_binds.contains_key(*p))
                            .unwrap_or(false)
                })
                .filter_map(|p| self.thumbs.get(p).map(|img| (p.clone(), img.clone())))
                .collect();
            if let Some(gpu) = self.gpu.as_mut() {
                for (path, img) in to_upload {
                    gpu.upload_thumb(path, &img);
                }
            }
        }

        // Empty state: dark window with a centred prompt.
        if self.mode == ViewMode::Empty {
            let scale = self
                .window
                .as_ref()
                .map(|w| w.scale_factor() as f32)
                .unwrap_or(1.0);
            let Some(gpu) = self.gpu.as_mut() else { return };
            let win = (gpu.config.width as f32, gpu.config.height as f32);
            // Re-use the status overlay as a centred hint text (no timer — always visible).
            let hint = "Drop an image here  ·  Press Enter to open";
            let confirm_text = self.confirm_assoc.map(confirm_overlay_text);
            let _ = gpu.render(
                Uniforms {
                    mvp: mvp((1, 1), win, &self.state),
                },
                None,
                None,
                Some(hint),
                None,
                self.help_visible,
                confirm_text,
                self.about_visible,
                None, // no image in empty mode → no info panel
                scale,
                true,
                &[], // no SVG tiles in the empty state
            );
            return;
        }

        if self.mode == ViewMode::Grid {
            let now = Instant::now();
            let status = match self.status_until {
                Some(t) if now < t => Some(self.status_text.clone()),
                _ => None,
            };
            let scale = self
                .window
                .as_ref()
                .map(|w| w.scale_factor() as f32)
                .unwrap_or(1.0);

            // Find filter active: render only the matched thumbnails, re-packed, with a top search
            // bar and the cursor on the highlighted match. A throwaway GridState drives the layout
            // (sel = highlighted match, its own scroll) so the real grid/selection is untouched.
            if let Some(st) = &self.find {
                let view_paths: Vec<PathBuf> = st
                    .matches
                    .iter()
                    .filter_map(|&i| self.nav.paths.get(i).cloned())
                    .collect();
                let mut fgrid = GridState::new(st.sel.min(view_paths.len().saturating_sub(1)));
                fgrid.scroll_y = st.scroll_y;
                let bar = self.find_bar_text();
                let Some(gpu) = self.gpu.as_mut() else { return };
                let presented =
                    gpu.render_grid(&view_paths, &fgrid, None, status.as_deref(), bar.as_deref(), scale);
                if !presented {
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
                return;
            }

            // Compute overlay text + scale before borrowing gpu (both read &self).
            let confirm = self.modal_text();
            let paths = self.nav.paths.clone(); // clone needed to split borrow from gpu
            let Some(gpu) = self.gpu.as_mut() else { return };
            let presented = gpu.render_grid(
                &paths,
                &self.grid,
                confirm.as_deref(),
                status.as_deref(),
                None,
                scale,
            );
            if !presented {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            return;
        }

        // Single-image mode (original path).
        let now = Instant::now();
        // SVG deep-zoom tiles: compute first (it needs `&mut self` to enqueue/touch), before the
        // immutable-borrow overlay locals below. Empty for raster / low zoom (see `svg_tile_draws`).
        let win_phys = self
            .gpu
            .as_ref()
            .map(|g| (g.config.width as f32, g.config.height as f32))
            .unwrap_or((800.0, 600.0));
        let svg_tile_draws = self.svg_tile_draws(win_phys);
        let overlay_active = matches!(self.overlay_until, Some(t) if now < t);
        let overlay = if overlay_active {
            self.nav.current_path().map(|p| p.display().to_string())
        } else {
            None
        };
        let date_overlay = if overlay_active && !self.date_text.is_empty() {
            Some(self.date_text.as_str())
        } else {
            None
        };
        let status = match self.status_until {
            Some(t) if now < t => Some(self.status_text.as_str()),
            _ => None,
        };
        // Info panel body (I key) — computed here (App owns nav) and passed into the renderer.
        let info = if self.info_visible {
            self.build_info_string()
        } else {
            None
        };
        // Active modal overlay text — computed before borrowing gpu (it's a &self method).
        let modal_text = self.modal_text();
        let scale = self
            .window
            .as_ref()
            .map(|w| w.scale_factor() as f32)
            .unwrap_or(1.0);
        let Some(gpu) = self.gpu.as_mut() else { return };
        let win = (gpu.config.width as f32, gpu.config.height as f32);
        let explorer_ref = self.explorer.as_ref();
        let confirm_text = modal_text.as_deref();
        let presented = gpu.render(
            Uniforms {
                mvp: mvp(self.img_size, win, &self.state),
            },
            overlay.as_deref(),
            date_overlay,
            status,
            explorer_ref,
            self.help_visible,
            confirm_text,
            self.about_visible,
            info.as_deref(),
            scale,
            false,
            &svg_tile_draws,
        );
        if presented {
            if !self.first_frame {
                self.first_frame = true;
                if perf_logging() {
                    let ms = self.start.elapsed().as_secs_f64() * 1000.0;
                    eprintln!("glanvu: first frame in {ms:.1} ms (cold: incl. one-time gpu init)");
                }
            }
        } else if let Some(w) = &self.window {
            w.request_redraw();
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let (ww, wh) = ideal_window_size(self.img_size.0, self.img_size.1);
        let attrs = Window::default_attributes()
            .with_title("Glanvu")
            .with_inner_size(LogicalSize::new(ww, wh));
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));

        let gpu = {
            let img = self
                .nav
                .current_image()
                .expect("current image must exist before resume");
            pollster::block_on(Gpu::new(window.clone(), img))
        };
        self.window = Some(window);
        self.gpu = Some(gpu);
        if let Some(current) = self.nav.current_path() {
            self.update_title(self.nav.index, self.nav.paths.len(), &current.clone());
        }
        self.flash_overlay();
        self.nav.prune_and_prefetch();
        // The initial image is rasterized at its intrinsic size; for an SVG whose intrinsic size
        // is smaller than the (min-clamped) window, fit magnifies it. Re-raster at window size.
        self.svg_rerender_at = self.current_is_svg.then(Instant::now);
        self.refresh_svg_doc();
        self.draw();
        self.redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
            }

            // Regaining focus → reconcile the playlist with the directory (files may have been
            // added/removed in another app, e.g. recovered from Trash or new screenshots).
            WindowEvent::Focused(true) => {
                if matches!(self.mode, ViewMode::Single | ViewMode::Grid) {
                    self.rescan_directory();
                }
            }

            WindowEvent::Resized(size) => {
                if let Some(gpu) = &mut self.gpu {
                    gpu.resize(size);
                }
                // In fit mode the effective on-screen scale changes with the window size.
                if self.state.fit {
                    self.schedule_svg_rerender();
                }
                self.redraw();
            }

            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed {
                    return;
                }
                // If the about overlay is up, any keypress dismisses it.
                if self.about_visible {
                    self.about_visible = false;
                    self.redraw();
                    return;
                }

                // If the help overlay is up, the next keypress dismisses it.
                if self.help_visible {
                    self.help_visible = false;
                    self.redraw();
                    return;
                }

                // If the info panel is open, Escape closes it (rather than quitting). All other
                // keys fall through so you can keep navigating with the panel open and it updates.
                if self.info_visible {
                    if let Key::Named(NamedKey::Escape) = event.logical_key.as_ref() {
                        self.info_visible = false;
                        self.redraw();
                        return;
                    }
                }

                // If the confirm overlay is up, Enter confirms, Esc cancels, anything else is ignored.
                if let Some(set) = self.confirm_assoc {
                    match event.logical_key.as_ref() {
                        Key::Named(NamedKey::Enter) => {
                            self.confirm_assoc = None;
                            self.show_status("Working…");
                            self.redraw();
                            std::thread::spawn(move || {
                                let msg = if set {
                                    crate::associate::set_default_blocking()
                                } else {
                                    crate::associate::unset_default_blocking()
                                };
                                if let Ok(mut r) = crate::associate::ASSOC_RESULT.lock() {
                                    *r = Some(msg);
                                }
                            });
                        }
                        Key::Named(NamedKey::Escape) => {
                            self.confirm_assoc = None;
                            self.redraw();
                        }
                        _ => {}
                    }
                    return;
                }

                // If the delete confirmation is up, Enter trashes the image(s), Esc cancels.
                if self.confirm_delete.is_some() {
                    match event.logical_key.as_ref() {
                        Key::Named(NamedKey::Enter) => {
                            if let Some(paths) = self.confirm_delete.take() {
                                self.delete_paths_to_trash(paths);
                            }
                        }
                        Key::Named(NamedKey::Escape) => {
                            self.confirm_delete = None;
                            self.redraw();
                        }
                        _ => {}
                    }
                    return;
                }

                // Inline rename editor (F2): typed keys edit the name; Enter advances to confirm.
                if self.rename.is_some() {
                    match event.logical_key.as_ref() {
                        Key::Named(NamedKey::Enter) => {
                            if let Some(ed) = self.rename.take() {
                                self.finish_rename_edit(ed.text());
                            }
                        }
                        Key::Named(NamedKey::Escape) => {
                            self.rename = None;
                            self.show_status("Rename cancelled");
                            self.redraw();
                        }
                        Key::Named(NamedKey::Backspace) => {
                            if let Some(ed) = self.rename.as_mut() {
                                ed.backspace();
                            }
                            self.redraw();
                        }
                        Key::Named(NamedKey::Delete) => {
                            if let Some(ed) = self.rename.as_mut() {
                                ed.delete();
                            }
                            self.redraw();
                        }
                        Key::Named(NamedKey::ArrowLeft) => {
                            if let Some(ed) = self.rename.as_mut() {
                                ed.left();
                            }
                            self.redraw();
                        }
                        Key::Named(NamedKey::ArrowRight) => {
                            if let Some(ed) = self.rename.as_mut() {
                                ed.right();
                            }
                            self.redraw();
                        }
                        Key::Named(NamedKey::Home) => {
                            if let Some(ed) = self.rename.as_mut() {
                                ed.home();
                            }
                            self.redraw();
                        }
                        Key::Named(NamedKey::End) => {
                            if let Some(ed) = self.rename.as_mut() {
                                ed.end();
                            }
                            self.redraw();
                        }
                        Key::Named(NamedKey::Space) => {
                            if let Some(ed) = self.rename.as_mut() {
                                ed.insert(' ');
                            }
                            self.redraw();
                        }
                        Key::Character(s) => {
                            if let Some(ed) = self.rename.as_mut() {
                                ed.insert_str(s);
                            }
                            self.redraw();
                        }
                        _ => {}
                    }
                    return;
                }

                // Rename confirmation: Enter performs the rename, Esc cancels.
                if let Some((old, new)) = self.confirm_rename.clone() {
                    match event.logical_key.as_ref() {
                        Key::Named(NamedKey::Enter) => {
                            self.confirm_rename = None;
                            self.do_rename(old, new);
                        }
                        Key::Named(NamedKey::Escape) => {
                            self.confirm_rename = None;
                            self.show_status("Rename cancelled");
                            self.redraw();
                        }
                        _ => {}
                    }
                    return;
                }

                // Find by name. Typed keys edit the query and re-rank. In single view the arrows
                // walk the floating list (Up/Down) and edit the caret (Left/Right); in the grid
                // filter all four arrows move the cursor over the matched tiles. Enter opens the
                // highlighted match; Esc closes (in grid, selecting the match in the full grid).
                if self.find.is_some() {
                    let in_grid = self.mode == ViewMode::Grid;
                    match event.logical_key.as_ref() {
                        Key::Named(NamedKey::Escape) => {
                            if in_grid {
                                self.find_close_grid();
                            } else {
                                self.find = None;
                                self.redraw();
                            }
                        }
                        Key::Named(NamedKey::Enter) => self.find_open_selected(),
                        Key::Named(NamedKey::ArrowDown) => {
                            if in_grid {
                                self.find_grid_move(0, 1);
                            } else {
                                if let Some(st) = self.find.as_mut() {
                                    if !st.matches.is_empty() {
                                        st.sel = (st.sel + 1) % st.matches.len();
                                    }
                                }
                                self.redraw();
                            }
                        }
                        Key::Named(NamedKey::ArrowUp) => {
                            if in_grid {
                                self.find_grid_move(0, -1);
                            } else {
                                if let Some(st) = self.find.as_mut() {
                                    if !st.matches.is_empty() {
                                        st.sel = (st.sel + st.matches.len() - 1) % st.matches.len();
                                    }
                                }
                                self.redraw();
                            }
                        }
                        Key::Named(NamedKey::ArrowLeft) => {
                            if in_grid {
                                self.find_grid_move(-1, 0);
                            } else {
                                if let Some(st) = self.find.as_mut() {
                                    st.input.left();
                                }
                                self.redraw();
                            }
                        }
                        Key::Named(NamedKey::ArrowRight) => {
                            if in_grid {
                                self.find_grid_move(1, 0);
                            } else {
                                if let Some(st) = self.find.as_mut() {
                                    st.input.right();
                                }
                                self.redraw();
                            }
                        }
                        Key::Named(NamedKey::Backspace) => {
                            if let Some(st) = self.find.as_mut() {
                                st.input.backspace();
                                st.sel = 0;
                            }
                            self.find_recompute();
                            self.redraw();
                        }
                        Key::Named(NamedKey::Delete) => {
                            if let Some(st) = self.find.as_mut() {
                                st.input.delete();
                                st.sel = 0;
                            }
                            self.find_recompute();
                            self.redraw();
                        }
                        Key::Named(NamedKey::Space) => {
                            if let Some(st) = self.find.as_mut() {
                                st.input.insert(' ');
                                st.sel = 0;
                            }
                            self.find_recompute();
                            self.redraw();
                        }
                        Key::Character(s) => {
                            if let Some(st) = self.find.as_mut() {
                                st.input.insert_str(s);
                                st.sel = 0;
                            }
                            self.find_recompute();
                            self.redraw();
                        }
                        _ => {}
                    }
                    return;
                }

                // Empty mode: Esc/Q to quit, Enter to open a file, H for help.
                if self.mode == ViewMode::Empty {
                    match event.logical_key.as_ref() {
                        Key::Named(NamedKey::Escape)
                        | Key::Character("q")
                        | Key::Character("Q") => event_loop.exit(),
                        Key::Character("h") | Key::Character("H") | Key::Character("?") => {
                            self.help_visible = true;
                            self.redraw();
                        }
                        Key::Character("d") | Key::Character("D") => {
                            self.confirm_assoc = Some(true);
                            self.redraw();
                        }
                        Key::Character("u") | Key::Character("U") => {
                            self.confirm_assoc = Some(false);
                            self.redraw();
                        }
                        Key::Named(NamedKey::Enter) => {
                            if let Some(path) = rfd::FileDialog::new()
                                .set_title("Open image — Glanvu")
                                .add_filter(
                                    "Images",
                                    &["jpg", "jpeg", "png", "gif", "bmp", "tif", "tiff", "webp"],
                                )
                                .add_filter("All files", &["*"])
                                .pick_file()
                            {
                                self.open_file_path(&path);
                            }
                        }
                        _ => {}
                    }
                    return;
                }

                // Tab / G toggles between single-image and grid mode.
                let is_grid_toggle = event.logical_key == Key::Named(NamedKey::Tab)
                    || matches!(
                        event.logical_key.as_ref(),
                        Key::Character("g") | Key::Character("G")
                    );
                if is_grid_toggle {
                    match self.mode {
                        ViewMode::Single => self.enter_grid(),
                        ViewMode::Grid => self.enter_single(self.grid.sel),
                        ViewMode::Empty => {} // handled above
                    }
                    return;
                }
                // In grid mode, arrows navigate tiles and Enter opens the selection.
                if self.mode == ViewMode::Grid {
                    let n = self.nav.paths.len();
                    let cmd_ctrl =
                        self.modifiers.control_key() || self.modifiers.super_key();
                    match event.logical_key.as_ref() {
                        // Esc collapses a multi-selection to the cursor first; only then quits.
                        Key::Named(NamedKey::Escape) => {
                            if self.grid.selected.len() > 1 {
                                self.grid.clear_to_cursor();
                                self.redraw();
                            } else {
                                event_loop.exit();
                            }
                        }
                        Key::Character("q") | Key::Character("Q") => event_loop.exit(),
                        // Find by name: `F` or `/`.
                        Key::Character("f") | Key::Character("F") | Key::Character("/") => {
                            self.begin_find()
                        }
                        // Toggle sort order (name / date), keeping the selection.
                        Key::Character("o") | Key::Character("O") => self.grid_cycle_sort(),
                        Key::Named(NamedKey::Enter) => {
                            let sel = self.grid.sel;
                            self.enter_single(sel);
                        }
                        // S from grid: open selected image and start slideshow from there.
                        Key::Character("s") | Key::Character("S") => {
                            let sel = self.grid.sel;
                            self.enter_single(sel);
                            self.toggle_slideshow();
                        }
                        // Ctrl/Cmd+A selects everything.
                        Key::Character("a") | Key::Character("A") if cmd_ctrl => {
                            self.grid.select_all(n);
                            self.redraw();
                        }
                        // Space toggles the cursor tile's selection (keyboard equivalent of Ctrl+click).
                        Key::Named(NamedKey::Space) => {
                            let sel = self.grid.sel;
                            self.grid.toggle(sel);
                            self.redraw();
                        }
                        // F5: force a full refresh (re-scan + drop all caches).
                        Key::Named(NamedKey::F5) => self.force_refresh(),
                        Key::Named(NamedKey::ArrowRight) => self.grid_move(1, 0),
                        Key::Named(NamedKey::ArrowLeft) => self.grid_move(-1, 0),
                        Key::Named(NamedKey::ArrowDown) => self.grid_move(0, 1),
                        Key::Named(NamedKey::ArrowUp) => self.grid_move(0, -1),
                        Key::Named(NamedKey::Home) => self.grid_set_cursor(0),
                        Key::Named(NamedKey::End) => {
                            self.grid_set_cursor(n.saturating_sub(1))
                        }
                        // Delete / Backspace: confirm, then trash the selection (or the cursor).
                        Key::Named(NamedKey::Delete) | Key::Named(NamedKey::Backspace) => {
                            let mut idxs: Vec<usize> = if self.grid.selected.is_empty() {
                                vec![self.grid.sel]
                            } else {
                                self.grid.selected.iter().copied().collect()
                            };
                            idxs.sort_unstable();
                            let paths: Vec<PathBuf> = idxs
                                .iter()
                                .filter_map(|&i| self.nav.paths.get(i).cloned())
                                .collect();
                            if !paths.is_empty() {
                                self.confirm_delete = Some(paths);
                                self.redraw();
                            }
                        }
                        _ => {}
                    }
                    return;
                }

                // Explorer panel: intercept keys when open.
                if self.explorer.is_some() {
                    let (_, win_h) = self.win_size();
                    let scale = self
                        .window
                        .as_ref()
                        .map(|w| w.scale_factor() as f32)
                        .unwrap_or(1.0);
                    match event.logical_key.as_ref() {
                        Key::Named(NamedKey::Escape) | Key::Named(NamedKey::Enter)
                            if matches!(
                                event.logical_key.as_ref(),
                                Key::Named(NamedKey::Escape)
                            ) =>
                        {
                            self.close_explorer();
                            self.redraw();
                        }
                        Key::Named(NamedKey::Enter) => {
                            if let Some(exp) = &self.explorer {
                                match exp.open_sel() {
                                    glanvu_viewer_core::explorer::OpenResult::OpenImage(path) => {
                                        self.close_explorer();
                                        let nav_dir = path
                                            .parent()
                                            .filter(|p| !p.as_os_str().is_empty())
                                            .map(|p| p.to_path_buf())
                                            .unwrap_or_else(|| PathBuf::from("."));
                                        if nav_dir
                                            == self
                                                .nav
                                                .paths
                                                .first()
                                                .and_then(|p| p.parent())
                                                .map(|p| p.to_path_buf())
                                                .unwrap_or_default()
                                        {
                                            // Same folder — just find & show the image.
                                            if let Some(idx) =
                                                self.nav.paths.iter().position(|p| p == &path)
                                            {
                                                if let Some(res) = self.nav.show_index(idx) {
                                                    self.apply_nav_result(res);
                                                }
                                            }
                                        } else {
                                            // Different folder — reload playlist and open.
                                            let images = glanvu_core::list_images(&nav_dir);
                                            if let Some(idx) =
                                                images.iter().position(|p| p == &path)
                                            {
                                                if let Ok(img) = glanvu_core::decode_path(&path) {
                                                    let new_nav =
                                                        glanvu_viewer_core::nav::FolderNav::new(
                                                            images, idx, path, img,
                                                        );
                                                    self.nav = new_nav;
                                                    if let Some(res) =
                                                        self.nav.show_index(self.nav.index)
                                                    {
                                                        self.apply_nav_result(res);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    glanvu_viewer_core::explorer::OpenResult::NavigateDir(dir) => {
                                        if let Some(exp) = self.explorer.as_mut() {
                                            exp.set_dir(dir);
                                            exp.scroll_to_sel(win_h / scale);
                                            self.redraw();
                                        }
                                    }
                                    glanvu_viewer_core::explorer::OpenResult::Nothing => {}
                                }
                            }
                        }
                        Key::Named(NamedKey::ArrowDown) => {
                            if let Some(exp) = self.explorer.as_mut() {
                                exp.move_sel(1);
                                exp.scroll_to_sel(win_h / scale);
                                self.redraw();
                            }
                        }
                        Key::Named(NamedKey::ArrowUp) => {
                            if let Some(exp) = self.explorer.as_mut() {
                                exp.move_sel(-1);
                                exp.scroll_to_sel(win_h / scale);
                                self.redraw();
                            }
                        }
                        _ => {} // other keys fall through to single-mode handler
                    }
                    return;
                }

                match event.logical_key.as_ref() {
                    Key::Named(NamedKey::Escape) => event_loop.exit(),
                    Key::Character("q") | Key::Character("Q") => event_loop.exit(),
                    // Find by name: `F` or `/` (fullscreen is on Space / F11 only).
                    Key::Character("f") | Key::Character("F") | Key::Character("/") => {
                        self.begin_find()
                    }
                    Key::Character("h") | Key::Character("H") | Key::Character("?") => {
                        self.help_visible = true;
                        self.about_visible = false;
                        self.info_visible = false;
                        self.redraw();
                    }
                    Key::Character("a") | Key::Character("A") => {
                        self.about_visible = !self.about_visible;
                        self.help_visible = false;
                        self.info_visible = false;
                        self.redraw();
                    }
                    Key::Character("i") | Key::Character("I") => {
                        self.info_visible = !self.info_visible;
                        self.about_visible = false;
                        self.help_visible = false;
                        self.redraw();
                    }
                    Key::Character("d") | Key::Character("D") => {
                        self.confirm_assoc = Some(true);
                        self.redraw();
                    }
                    Key::Character("u") | Key::Character("U") => {
                        self.confirm_assoc = Some(false);
                        self.redraw();
                    }
                    // Delete / Backspace (the latter is the main "delete" key on Mac keyboards):
                    // confirm, then move the current image to the system Trash.
                    Key::Named(NamedKey::Delete) | Key::Named(NamedKey::Backspace) => {
                        if let Some(path) = self.nav.current_path() {
                            self.confirm_delete = Some(vec![path.clone()]);
                            self.redraw();
                        }
                    }
                    // R (or F2): rename the current image (inline editor → confirmation).
                    Key::Character("r") | Key::Character("R") | Key::Named(NamedKey::F2) => {
                        self.begin_rename()
                    }
                    // F5: force a full refresh (re-scan + drop all caches).
                    Key::Named(NamedKey::F5) => self.force_refresh(),
                    Key::Named(NamedKey::Space) | Key::Named(NamedKey::F11) => {
                        self.toggle_fullscreen();
                        self.flash_overlay();
                        self.redraw();
                    }
                    // T: turn (rotate) 90° clockwise.
                    Key::Character("t") | Key::Character("T") => {
                        self.state.quarter_turns = (self.state.quarter_turns + 1) % 4;
                        self.redraw();
                    }
                    Key::Character("0") => {
                        self.state = ViewState::fit();
                        self.schedule_svg_rerender();
                        self.redraw();
                    }
                    Key::Character("1") => {
                        self.state.fit = false;
                        self.state.zoom = 1.0;
                        self.state.pan = (0.0, 0.0);
                        self.schedule_svg_rerender();
                        self.redraw();
                    }
                    Key::Character("+") | Key::Character("=") => {
                        self.state.zoom *= 1.25;
                        self.schedule_svg_rerender();
                        self.redraw();
                    }
                    Key::Character("-") | Key::Character("_") => {
                        self.state.zoom /= 1.25;
                        self.schedule_svg_rerender();
                        self.redraw();
                    }
                    Key::Character("s") | Key::Character("S") => {
                        self.toggle_slideshow();
                        self.redraw();
                    }
                    // Enter opens / closes the directory explorer.
                    Key::Named(NamedKey::Enter) => {
                        self.open_explorer();
                        self.redraw();
                    }
                    // Manual navigation stops the slideshow (user took control).
                    Key::Named(NamedKey::ArrowRight) | Key::Named(NamedKey::ArrowDown) => {
                        self.stop_slideshow();
                        if let Some(res) = self.nav.next() {
                            self.apply_nav_result(res);
                        }
                    }
                    Key::Named(NamedKey::ArrowLeft) | Key::Named(NamedKey::ArrowUp) => {
                        self.stop_slideshow();
                        if let Some(res) = self.nav.prev() {
                            self.apply_nav_result(res);
                        }
                    }
                    Key::Named(NamedKey::Home) => {
                        self.stop_slideshow();
                        if let Some(res) = self.nav.first() {
                            self.apply_nav_result(res);
                        }
                    }
                    Key::Named(NamedKey::End) => {
                        self.stop_slideshow();
                        if let Some(res) = self.nav.last() {
                            self.apply_nav_result(res);
                        }
                    }
                    Key::Character("c") => self.copy_image_to_clipboard(),
                    Key::Character("C") => self.copy_path_to_clipboard(),
                    Key::Character("o") | Key::Character("O") => {
                        self.sort_mode = self.sort_mode.next();
                        self.nav.resort(self.sort_mode);
                        self.show_status(self.sort_mode.label());
                        self.redraw();
                    }
                    _ => {}
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32 / 60.0,
                };
                if dy != 0.0 {
                    if self.mode == ViewMode::Grid && self.find.is_some() {
                        // Scroll the filtered view (its own scroll, accounting for the search bar).
                        let (win_w, win_h) = self.win_size();
                        let scale = self
                            .window
                            .as_ref()
                            .map(|w| w.scale_factor() as f32)
                            .unwrap_or(1.0);
                        let inset = find_bar_height(scale) + GAP;
                        let cols = GridState::col_count(win_w).max(1);
                        if let Some(st) = self.find.as_mut() {
                            let rows = st.matches.len().div_ceil(cols);
                            let content_h =
                                MARGIN * 2.0 + rows as f32 * (CELL_H + GAP) - GAP + inset;
                            let max_s = (content_h - win_h).max(0.0);
                            st.scroll_y -= dy * (CELL_H + GAP) * 0.5;
                            st.scroll_y = st.scroll_y.clamp(0.0, max_s);
                        }
                    } else if self.mode == ViewMode::Grid {
                        let (win_w, win_h) = self.win_size();
                        let n = self.nav.paths.len();
                        self.grid.scroll_y -= dy * (CELL_H + GAP) * 0.5;
                        let max_s = (GridState::total_height(n, win_w) - win_h).max(0.0);
                        self.grid.scroll_y = self.grid.scroll_y.clamp(0.0, max_s);
                    } else {
                        self.zoom_at_cursor(1.1_f32.powf(dy));
                    }
                    self.redraw();
                }
            }

            WindowEvent::MouseInput { state, button, .. } => {
                // While the grid filter is open, navigation is keyboard-driven; ignore grid clicks
                // (their hit-testing assumes the full, un-filtered playlist layout).
                if self.mode == ViewMode::Grid && self.find.is_some() {
                    return;
                }
                if button == MouseButton::Left {
                    if self.mode == ViewMode::Grid {
                        if state == ElementState::Pressed {
                            // Begin a potential drag. Selection is applied on release (click) or
                            // on move (marquee), so a plain press doesn't disturb the selection yet.
                            let cmd_ctrl =
                                self.modifiers.control_key() || self.modifiers.super_key();
                            self.grid_drag = Some(GridDrag {
                                start: (self.cursor.x as f32, self.cursor.y as f32),
                                base: if cmd_ctrl {
                                    self.grid.selected.clone()
                                } else {
                                    HashSet::new()
                                },
                                additive: cmd_ctrl,
                                range: self.modifiers.shift_key(),
                                moved: false,
                            });
                        } else if let Some(drag) = self.grid_drag.take() {
                            if drag.moved {
                                // Marquee finished — selection was updated live during the drag.
                                self.grid.marquee = None;
                                self.redraw();
                            } else {
                                // No movement → treat as a click at the release position.
                                let (win_w, _) = self.win_size();
                                let n = self.nav.paths.len();
                                let hit = self.grid.hit_test(
                                    self.cursor.x as f32,
                                    self.cursor.y as f32,
                                    win_w,
                                    n,
                                );
                                match hit {
                                    Some(idx) => {
                                        let now = Instant::now();
                                        let is_double = !drag.range
                                            && !drag.additive
                                            && self.last_grid_click.is_some_and(|(t, prev)| {
                                                prev == idx
                                                    && now.duration_since(t).as_millis() < 400
                                            });
                                        if is_double {
                                            self.last_grid_click = None;
                                            self.enter_single(idx);
                                        } else {
                                            self.last_grid_click = Some((now, idx));
                                            if drag.range {
                                                self.grid.select_range(idx);
                                            } else if drag.additive {
                                                self.grid.toggle(idx);
                                            } else {
                                                self.grid.select_single(idx);
                                            }
                                            self.redraw();
                                        }
                                    }
                                    // Click on empty space (no modifier) clears the selection.
                                    None => {
                                        if !drag.range && !drag.additive {
                                            self.grid.selected.clear();
                                            self.redraw();
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // Explorer click: select or open entry.
                    // Use GPU-computed width (actual font metrics) for accurate click detection.
                    let exp_panel_w = self
                        .gpu
                        .as_ref()
                        .map(|g| g.explorer_computed_panel_w)
                        .unwrap_or(PANEL_W);
                    if self.mode == ViewMode::Single
                        && self.explorer.is_some()
                        && state == ElementState::Pressed
                        && (self.cursor.x as f32) < exp_panel_w
                    {
                        let my = self.cursor.y as f32;
                        let (_, win_h) = self.win_size();
                        let scale = self
                            .window
                            .as_ref()
                            .map(|w| w.scale_factor() as f32)
                            .unwrap_or(1.0);
                        let hit = self.explorer.as_ref().and_then(|e| e.hit_entry(my / scale));
                        if let Some(idx) = hit {
                            if self.explorer.as_ref().map(|e| e.sel) == Some(idx) {
                                // Double-tap: open (same entry selected twice).
                                let action = self.explorer.as_ref().map(|e| e.open_sel());
                                match action {
                                    Some(
                                        glanvu_viewer_core::explorer::OpenResult::NavigateDir(dir),
                                    ) => {
                                        if let Some(exp) = self.explorer.as_mut() {
                                            exp.set_dir(dir);
                                            exp.scroll_to_sel(win_h / scale);
                                        }
                                    }
                                    Some(glanvu_viewer_core::explorer::OpenResult::OpenImage(
                                        path,
                                    )) => {
                                        self.close_explorer();
                                        if let Some(pos) =
                                            self.nav.paths.iter().position(|p| p == &path)
                                        {
                                            if let Some(res) = self.nav.show_index(pos) {
                                                self.apply_nav_result(res);
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            } else {
                                // First tap: just select.
                                if let Some(exp) = self.explorer.as_mut() {
                                    exp.sel = idx;
                                }
                            }
                            self.redraw();
                        }
                        return; // don't start drag when clicking in the panel
                    }
                    self.dragging = state == ElementState::Pressed;
                }
                // Donation link click detection. Hit zones are computed during render (exact text
                // metrics), so here we just test the cursor against them — no geometry duplication.
                if button == MouseButton::Left && state == ElementState::Pressed {
                    let cx = self.cursor.x as f32;
                    let cy = self.cursor.y as f32;
                    if let Some(gpu) = self.gpu.as_ref() {
                        for hit in &gpu.donate_hits {
                            if cx >= hit.x0 && cx < hit.x1 && cy >= hit.y0 && cy < hit.y1 {
                                let url = if cx < hit.split_x {
                                    KOFI_URL
                                } else {
                                    GITHUB_SPONSORS_URL
                                };
                                let _ = open::that(url);
                                break;
                            }
                        }
                    }
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                if self.dragging && self.mode == ViewMode::Single {
                    self.state.pan.0 += (position.x - self.cursor.x) as f32;
                    self.state.pan.1 -= (position.y - self.cursor.y) as f32;
                    self.redraw();
                } else if self.mode == ViewMode::Grid && self.grid_drag.is_some() {
                    // Rubber-band selection: once the pointer leaves a small dead-zone, the press
                    // becomes a marquee that live-selects every tile inside the dragged rectangle.
                    let (start, base, range, moved) = {
                        let d = self.grid_drag.as_ref().unwrap();
                        (d.start, d.base.clone(), d.range, d.moved)
                    };
                    if !range {
                        let (cx, cy) = (position.x as f32, position.y as f32);
                        let dist = ((cx - start.0).powi(2) + (cy - start.1).powi(2)).sqrt();
                        if moved || dist > 6.0 {
                            let (win_w, _) = self.win_size();
                            let n = self.nav.paths.len();
                            let tiles = self.grid.tiles_in_rect(start.0, start.1, cx, cy, win_w, n);
                            let mut sel = base;
                            sel.extend(tiles);
                            self.grid.selected = sel;
                            self.grid.marquee = Some((start.0, start.1, cx, cy));
                            if let Some(d) = self.grid_drag.as_mut() {
                                d.moved = true;
                            }
                            self.redraw();
                        }
                    }
                }
                self.cursor = position;
            }

            WindowEvent::RedrawRequested => self.draw(),

            // macOS "Open With" / Finder sends files via Apple Events → winit → DroppedFile.
            // Also handles literal drag-and-drop onto the window.
            WindowEvent::DroppedFile(path) => {
                self.open_file_path(&path.clone());
            }

            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();

        // macOS: drain any files queued by applicationOpenFile: (Finder "Open With").
        #[cfg(target_os = "macos")]
        {
            let pending: Vec<PathBuf> = crate::macos_open::PENDING_OPEN_PATHS
                .lock()
                .map(|mut v| v.drain(..).collect())
                .unwrap_or_default();
            for path in pending {
                self.open_file_path(&path);
            }
        }

        // Drain result from background set-default / unset-default task.
        if let Ok(mut slot) = crate::associate::ASSOC_RESULT.lock() {
            if let Some(msg) = slot.take() {
                self.show_status(&msg);
                self.redraw();
            }
        }

        // Grid thumbnail polling: redraw immediately so draw() drains the worker.
        if self.mode == ViewMode::Grid && self.nav.paths.iter().any(|p| self.thumbs.is_pending(p)) {
            self.redraw();
            event_loop.set_control_flow(ControlFlow::WaitUntil(now + Duration::from_millis(80)));
            return;
        }

        // Slideshow: advance when the timer fires.
        if let Some(t) = self.slideshow_next {
            if now >= t {
                if let Some(res) = self.nav.next() {
                    self.apply_nav_result(res);
                }
                self.slideshow_next = Some(now + self.slideshow_interval);
            }
        }

        // Expire the overlay timer.
        if let Some(t) = self.overlay_until {
            if now >= t {
                self.overlay_until = None;
                self.redraw();
            }
        }

        // Schedule the next wake-up at the earliest pending deadline.
        // Expire the status overlay timer.
        if let Some(t) = self.status_until {
            if Instant::now() >= t {
                self.status_until = None;
                self.redraw();
            }
        }

        // Apply any completed background SVG re-raster (decoding runs off the UI thread — see
        // `spawn_svg_rerender_worker` — so a large/complex SVG can't stall zoom or redraw).
        // Discard replies whose generation is stale (superseded by a newer request) or that
        // arrive after navigating away from that image.
        while let Ok((gen, path, result)) = self.svg_rerender_rx.try_recv() {
            if gen != self.svg_rerender_gen {
                continue;
            }
            self.svg_rerender_inflight = false;
            if !self.current_is_svg || self.nav.current_path() != Some(&path) {
                continue;
            }
            match result {
                Ok(img) => {
                    if let Some(gpu) = self.gpu.as_mut() {
                        gpu.set_image(&img);
                    }
                    self.redraw();
                }
                Err(e) => {
                    eprintln!("glanvu: SVG re-raster failed for {}: {e}", path.display());
                }
            }
        }

        // Apply the completed SVG viewport render (ignore replies from superseded requests).
        while let Ok((gen, col, row, result)) = self.tile_rx.try_recv() {
            if gen != self.svg_vp_gen {
                continue;
            }
            let Some((rx, ry, rw, rh, ps)) = self.svg_vp_pending else {
                continue;
            };
            self.svg_vp_pending = None;
            match result {
                Ok(img) => {
                    let key = (gen, col, row);
                    if let Some(gpu) = self.gpu.as_mut() {
                        gpu.clear_svg_tiles(); // keep only the newest viewport texture
                        gpu.upload_svg_tile(key, &img, gen);
                    }
                    self.svg_vp_region = Some((rx, ry, rw, rh));
                    self.svg_vp_scale = ps;
                    self.svg_vp_key = key;
                    self.redraw();
                }
                Err(e) => eprintln!("glanvu: SVG viewport render failed: {e}"),
            }
        }

        // SVG base layer re-raster: settle → render the WHOLE image at fit-to-window resolution
        // (bounded, cheap, independent of zoom). Zoomed-in sharpness is provided by the viewport
        // tiles composited on top; the base is just the always-present fallback, so it must never
        // grow to `iw*zoom` (that whole-image render at deep zoom was slow and blocked navigation).
        if let Some(t) = self.svg_rerender_at {
            if now >= t {
                self.svg_rerender_at = None;
                if let Some(path) = self.nav.current_path().cloned() {
                    let (win_w, win_h) = self.win_size();
                    let (iw, ih) = (self.img_size.0.max(1) as f32, self.img_size.1.max(1) as f32);
                    let fit = fit_scale(self.img_size, (win_w, win_h), self.state.quarter_turns);
                    let max_dim = self
                        .gpu
                        .as_ref()
                        .map(|g| g.device.limits().max_texture_dimension_2d)
                        .unwrap_or(8192);
                    let target_w = ((iw * fit).round().max(1.0) as u32).min(max_dim);
                    let target_h = ((ih * fit).round().max(1.0) as u32).min(max_dim);
                    self.svg_rerender_gen += 1;
                    self.svg_rerender_inflight = true;
                    self.svg_rerender_mailbox
                        .post((self.svg_rerender_gen, path, target_w, target_h));
                }
            }
        }

        // Decide whether to (re)render the SVG deep-zoom viewport now that events have settled.
        // Returns a wakeup while debouncing a scale change so this re-runs to enqueue after settle.
        let vp_win = self
            .gpu
            .as_ref()
            .map(|g| (g.config.width as f32, g.config.height as f32))
            .unwrap_or((800.0, 600.0));
        let vp_debounce_deadline = self.plan_svg_viewport(vp_win);

        let svg_poll_deadline = self.svg_rerender_inflight.then(|| now + SVG_RERENDER_POLL);
        // Keep polling while the SVG viewport render is still in flight in the background.
        let tile_poll_deadline = self.svg_vp_pending.is_some().then(|| now + SVG_RERENDER_POLL);

        let deadline = [
            self.overlay_until,
            self.slideshow_next,
            self.status_until,
            self.svg_rerender_at,
            svg_poll_deadline,
            tile_poll_deadline,
            vp_debounce_deadline,
        ]
        .into_iter()
        .flatten()
        .min();
        match deadline {
            Some(t) => event_loop.set_control_flow(ControlFlow::WaitUntil(t)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run(path: &str) -> ExitCode {
    let start = Instant::now();
    let first_path = PathBuf::from(path);

    let first_image = match glanvu_core::decode_path(&first_path) {
        Ok(img) => img,
        Err(e) => {
            eprintln!("glanvu: cannot open {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let img_size = (first_image.width, first_image.height);
    let current_is_svg = glanvu_core::is_svg_path(&first_path);

    let dir = first_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut paths = glanvu_core::list_images(&dir);
    let index = locate(&paths, &first_path).unwrap_or_else(|| {
        paths = vec![first_path.clone()];
        0
    });

    let nav = FolderNav::new(paths, index, first_path, first_image);
    let (svg_rerender_mailbox, svg_rerender_rx) = spawn_svg_rerender_worker();
    let (tile_queue, tile_rx) = spawn_tile_worker();

    let event_loop = match EventLoop::new() {
        Ok(el) => el,
        Err(e) => {
            eprintln!("glanvu: cannot start event loop: {e}");
            return ExitCode::FAILURE;
        }
    };
    event_loop.set_control_flow(ControlFlow::Wait);

    // macOS: patch the (now-registered) WinitApplicationDelegate to handle "Open With" before the
    // app runs, so the kAEOpenDocuments Apple Event delivered during launch is handled (queued).
    #[cfg(target_os = "macos")]
    crate::macos_open::install();

    let mut app = App {
        start,
        nav,
        img_size,
        state: ViewState::fit(),
        mode: ViewMode::Single,
        grid: GridState::new(index),
        thumbs: ThumbnailCache::new(),
        window: None,
        gpu: None,
        first_frame: false,
        cursor: PhysicalPosition::new(0.0, 0.0),
        dragging: false,
        overlay_until: None,
        slideshow_next: None,
        slideshow_interval: Duration::from_secs(3),
        status_text: String::new(),
        status_until: None,
        explorer: None,
        help_visible: false,
        info_visible: false,
        modifiers: ModifiersState::empty(),
        grid_drag: None,
        about_visible: false,
        confirm_assoc: None,
        confirm_delete: None,
        rename: None,
        confirm_rename: None,
        find: None,
        last_grid_click: None,
        sort_mode: glanvu_viewer_core::nav::SortMode::default(),
        date_text: String::new(),
        current_is_svg,
        svg_rerender_at: None,
        svg_rerender_gen: 0,
        svg_rerender_inflight: false,
        svg_rerender_mailbox,
        svg_rerender_rx,
        svg_doc: None,
        svg_vp_region: None,
        svg_vp_scale: 0.0,
        svg_vp_key: (0, 0, 0),
        svg_vp_pending: None,
        svg_vp_gen: 0,
        svg_vp_last_scale: 0.0,
        svg_vp_settled_at: Instant::now(),
        tile_queue,
        tile_rx,
    };

    match event_loop.run_app(&mut app) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("glanvu: viewer error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Launch the viewer without an initial file.
///
/// Used when the app is opened without arguments (double-click on .app, Dock, Spotlight).
/// A dark window appears with a "drop or press Enter" prompt; any file received via
/// `DroppedFile` (macOS Apple Events from "Open With") or by pressing Enter opens normally.
pub fn run_empty() -> ExitCode {
    let start = Instant::now();

    // Placeholder 1×1 black image — replaced as soon as a real file arrives.
    let placeholder = DecodedImage {
        width: 1,
        height: 1,
        rgba: vec![18, 18, 20, 255],
    };
    let placeholder_path = PathBuf::from("(none)");
    let nav = FolderNav::new(
        vec![placeholder_path.clone()],
        0,
        placeholder_path,
        placeholder,
    );
    let (svg_rerender_mailbox, svg_rerender_rx) = spawn_svg_rerender_worker();
    let (tile_queue, tile_rx) = spawn_tile_worker();

    let event_loop = match EventLoop::new() {
        Ok(el) => el,
        Err(e) => {
            eprintln!("glanvu: cannot start event loop: {e}");
            return ExitCode::FAILURE;
        }
    };
    event_loop.set_control_flow(ControlFlow::Wait);

    // macOS: patch the (now-registered) WinitApplicationDelegate to handle "Open With" before the
    // app runs, so the kAEOpenDocuments Apple Event delivered during launch is handled (queued).
    #[cfg(target_os = "macos")]
    crate::macos_open::install();

    let mut app = App {
        start,
        nav,
        img_size: (1, 1),
        state: ViewState::fit(),
        mode: ViewMode::Empty, // special empty state
        grid: GridState::new(0),
        thumbs: ThumbnailCache::new(),
        window: None,
        gpu: None,
        first_frame: false,
        cursor: PhysicalPosition::new(0.0, 0.0),
        dragging: false,
        overlay_until: None,
        slideshow_next: None,
        slideshow_interval: Duration::from_secs(3),
        status_text: String::new(),
        status_until: None,
        explorer: None,
        help_visible: false,
        info_visible: false,
        modifiers: ModifiersState::empty(),
        grid_drag: None,
        about_visible: false,
        confirm_assoc: None,
        confirm_delete: None,
        rename: None,
        confirm_rename: None,
        find: None,
        last_grid_click: None,
        sort_mode: glanvu_viewer_core::nav::SortMode::default(),
        date_text: String::new(),
        current_is_svg: false,
        svg_rerender_at: None,
        svg_rerender_gen: 0,
        svg_rerender_inflight: false,
        svg_rerender_mailbox,
        svg_rerender_rx,
        svg_doc: None,
        svg_vp_region: None,
        svg_vp_scale: 0.0,
        svg_vp_key: (0, 0, 0),
        svg_vp_pending: None,
        svg_vp_gen: 0,
        svg_vp_last_scale: 0.0,
        svg_vp_settled_at: Instant::now(),
        tile_queue,
        tile_rx,
    };

    match event_loop.run_app(&mut app) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("glanvu: viewer error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        human_size, image_scale, pad_region, region_covers, tile_mvp, visible_image_rect,
        TextInput, ViewState,
    };

    fn approx(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() <= eps
    }

    #[test]
    fn visible_rect_is_whole_image_at_fit() {
        // Fit view: the whole image is visible (letterboxed), so the visible rect == full image,
        // even when window and image aspect ratios differ.
        let st = ViewState::fit();
        let r = visible_image_rect((1000, 500), (1000.0, 800.0), &st);
        assert!(approx(r.0, 0.0, 0.5) && approx(r.1, 0.0, 0.5), "origin {r:?}");
        assert!(approx(r.2, 1000.0, 0.5) && approx(r.3, 500.0, 0.5), "size {r:?}");
    }

    #[test]
    fn visible_rect_is_centered_half_at_zoom_2() {
        // Zoom 2 (no pan) on a square-fit setup shows the centered middle half of the image.
        let st = ViewState {
            fit: true,
            zoom: 2.0,
            pan: (0.0, 0.0),
            quarter_turns: 0,
        };
        let r = visible_image_rect((1000, 800), (1000.0, 800.0), &st);
        assert!(approx(r.0, 250.0, 1.0) && approx(r.1, 200.0, 1.0), "origin {r:?}");
        assert!(approx(r.2, 500.0, 1.0) && approx(r.3, 400.0, 1.0), "size {r:?}");
    }

    #[test]
    fn pad_region_expands_and_clamps() {
        // Interior rect: padded by 20% each side.
        let r = pad_region((400.0, 300.0, 100.0, 100.0), 0.20, (1000, 800));
        assert!(approx(r.0, 380.0, 0.01) && approx(r.1, 280.0, 0.01), "origin {r:?}");
        assert!(approx(r.2, 140.0, 0.01) && approx(r.3, 140.0, 0.01), "size {r:?}");
        // Rect near the edge: padding is clamped to the image bounds (no negative / overflow).
        let e = pad_region((0.0, 0.0, 100.0, 100.0), 0.20, (1000, 800));
        assert!(approx(e.0, 0.0, 0.01) && approx(e.1, 0.0, 0.01), "clamped origin {e:?}");
        assert!(e.0 + e.2 <= 1000.01 && e.1 + e.3 <= 800.01);
    }

    #[test]
    fn region_covers_containment() {
        let outer = (100.0, 100.0, 200.0, 200.0);
        assert!(region_covers(outer, (120.0, 120.0, 100.0, 100.0))); // inside
        assert!(region_covers(outer, outer)); // equal
        assert!(!region_covers(outer, (90.0, 120.0, 100.0, 100.0))); // spills left
        assert!(!region_covers(outer, (120.0, 120.0, 200.0, 50.0))); // spills right
    }

    #[test]
    fn whole_image_tile_mvp_equals_base_mvp() {
        // A tile covering the entire image must produce exactly the same transform as `mvp`,
        // guaranteeing tiles overlay the base layer without drift.
        let st = ViewState {
            fit: true,
            zoom: 3.0,
            pan: (40.0, -25.0),
            quarter_turns: 1,
        };
        let img = (800, 600);
        let win = (1280.0, 720.0);
        let base = super::mvp(img, win, &st);
        let whole = tile_mvp(img, (0.0, 0.0, 800.0, 600.0), win, &st);
        for c in 0..4 {
            for r in 0..4 {
                assert!(
                    approx(base[c][r], whole[c][r], 1e-3),
                    "mvp mismatch at [{c}][{r}]: {} vs {}",
                    base[c][r],
                    whole[c][r]
                );
            }
        }
    }

    #[test]
    fn image_scale_matches_fit_and_zoom() {
        let fit = ViewState::fit();
        // Square image in square window: base = 1, scale = zoom.
        assert!(approx(image_scale((100, 100), (100.0, 100.0), &fit), 1.0, 1e-4));
        let z = ViewState {
            fit: false,
            zoom: 2.5,
            pan: (0.0, 0.0),
            quarter_turns: 0,
        };
        assert!(approx(image_scale((100, 100), (500.0, 500.0), &z), 2.5, 1e-4));
    }

    #[test]
    fn text_input_edits_and_caret() {
        let mut t = TextInput::new("photo");
        assert_eq!(t.text(), "photo");
        assert_eq!(t.cursor, 5); // cursor starts at end
        t.backspace();
        assert_eq!(t.text(), "phot");
        t.insert('o');
        assert_eq!(t.text(), "photo");
        t.home();
        t.insert('X'); // insert at start
        assert_eq!(t.text(), "Xphoto");
        assert_eq!(t.cursor, 1);
        t.delete(); // delete char after cursor ('p')
        assert_eq!(t.text(), "Xhoto");
        t.right();
        t.insert('_'); // "Xh_oto"
        assert_eq!(t.text(), "Xh_oto");
        t.home();
        assert_eq!(t.display_with_caret(), "|Xh_oto");
        t.end();
        t.right(); // no-op past end
        assert_eq!(t.cursor, t.text().chars().count());
    }

    #[test]
    fn human_size_scales_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(1_048_576), "1.0 MB");
        assert_eq!(human_size(1_073_741_824), "1.0 GB");
    }
}
