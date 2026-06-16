// SPDX-License-Identifier: Apache-2.0

//! Directory explorer panel — pure state, no GPU.
//!
//! Shows the contents of a directory (parent `../`, sub-directories, supported images) as a
//! scrollable list. The viewer renders the panel as a semi-transparent overlay on the left side.

use std::path::{Path, PathBuf};

use glanvu_core::is_supported_path;

/// Width of the explorer panel in physical pixels.
pub const PANEL_W: f32 = 260.0;
/// Height of each entry row.
pub const LINE_H: f32 = 22.0;
/// Font size for entries (matches the help overlay font so all text feels consistent).
pub const FONT: f32 = 15.0;
/// Top of the first entry (header occupies this space).
pub const HEADER_H: f32 = 30.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    /// Goes to the parent directory.
    Parent,
    /// Opens a sub-directory (reloads explorer in that dir).
    Dir,
    /// Opens an image file.
    Image,
}

#[derive(Debug, Clone)]
pub struct Entry {
    /// Display label shown in the panel.
    pub label: String,
    pub path: PathBuf,
    pub kind: EntryKind,
}

/// What the caller should do when the user activates an entry.
pub enum OpenResult {
    /// Open this image in the viewer and close the explorer.
    OpenImage(PathBuf),
    /// Navigate into this directory; refresh the explorer in-place.
    NavigateDir(PathBuf),
    Nothing,
}

pub struct ExplorerState {
    pub dir: PathBuf,
    entries: Vec<Entry>,
    pub sel: usize,
    pub scroll_y: f32,
    /// Computed panel width in physical pixels (adapts to the longest label).
    pub panel_w: f32,
}

impl ExplorerState {
    /// Build the explorer for the directory containing `current_file`.
    /// Pre-selects `current_file` in the list when found.
    pub fn for_path(current_file: &Path) -> Self {
        let dir = current_file
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        let mut state = ExplorerState {
            dir,
            entries: Vec::new(),
            sel: 0,
            scroll_y: 0.0,
            panel_w: PANEL_W,
        };
        state.reload(Some(current_file));
        state
    }

    /// Navigate into `dir`; reset selection and scroll.
    pub fn set_dir(&mut self, dir: PathBuf) {
        self.dir = dir;
        self.sel = 0;
        self.scroll_y = 0.0;
        self.panel_w = PANEL_W; // reset before reload recomputes
        self.reload(None);
    }

    fn reload(&mut self, current: Option<&Path>) {
        self.entries.clear();

        // Parent entry.
        if let Some(parent) = self.dir.parent().filter(|p| !p.as_os_str().is_empty()) {
            self.entries.push(Entry {
                label: "../".to_string(),
                path: parent.to_path_buf(),
                kind: EntryKind::Parent,
            });
        }

        let mut dirs: Vec<(String, PathBuf)> = Vec::new();
        let mut images: Vec<(String, PathBuf)> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for entry in rd.filter_map(|e| e.ok()) {
                let path = entry.path();
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if name.starts_with('.') {
                    continue;
                }
                if path.is_dir() {
                    dirs.push((name, path));
                } else if is_supported_path(&path) {
                    images.push((name, path));
                }
            }
        }
        dirs.sort_by_key(|(n, _)| n.to_ascii_lowercase());
        images.sort_by_key(|(n, _)| n.to_ascii_lowercase());

        let current_name = current
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned());

        for (name, path) in dirs {
            self.entries.push(Entry {
                label: format!("{name}/"),
                path,
                kind: EntryKind::Dir,
            });
        }
        for (name, path) in images {
            let is_cur = current_name.as_deref() == Some(name.as_str());
            let label = if is_cur {
                format!("> {name}")
            } else {
                name.clone()
            };
            self.entries.push(Entry {
                label,
                path,
                kind: EntryKind::Image,
            });
        }

        // Pre-select the currently open file.
        if let Some(cn) = &current_name {
            if let Some(pos) = self.entries.iter().position(|e| {
                e.kind == EntryKind::Image
                    && e.path.file_name().map(|n| n.to_string_lossy()) == Some(cn.as_str().into())
            }) {
                self.sel = pos;
            }
        }

        // Adapt panel width to the longest label (approximate: 9px/char at 15px sans-serif).
        let dir_label_len = self.dir_label().len() + 1; // +1 for trailing /
        let max_label = self
            .entries
            .iter()
            .map(|e| e.label.len())
            .max()
            .unwrap_or(0)
            .max(dir_label_len);
        const CHAR_W: f32 = 9.0;
        const PAD: f32 = 28.0; // 8px left + 20px right margin
        self.panel_w = (max_label as f32 * CHAR_W + PAD).clamp(180.0, 460.0);
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn move_sel(&mut self, delta: isize) {
        let n = self.entries.len();
        if n == 0 {
            return;
        }
        self.sel = (self.sel as isize + delta).clamp(0, n as isize - 1) as usize;
    }

    pub fn scroll_to_sel(&mut self, panel_h: f32) {
        let abs_top = HEADER_H + self.sel as f32 * LINE_H;
        let screen_top = abs_top - self.scroll_y;
        let screen_bot = screen_top + LINE_H;
        if screen_top < HEADER_H {
            self.scroll_y = abs_top - HEADER_H;
        } else if screen_bot > panel_h - 4.0 {
            self.scroll_y = abs_top + LINE_H - (panel_h - 4.0);
        }
        let total_h = HEADER_H + self.entries.len() as f32 * LINE_H;
        let max_scroll = (total_h - panel_h).max(0.0);
        self.scroll_y = self.scroll_y.clamp(0.0, max_scroll);
    }

    pub fn open_sel(&self) -> OpenResult {
        let Some(e) = self.entries.get(self.sel) else {
            return OpenResult::Nothing;
        };
        match e.kind {
            EntryKind::Image => OpenResult::OpenImage(e.path.clone()),
            EntryKind::Dir | EntryKind::Parent => OpenResult::NavigateDir(e.path.clone()),
        }
    }

    /// Screen y-coordinate (y-down) of entry `i` (includes header offset and scroll).
    pub fn entry_y(&self, i: usize) -> f32 {
        HEADER_H + i as f32 * LINE_H - self.scroll_y
    }

    /// Entry index at screen position `my` (`None` if in the header or out of range).
    pub fn hit_entry(&self, my: f32) -> Option<usize> {
        if my < HEADER_H {
            return None;
        }
        let i = ((my - HEADER_H + self.scroll_y) / LINE_H) as usize;
        if i < self.entries.len() {
            Some(i)
        } else {
            None
        }
    }

    /// Short display name for the current directory (used as panel header).
    pub fn dir_label(&self) -> String {
        self.dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/".to_string())
    }

    /// Build the full panel text: header line + one line per entry.
    pub fn panel_text(&self) -> String {
        let mut s = self.dir_label();
        s.push('/');
        for e in &self.entries {
            s.push('\n');
            s.push_str(&e.label);
        }
        s
    }
}
