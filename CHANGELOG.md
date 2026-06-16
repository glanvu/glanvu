# Changelog

All notable changes to Glanvu are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versioning follows [Semantic Versioning](https://semver.org/).

## [Unreleased]

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

[Unreleased]: https://github.com/glanvu/glanvu/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/glanvu/glanvu/releases/tag/v0.5.0
