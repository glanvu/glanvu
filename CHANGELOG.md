# Changelog

All notable changes to Glanvu are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versioning follows [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.8.0] — 2026-07-02

### Added

- **PDF support** — open multi-page PDFs like any other image (rendered via the native PDFium
  library, Glanvu's first non-pure-Rust runtime dependency — see D13 in the decision log), fit to
  the window preserving the document's own aspect ratio. `↑`/`↓` turn pages within the document
  (clamped at the first/last page, no wraparound); `←`/`→` still walk the folder. A "page N/M"
  indicator (top-right) appears alongside the filename overlay. One thumbnail per PDF in the grid
  (page 1). `glanvu convert` rasterizes page 1 (a note is printed for multi-page sources); PDF is
  input-only, like SVG. Included in file association / "Open With" / `glanvu set-default`.

### Changed

- The default/empty-state window is less cramped and square: the minimum window size grew from
  320×240 to 640×360 (near 16:9 instead of 4:3), which also affects any image small enough to hit
  that floor.

### Fixed

- The native "Open file" dialog's default filter now offers `.svg` and `.pdf` alongside the other
  image types (it had its own extension list, separate from folder scanning and file association,
  that was missed when each format landed).

## [0.7.0] — 2026-07-01

### Added

- **SVG support** — the first vector format. Rendered crisp at every zoom level: the GPU scales
  the current raster smoothly during an active zoom/pan/resize gesture (free, unchanged), and
  once it settles, Glanvu re-rasterizes at the new effective on-screen resolution in the
  background so large or complex files never stall the UI. Supported everywhere: the viewer,
  thumbnail grid (rasterized directly at thumbnail size, so it's sharp rather than
  intrinsic-then-downscaled), `glanvu info`, `glanvu convert` (input only — no raster→vector),
  and system file association (`glanvu set-default` / macOS "Open With").
- **SVG stays sharp when zoomed in (viewport rendering).** When zoomed in past fit, Glanvu renders
  only the visible region (plus a small pad) at screen resolution, composited over a cheap
  fit-resolution base layer, on a background thread so the UI never stalls. Small pans within the
  pad are free; the render scale is capped so even filter-heavy SVGs stay responsive. Vector-only —
  raster images are unaffected.

### Fixed

- Mouse-wheel zoom now anchors to the cursor position instead of the image center, matching
  every other viewer/editor's scroll-to-zoom behavior.
- Large images no longer look soft or aliased when fit to the window (or zoomed out). Image
  textures now carry a full mipmap chain and are sampled trilinearly, so downscaled detail is
  filtered correctly instead of a single-texel tap; previously a large photo looked rough until
  you zoomed toward 100%.

## [0.6.1] — 2026-07-01

### Fixed

- macOS bundles are now ad-hoc signed, so Gatekeeper shows the standard "unidentified
  developer" prompt with an **Open Anyway** button in System Settings → Privacy & Security,
  instead of the "damaged and can't be opened" dead-end (no bypass) that unsigned bundles hit
  on Sonoma and later once quarantined by a browser or Homebrew download.

## [0.6.0] — 2026-06-21

A feature release for the viewer: file management (rename, delete to Trash),
search, multi-selection, and folder awareness — plus a redesigned help overlay.

### Added

- **Find by name** (`F` or `/`) — fuzzy subsequence search within the folder. In
  single view it shows a floating list of the best matches; in the grid it is a
  live filter: a search bar plus only the matching thumbnails, with a 2D cursor.
  `Enter` opens the highlighted image, `Esc` clears.
- **File-info overlay** (`I`) — a translucent top-left panel with the current
  image's name, dimensions, format, size and modified date. Stays open while you
  navigate.
- **Move to Trash** (`Delete` / `Backspace`) — sends the image to the system
  recycle bin (not a permanent delete), behind a confirmation modal.
- **Grid multi-selection** — `Shift`+click/arrows for a range, `Ctrl`/`⌘`+click
  or `Space` to toggle one, `Ctrl`/`⌘`+`A` to select all, and click-and-drag for
  a rubber-band marquee. `Delete` moves the whole selection to Trash.
- **Rename** (`R`) — inline editor pre-filled with the current name; a name
  collision moves the displaced file to Trash after confirmation.
- **Folder awareness** — the playlist re-scans on window focus, picking up files
  added or removed externally and detecting content changes (by mtime). `F5`
  forces a full refresh (re-scan + drop all caches).
- **Sort in the grid** (`O`) — toggle name/date order from the grid while keeping
  the current selection.

### Changed

- **Help overlay** redesigned into a compact two-column layout with labelled,
  colour-accented sections (Navigate, View, Organize, Grid selection, App), and
  now documents the grid selection shortcuts.
- **Rotate** moved to `T` (turn); `R` is now rename.

## [0.5.4] — 2026-06-20

Packaging fixes, shipped end-to-end through the automated release pipeline. The
application itself is unchanged from 0.5.3 — this release exists to deliver
corrected distribution manifests cleanly.

### Fixed

- **Scoop:** corrected the Windows download URL. The `/download/vX/` path lagged
  behind the bumped filename, producing a 404 on `scoop install`.
- **winget:** declare the `Microsoft.VCRedist.2015+.x64` runtime dependency the
  MSVC build links against (requested during winget validation).

## [0.5.3] — 2026-06-20

Windows polish release: fixes the three issues Windows users hit on first launch.
macOS and Linux behaviour is unchanged.

### Fixed

- **Windows: no more stray console window.** Release builds now use the GUI
  subsystem, so launching Glanvu (double-click or "Open with") no longer leaves a
  `cmd`/console window open behind the viewer. CLI subcommands (`glanvu convert`,
  `info`, `--help`) still print normally when run from a terminal — the process
  reattaches to the parent console at startup.
- **Windows: Glanvu now appears under "Open with".** The app registers itself in
  `HKCU\Software\Classes` (Applications entry + `SupportedTypes` + per-extension
  `OpenWithProgids`), so right-click → "Open with" lists Glanvu for image files.
- **Windows: `set-default` / the `D` and `U` keys now work.** Glanvu registers a
  ProgID and opens Settings → Default apps for confirmation. Windows guards the real
  default behind a per-user hash, so — as on macOS — the OS owns the final step.

### Added

- **macOS Intel (x86_64) build.** A native Intel `.app` is now published alongside
  the Apple Silicon build; Homebrew installs the right one automatically.

## [0.5.0] — 2026-06-15

First public release. The core viewer and batch pipeline are complete and
tested. Packaged installers are in progress; for now, build from source.

### Viewer

- GPU-accelerated image viewer (wgpu + winit) for JPEG, PNG, WebP, GIF, BMP, TIFF.
- Keyboard-first navigation: arrow keys walk the folder; `Home`/`End` jump to
  first/last; `0` fits to window; `1` actual size; `R` rotates 90°.
- Zoom and pan with `+`/`−`, mouse wheel, and drag.
- Fullscreen with `Space`, `F`, or `F11`.
- Filename + dimensions overlay; sort toggle (`O`) cycles name ↔ date order.
- Copy image to clipboard (`C`); copy file path (`Shift+C`).
- Two-column keyboard cheatsheet (`H` / `?`).
- Slideshow (`S`): auto-advances at configurable interval, wraps at end.
- Thumbnail grid (`Tab` / `G`): scrollable folder overview, keyboard navigation,
  Enter or double-click to open.
- Directory explorer (`Enter`): side panel for folder-level browsing.
- Version label in empty-state watermark.

### macOS integration

- `.app` bundle with `Info.plist`, UTType file-type associations, and drag-and-drop.
- `glanvu set-default` registers Glanvu as the default viewer for all supported
  image types via `NSWorkspace` (`D` in-viewer, `U` to restore previous defaults).
- "Open With" support in Finder.

### Batch convert (`glanvu convert`)

Headless, no GPU. Pipeline order: **crop → rotate → resize → encode**.

- `--to FMT` — target format: jpg, png, gif, bmp, tiff, webp (required).
- `--crop X,Y,WxH` — extract a region before any other step.
- `--rotate 90|180|270|-90` — fixed-angle rotation (clockwise; negative = CCW).
- `--resize WxH` — fit within bounding box, aspect ratio preserved (Lanczos3).
- `--quality 1-100` — JPEG quality (JPEG output only; warns and ignores for
  other formats).
- `--rename PATTERN` — output name pattern: `{stem}`, `{n}`, `{n:04}`.
- `--out DIR` — output directory (default: next to each input).
- Parallel execution (Rayon — all CPU cores).
- Refuses to overwrite inputs or produce colliding outputs (data-loss guard).

### Architecture

Three-crate Cargo workspace:

- **`glanvu-core`** — decode, normalize, convert. No GPU, no window.
- **`glanvu-viewer-core`** — pure viewer state (nav, thumbnails, grid,
  explorer). No GPU, no window; depends only on `glanvu-core`.
- **`glanvu`** — the viewer (winit + wgpu, glyphon text overlay) and the CLI.

Workspace enforces `unsafe_code = deny` and `clippy::all = warn`.

---

[Unreleased]: https://github.com/glanvu/glanvu/compare/v0.8.0...HEAD
[0.8.0]: https://github.com/glanvu/glanvu/releases/tag/v0.8.0
[0.5.0]: https://github.com/glanvu/glanvu/releases/tag/v0.5.0
