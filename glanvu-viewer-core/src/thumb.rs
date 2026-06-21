// SPDX-License-Identifier: Apache-2.0

//! Thumbnail generation, memory cache, and disk cache.
//!
//! `ThumbnailCache` owns the decoded thumbnail images (small versions of the folder's images). It
//! is separate from `FolderNav` (which owns full-resolution images + the prefetch cache); the two
//! caches have different sizes, lifetimes, and eviction strategies.
//!
//! Disk cache: `~/.cache/glanvu/thumbs/` (Linux/macOS) or `%LOCALAPPDATA%\glanvu\thumbs\`
//! (Windows). Cache keys encode the source path hash + mtime so stale entries are ignored.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

use glanvu_core::DecodedImage;

/// Maximum thumbnail size (fit-within, aspect preserved).
pub const THUMB_W: u32 = 160;
pub const THUMB_H: u32 = 120;

// ---------------------------------------------------------------------------
// Disk cache helpers
// ---------------------------------------------------------------------------

/// Return the disk cache directory for thumbnails, creating it on first use.
fn thumb_cache_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:\\glanvu_cache"));

    #[cfg(not(target_os = "windows"))]
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));

    base.join("glanvu").join("thumbs")
}

/// Cache key: `<path_hash>_<mtime_secs>.jpg`.
fn disk_key(path: &Path, mtime_secs: u64) -> String {
    let mut h = DefaultHasher::new();
    path.hash(&mut h);
    format!("{:016x}_{mtime_secs}.jpg", h.finish())
}

// ---------------------------------------------------------------------------
// Background thumbnail worker message
// ---------------------------------------------------------------------------

struct WorkerResult {
    path: PathBuf,
    image: DecodedImage,
}

// ---------------------------------------------------------------------------
// ThumbnailCache
// ---------------------------------------------------------------------------

/// In-memory + disk thumbnail cache with a background generation worker.
pub struct ThumbnailCache {
    mem: HashMap<PathBuf, DecodedImage>,
    in_flight: HashSet<PathBuf>,
    tx: Sender<PathBuf>,
    rx: Receiver<WorkerResult>,
}

impl Default for ThumbnailCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ThumbnailCache {
    pub fn new() -> Self {
        let cache_dir = thumb_cache_dir();
        let _ = std::fs::create_dir_all(&cache_dir);

        let (req_tx, req_rx) = mpsc::channel::<PathBuf>();
        let (res_tx, res_rx) = mpsc::channel::<WorkerResult>();
        let dir = cache_dir.clone();
        std::thread::spawn(move || {
            while let Ok(path) = req_rx.recv() {
                if let Some(image) = generate_thumb(&path, &dir) {
                    if res_tx.send(WorkerResult { path, image }).is_err() {
                        break;
                    }
                }
            }
        });

        ThumbnailCache {
            mem: HashMap::new(),
            in_flight: HashSet::new(),
            tx: req_tx,
            rx: res_rx,
        }
    }

    /// Get the thumbnail for `path` if it is already in the in-memory cache.
    pub fn get(&self, path: &Path) -> Option<&DecodedImage> {
        self.mem.get(path)
    }

    /// Ensure `path` is either cached or queued for generation.
    pub fn request(&mut self, path: &Path) {
        if self.mem.contains_key(path) || self.in_flight.contains(path) {
            return;
        }
        self.in_flight.insert(path.to_path_buf());
        let _ = self.tx.send(path.to_path_buf());
    }

    /// Pull completed thumbnails from the worker into the in-memory cache.
    /// Returns `true` if at least one new thumbnail arrived.
    pub fn drain(&mut self) -> bool {
        let mut got_any = false;
        while let Ok(r) = self.rx.try_recv() {
            self.in_flight.remove(&r.path);
            self.mem.insert(r.path, r.image);
            got_any = true;
        }
        got_any
    }

    /// Whether `path` has a thumbnail ready or is being generated.
    pub fn is_pending(&self, path: &Path) -> bool {
        self.in_flight.contains(path)
    }

    /// Forget the cached thumbnail for `path` so the next `request` regenerates it (used when the
    /// file changed on disk).
    pub fn invalidate(&mut self, path: &Path) {
        self.mem.remove(path);
        self.in_flight.remove(path);
    }

    /// Drop every cached thumbnail (manual full refresh / F5). Pending generations are forgotten;
    /// they will simply be ignored when they arrive.
    pub fn clear(&mut self) {
        self.mem.clear();
        self.in_flight.clear();
    }
}

// ---------------------------------------------------------------------------
// Thumbnail generation (runs in the worker thread)
// ---------------------------------------------------------------------------

fn generate_thumb(path: &Path, cache_dir: &Path) -> Option<DecodedImage> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let key = disk_key(path, mtime);
    let cached = cache_dir.join(&key);

    // Try disk cache first.
    if cached.exists() {
        if let Ok(img) = glanvu_core::decode_path(&cached) {
            return Some(img);
        }
        // Corrupt cache entry — delete and regenerate.
        let _ = std::fs::remove_file(&cached);
    }

    // Generate from the source image.
    let thumb = glanvu_core::decode_thumbnail(path, THUMB_W, THUMB_H).ok()?;

    // Save to disk cache (best-effort; failures are silent).
    let _ = glanvu_core::encode_to_file(&thumb, &cached, glanvu_core::SourceFormat::Jpeg);

    Some(thumb)
}
