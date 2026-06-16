// SPDX-License-Identifier: Apache-2.0

//! Listing the viewable images in a folder, for navigation (next/prev).

use std::path::{Path, PathBuf};

/// File extensions Glanvu can currently open (the Phase 1 base set), lowercase, without the dot.
const SUPPORTED_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "gif", "bmp", "tif", "tiff", "webp"];

/// Whether a path has a supported image extension (case-insensitive).
pub fn is_supported_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| SUPPORTED_EXTENSIONS.contains(&e.as_str()))
}

/// List the supported image files directly inside `dir`, sorted case-insensitively by file name.
///
/// Returns an empty vector if the directory cannot be read. Does not recurse.
pub fn list_images(dir: &Path) -> Vec<PathBuf> {
    let mut images: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file() && is_supported_path(p))
            .collect(),
        Err(_) => Vec::new(),
    };
    images.sort_by_key(|p| {
        p.file_name()
            .map(|n| n.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default()
    });
    images
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_only_images_sorted_case_insensitively() {
        let dir = std::env::temp_dir().join("glanvu-folder-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["B.png", "a.JPG", "notes.txt", "c.webp", "sub"] {
            if name == "sub" {
                std::fs::create_dir_all(dir.join(name)).unwrap();
            } else {
                std::fs::write(dir.join(name), b"x").unwrap();
            }
        }

        let names: Vec<String> = list_images(&dir)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();

        assert_eq!(names, vec!["a.JPG", "B.png", "c.webp"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extension_check_is_case_insensitive() {
        assert!(is_supported_path(Path::new("/x/PHOTO.JPEG")));
        assert!(is_supported_path(Path::new("/x/a.WebP")));
        assert!(!is_supported_path(Path::new("/x/a.txt")));
        assert!(!is_supported_path(Path::new("/x/noext")));
    }
}
