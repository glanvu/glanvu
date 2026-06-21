// SPDX-License-Identifier: Apache-2.0

//! `glanvu-viewer-core` is the pure viewer-state layer behind Glanvu's GUI: folder navigation
//! (playlist + prefetch), the thumbnail cache, grid-view layout, and the directory explorer.
//!
//! It sits between `glanvu-core` (headless image decode/convert) and the GPU/window layer in the
//! `glanvu` binary. Like `glanvu-core`, it is deliberately free of any GPU, windowing, or GUI code
//! so navigation and layout logic stay unit-testable without a GPU and reusable headlessly.
//!
//! **Never let GPU types leak into this crate.**

pub mod explorer;
pub mod find;
pub mod grid;
pub mod nav;
pub mod thumb;
