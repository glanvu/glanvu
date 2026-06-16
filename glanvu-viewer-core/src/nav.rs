// SPDX-License-Identifier: Apache-2.0

//! Folder-navigation state: sorted playlist, prefetch cache, background decode worker.
//!
//! `FolderNav` is the pure image-state layer — no GPU, no window. It owns the playlist of images
//! in the current folder, the bounded prefetch cache, and the channel to the background decode
//! worker. The viewer (`viewer.rs`) holds a `FolderNav` and calls `show_index` / `next` / `prev`
//! to drive navigation; the GPU layer (`Gpu`) is told about the new image separately.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Instant;

use glanvu_core::DecodedImage;

/// How many neighbors on each side to keep decoded / prefetched.
pub const PREFETCH_RADIUS: usize = 2;

/// Order in which images are listed in the folder.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum SortMode {
    #[default]
    NameAsc,
    DateDesc,
}

impl SortMode {
    pub fn next(self) -> Self {
        match self {
            Self::NameAsc => Self::DateDesc,
            Self::DateDesc => Self::NameAsc,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::NameAsc => "Sorted by name",
            Self::DateDesc => "Sorted by date",
        }
    }
}

/// Outcome of `show_index`: tells the caller what changed.
pub struct ShowResult {
    pub img_size: (u32, u32),
    pub path: PathBuf,
    pub index: usize,
    pub total: usize,
    pub cache_hit: bool,
    pub elapsed_ms: f64,
}

/// Playlist + prefetch cache for the currently open folder.
pub struct FolderNav {
    pub paths: Vec<PathBuf>,
    pub index: usize,
    cache: HashMap<PathBuf, DecodedImage>,
    in_flight: HashSet<PathBuf>,
    prefetch_tx: Sender<PathBuf>,
    prefetch_rx: Receiver<(PathBuf, DecodedImage)>,
}

impl FolderNav {
    /// Create a new `FolderNav` for `paths`, with `initial` already decoded and pre-loaded.
    /// `start_index` is the position of `initial_path` in `paths`.
    pub fn new(
        paths: Vec<PathBuf>,
        start_index: usize,
        initial_path: PathBuf,
        initial: DecodedImage,
    ) -> Self {
        let (prefetch_tx, req_rx) = mpsc::channel::<PathBuf>();
        let (res_tx, prefetch_rx) = mpsc::channel::<(PathBuf, DecodedImage)>();
        std::thread::spawn(move || {
            while let Ok(p) = req_rx.recv() {
                if let Ok(img) = glanvu_core::decode_path(&p) {
                    if res_tx.send((p, img)).is_err() {
                        break;
                    }
                }
            }
        });
        let mut cache = HashMap::new();
        cache.insert(initial_path, initial);
        FolderNav {
            paths,
            index: start_index,
            cache,
            in_flight: HashSet::new(),
            prefetch_tx,
            prefetch_rx,
        }
    }

    /// The path of the image currently on screen.
    pub fn current_path(&self) -> Option<&PathBuf> {
        self.paths.get(self.index)
    }

    /// The decoded image currently on screen (None before the first `show_index`).
    pub fn current_image(&self) -> Option<&DecodedImage> {
        self.cache.get(self.paths.get(self.index)?)
    }

    /// Pull any results from the background worker into the cache.
    pub fn drain_prefetch(&mut self) {
        while let Ok((path, image)) = self.prefetch_rx.try_recv() {
            self.in_flight.remove(&path);
            self.cache.insert(path, image);
        }
    }

    /// Evict entries outside the prefetch window; request any missing neighbors.
    pub fn prune_and_prefetch(&mut self) {
        let n = self.paths.len();
        if n == 0 {
            return;
        }
        let lo = self.index.saturating_sub(PREFETCH_RADIUS);
        let hi = (self.index + PREFETCH_RADIUS).min(n - 1);
        let keep: HashSet<PathBuf> = (lo..=hi).map(|i| self.paths[i].clone()).collect();
        self.cache.retain(|k, _| keep.contains(k));
        self.in_flight.retain(|k| keep.contains(k));

        for i in lo..=hi {
            if i == self.index {
                continue;
            }
            let p = &self.paths[i];
            if !self.cache.contains_key(p) && !self.in_flight.contains(p) {
                self.in_flight.insert(p.clone());
                let _ = self.prefetch_tx.send(p.clone());
            }
        }
    }

    /// Navigate to `idx`. Decodes synchronously on a cache miss. Returns `None` if the index is
    /// out of range or the decode fails (the caller keeps showing the current image).
    pub fn show_index(&mut self, idx: usize) -> Option<ShowResult> {
        let n = self.paths.len();
        if n == 0 {
            return None;
        }
        let idx = idx.min(n - 1);
        let t0 = Instant::now();
        self.drain_prefetch();

        let path = self.paths[idx].clone();
        let cache_hit = self.cache.contains_key(&path);
        if !cache_hit {
            match glanvu_core::decode_path(&path) {
                Ok(img) => {
                    self.cache.insert(path.clone(), img);
                }
                Err(e) => {
                    eprintln!("glanvu: skipping {}: {e}", path.display());
                    return None;
                }
            }
        }

        self.index = idx;
        self.prune_and_prefetch();

        let img = self.cache.get(&path)?;
        Some(ShowResult {
            img_size: (img.width, img.height),
            path,
            index: idx,
            total: n,
            cache_hit,
            elapsed_ms: t0.elapsed().as_secs_f64() * 1000.0,
        })
    }

    /// Navigate to the next image (wraps around).
    // `next`/`prev` are the idiomatic names for a playlist cursor; `FolderNav` is not an iterator.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<ShowResult> {
        let n = self.paths.len();
        if n > 1 {
            self.show_index((self.index + 1) % n)
        } else {
            None
        }
    }

    /// Navigate to the previous image (wraps around).
    pub fn prev(&mut self) -> Option<ShowResult> {
        let n = self.paths.len();
        if n > 1 {
            self.show_index((self.index + n - 1) % n)
        } else {
            None
        }
    }

    /// Navigate to the first image.
    pub fn first(&mut self) -> Option<ShowResult> {
        self.show_index(0)
    }

    /// Navigate to the last image.
    pub fn last(&mut self) -> Option<ShowResult> {
        let n = self.paths.len();
        if n > 0 {
            self.show_index(n - 1)
        } else {
            None
        }
    }

    /// Re-sort the playlist and stay on the current image.
    pub fn resort(&mut self, mode: SortMode) {
        let current = self.current_path().cloned();
        sort_paths_by(&mut self.paths, mode);
        if let Some(path) = current {
            self.index = self
                .paths
                .iter()
                .position(|p| p == &path)
                .unwrap_or(self.index);
        }
        // Evict stale prefetch neighbors; new ones will be requested on the next draw.
        self.in_flight.clear();
        self.cache
            .retain(|k, _| self.paths.get(self.index).is_some_and(|p| p == k));
    }
}

fn sort_paths_by(paths: &mut [PathBuf], mode: SortMode) {
    match mode {
        SortMode::NameAsc => paths.sort_by(|a, b| {
            a.file_name()
                .unwrap_or_default()
                .to_ascii_lowercase()
                .cmp(&b.file_name().unwrap_or_default().to_ascii_lowercase())
        }),
        SortMode::DateDesc => paths.sort_by(|a, b| {
            let ta = std::fs::metadata(a).and_then(|m| m.modified()).ok();
            let tb = std::fs::metadata(b).and_then(|m| m.modified()).ok();
            tb.cmp(&ta)
        }),
    }
}

/// Find the position of `target` in `paths` by file name.
pub fn locate(paths: &[PathBuf], target: &Path) -> Option<usize> {
    let name = target.file_name()?;
    paths.iter().position(|p| p.file_name() == Some(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_nav(count: usize, start: usize) -> (FolderNav, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("glanvu-nav-test-{count}-{start}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let mut paths = Vec::new();
        for i in 0..count {
            let p = dir.join(format!("{i:04}.png"));
            let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([i as u8, 0, 0, 255]));
            image::DynamicImage::ImageRgba8(img).save(&p).unwrap();
            paths.push(p);
        }

        let initial_path = paths[start].clone();
        let initial = glanvu_core::decode_path(&initial_path).unwrap();
        let nav = FolderNav::new(paths, start, initial_path, initial);
        (nav, dir)
    }

    #[test]
    fn next_wraps_around() {
        let (mut nav, dir) = make_nav(3, 2);
        let res = nav.next().unwrap();
        assert_eq!(res.index, 0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn prev_wraps_around() {
        let (mut nav, dir) = make_nav(3, 0);
        let res = nav.prev().unwrap();
        assert_eq!(res.index, 2);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn first_and_last() {
        let (mut nav, dir) = make_nav(4, 2);
        assert_eq!(nav.first().unwrap().index, 0);
        assert_eq!(nav.last().unwrap().index, 3);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn single_image_nav_returns_none() {
        let (mut nav, dir) = make_nav(1, 0);
        assert!(nav.next().is_none());
        assert!(nav.prev().is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn locate_finds_by_filename() {
        let paths = vec![PathBuf::from("/a/foo.jpg"), PathBuf::from("/a/bar.png")];
        assert_eq!(locate(&paths, Path::new("/other/bar.png")), Some(1));
        assert_eq!(locate(&paths, Path::new("/other/missing.jpg")), None);
    }
}
