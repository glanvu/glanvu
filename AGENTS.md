# AGENTS.md — Glanvu

Operating guide for AI agents and contributors working in this repository.

## What this project is

**Glanvu** is a fast, keyboard-driven, **cross-platform image viewer and converter** (Linux, macOS,
Windows). GPU-accelerated, tiny footprint, keyboard-first navigation, and a real batch-convert
pipeline. Currently in **Phase 2** — see [Phase status](#phase-status) below.

The longer-term north star is a **universal viewer**: photos, RAW, DICOM, CAD/3D, gigapixel images,
multipage documents — one tool that opens anything instantly. The image viewer is the foundation.

## Documentation home

The deep internal WIP — feasibility analysis, decisions, roadmap, plans, marketing and business model —
lives in the owner's private Obsidian vault, **not** in this repo:

- Vault home: `WIP/glanvu/` (entry: `glanvu.README`, index: `doc/glanvu.doc-index`).

This repository carries only **publishable** docs (`README.md`, `AGENTS.md`, and code + crate
READMEs). Do not commit product strategy, business-model notes, or competitive analysis here.

## Architecture

Glanvu is a **Cargo workspace** with three crates:

```
glanvu-core/          (library) — image decode, normalize, convert. No GPU, no window.
glanvu-viewer-core/   (library) — pure viewer state (nav, thumbnails, grid, explorer). No GPU.
glanvu/               (binary)  — viewer (winit + wgpu) + CLI (convert, info).
```

### glanvu-core

Pure image logic; all modules are GPU-free and fully unit-testable without a display.

| Module | Responsibility |
|---|---|
| `decode` | Decode files/bytes → `DecodedImage` (RGBA8) + `ImageMeta`. Content-based format detection. |
| `format` | `SourceFormat` enum + extension/image-crate conversions. |
| `folder` | List + sort supported images in a directory (`list_images`). |
| `convert` | Decode → crop → rotate → optional resize (Lanczos3, aspect-preserving) → encode. |

### glanvu-viewer-core

Pure viewer-state layer; no GPU, no window; depends only on `glanvu-core`.

| Module | Responsibility |
|---|---|
| `nav` | `FolderNav`: sorted playlist, bounded prefetch cache, background decode worker. `SortMode` (NameAsc/DateDesc toggle). |
| `thumb` | `ThumbnailCache`: in-memory + disk thumbnail cache. |
| `grid` | `GridState`: thumbnail grid layout. |
| `explorer` | `ExplorerState`: directory tree navigation. |

### glanvu (binary)

| Module | Responsibility |
|---|---|
| `main` | Dispatch: open viewer / `info` / `convert` / `set-default`. |
| `viewer` | `Gpu` (wgpu pipeline + glyphon overlay) + `App` (winit event handler). Delegates image state to `FolderNav`. |
| `batch` | `glanvu convert` parser + rayon parallel execution. |
| `associate` | `glanvu set-default` — platform default-handler registration. macOS-native; Linux/Windows stubbed. |

### Architecture boundary (important)

`glanvu-viewer-core` is the boundary between **image state** and **GPU/window state**. This exists so:
1. Navigation logic is unit-testable without a GPU.
2. The future hosted web service can reuse `glanvu-viewer-core` + `glanvu-core` without dragging in
   the windowing layer.

**Never let GPU types leak into `glanvu-viewer-core` or `glanvu-core`.**

## Engineering principles

- **Original / clean-room work.** Glanvu is an independent implementation. Never copy, decompile, or
  transcribe third-party proprietary code, resources, icons, or assets into this project.
- **Performance is the product.** Instant load, tiny footprint, keyboard-first. Decode/render is
  designed as a reusable library core; broad format support comes through a sandboxed plugin layer.
- **Simplicity first.** Every change as simple as possible. Modify only what is strictly necessary;
  no refactoring or cleanup beyond the task scope.
- **No backward-compat shims.** This is not a published library yet. Remove unused code outright.

## Coding conventions

- **License header**: `// SPDX-License-Identifier: Apache-2.0` on every new source file.
- **Lints**: workspace enforces `clippy::all = warn` and `unsafe_code = deny`. No exceptions without a
  reviewed `#[allow(...)]` and a comment explaining why.
- **Format**: `cargo fmt --all` (max_width = 100). Run before every commit.
- **Tests**: pure logic (decode, nav, convert) gets unit tests with temporary files. GPU/window code
  is tested via manual smoke-runs. Do not add GPU logic to modules that should stay testable.
- **Comments**: only when the *why* is non-obvious. No "what the code does" comments.
- **Owner handles git**: do not commit, push, or branch unless explicitly asked.

## Security notes (pre-server)

These apply now (local CLI) and will matter more once the server layer lands:

- **Path traversal in batch**: `glanvu convert --out DIR` accepts any path the user provides.
  For a CLI tool this is intentional (the user controls their own system). When a server layer
  is added, the `--out` equivalent must be sandboxed to a per-request working directory.
- **Input validation**: the `convert` command validates that output paths do not collide with each
  other (data-loss guard) and that output does not equal input (overwrite guard). Extend both checks
  when adding new batch operations.
- **Crop bounds**: crop region is validated against the decoded dimensions (`x+w <= W && y+h <= H`).
  Preserve this check when modifying the crop pipeline.
- **Quality range**: `--quality` is clamped to `1..=100` before being passed to the JPEG encoder.
- **Resize dimensions**: `MAX_RESIZE_DIM = 32_768` is enforced in `convert.rs`. Maintain it; the
  future server layer makes runaway allocation a remote-exploit surface.
- **Codec exposure**: all Phase 1 decoders are pure-Rust (no system C libraries). AVIF/HEVC are
  excluded from Phase 1 (they need `dav1d` C). When adding C-backed decoders, track their CVEs and
  ensure they are sandboxed (glycin-style process isolation, Phase 2+).
- **`unsafe_code = deny`**: the workspace already enforces this. Maintain it.

## Building and distribution

```bash
make build          # debug build
make release        # release build
make app            # build macOS .app bundle → dist/macos/Glanvu.app
make install-app    # install to /Applications/
```

### macOS .app

`scripts/build-macos-app.sh` packages the release binary into a proper `.app` bundle with
`Info.plist` (bundle ID `com.glanvu.app`, file type associations for JPEG/PNG/WebP/GIF/BMP/TIFF).

First-run Gatekeeper warning: right-click → Open (until the app is codesigned + notarized with an
Apple Developer account).

### Brew tap (future publishing)

Formula: `dist/brew/Casks/glanvu.rb` → push to `github.com/glanvu/homebrew-glanvu`.
Users: `brew tap glanvu/glanvu && brew install --cask glanvu`.

## Running the project

```bash
cargo run -- <IMAGE_PATH>          # open viewer
cargo run -- info <IMAGE_PATH>     # print format/dims
cargo run -- convert --to webp --resize 2000x2000 --out out/ photos/*.jpg
cargo run -- set-default           # set Glanvu as default for all image types (macOS)
cargo run -- set-default --list    # show current default per type
cargo run -- --help

# With perf logging (init + per-switch timing):
GLANVU_PERF=1 cargo run --release -- <IMAGE_PATH>

# Tests, lint, format:
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

## Phase status

- **Phase 1 (MVP)**: complete. Viewer + folder nav + overlay + batch convert.
- **Phase 2**: in progress.

  | Item | Status |
  |---|---|
  | Code quality pass | done |
  | Thumbnail grid (`Tab` / `G`) | done |
  | Slideshow (`S`) | done |
  | Directory explorer (`Enter`) | done |
  | Double-click grid → open image | done |
  | macOS `.app` bundle | done |
  | Set as default (`D` / `U`, `glanvu set-default`) | done (macOS) |
  | Help overlay (`H` / `?`, two-column) | done |
  | Copy to clipboard (`C` image, `Shift+C` path) | done |
  | viewer-core extraction | done |
  | Batch enhancements (crop, rotate, quality, rename) | done |
  | Brew cask + Linux/Windows installers | planned |
  | Auto-update | planned |
  | Web app (glanvu.com) | planned (Phase 3) |

## Writing conventions

- Avoid em/en-dash separators in prose; plain punctuation.
- Shared artifacts in English; Spanish only in personal WIP vault files.
