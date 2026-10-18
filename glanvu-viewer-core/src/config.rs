// SPDX-License-Identifier: Apache-2.0

//! User preferences and persistence (`config.toml`).
//!
//! Stores user configuration across sessions:
//! - `sort_mode`: playlist ordering (`"name_asc"` or `"date_desc"`).
//! - `text_font_size`: font size for document viewing (`13`, `16`, `19`, `23`, `28` px).
//! - `slideshow_interval_secs`: interval between slides (seconds).
//! - `start_in_grid`: whether the viewer opens in grid view by default.
//! - `line_numbers`: line numbers display toggle for text/code documents.
//! - `word_wrap`: word wrap toggle for text/code documents.
//!
//! Storage location:
//! - macOS/Linux: `~/.config/glanvu/config.toml` (or `$XDG_CONFIG_HOME/glanvu/config.toml`).
//! - Windows: `%LOCALAPPDATA%\glanvu\config.toml`.
//!
//! Missing files or corrupt keys fall back gracefully to defaults.
//! Saves are written atomically via temporary file + rename to prevent data loss.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::nav::SortMode;

/// User configuration options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserConfig {
    /// Sort mode for folder navigation and grid view.
    pub sort_mode: SortMode,
    /// Text viewer font size in pixels.
    pub text_font_size: u32,
    /// Slideshow advance interval in seconds.
    pub slideshow_interval_secs: u64,
    /// Whether to start in thumbnail grid view instead of single-image view.
    pub start_in_grid: bool,
    /// Whether line numbers are enabled in text and code documents.
    pub line_numbers: bool,
    /// Whether word wrap is enabled in text and code documents.
    pub word_wrap: bool,
}

impl Default for UserConfig {
    fn default() -> Self {
        Self {
            sort_mode: SortMode::NameAsc,
            text_font_size: glanvu_core::DEFAULT_FONT_SIZE,
            slideshow_interval_secs: 3,
            start_in_grid: false,
            line_numbers: true,
            word_wrap: true,
        }
    }
}

impl UserConfig {
    /// Index in `glanvu_core::FONT_SIZES` corresponding to `text_font_size`.
    pub fn text_font_size_idx(&self) -> usize {
        glanvu_core::FONT_SIZES
            .iter()
            .position(|&s| s == self.text_font_size)
            .unwrap_or(glanvu_core::DEFAULT_FONT_SIZE_INDEX)
    }

    /// Set font size from an index into `glanvu_core::FONT_SIZES`.
    pub fn set_text_font_size_idx(&mut self, idx: usize) {
        if let Some(&size) = glanvu_core::FONT_SIZES.get(idx) {
            self.text_font_size = size;
        }
    }

    /// Default path to `config.toml` on the current platform.
    pub fn config_path() -> PathBuf {
        #[cfg(target_os = "windows")]
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("C:\\glanvu_cache"));

        #[cfg(not(target_os = "windows"))]
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from("/tmp"));

        base.join("glanvu").join("config.toml")
    }

    /// Human-friendly display string for the config file path, replacing `$HOME` with `~`.
    pub fn display_path() -> String {
        let path = Self::config_path();
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            if let Ok(rel) = path.strip_prefix(&home) {
                return format!("~/{}", rel.display());
            }
        }
        path.display().to_string()
    }

    /// Parse configuration from a TOML string, falling back to defaults for missing or invalid values.
    pub fn from_toml_str(content: &str) -> Self {
        let mut config = Self::default();

        for line in content.lines() {
            let line = line.trim();
            // Skip comments and empty lines
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            // Split into key and value
            let Some((raw_key, raw_val)) = line.split_once('=') else {
                continue;
            };

            let key = raw_key.trim();
            // Strip inline comment if present
            let val = raw_val.split('#').next().unwrap_or("").trim();
            let clean_val = val.trim_matches('"').trim_matches('\'').trim();

            match key {
                "sort_mode" => match clean_val.to_ascii_lowercase().as_str() {
                    "date" | "date_desc" | "datedesc" => config.sort_mode = SortMode::DateDesc,
                    "name" | "name_asc" | "nameasc" => config.sort_mode = SortMode::NameAsc,
                    _ => {}
                },
                "text_font_size" => {
                    if let Ok(size) = clean_val.parse::<u32>() {
                        if (10..=40).contains(&size) {
                            if let Some(&closest) = glanvu_core::FONT_SIZES
                                .iter()
                                .min_by_key(|&&s| (s as i32 - size as i32).abs())
                            {
                                config.text_font_size = closest;
                            }
                        }
                    }
                }
                "slideshow_interval_secs" | "slideshow_interval" => {
                    if let Ok(secs) = clean_val.parse::<u64>() {
                        config.slideshow_interval_secs = secs.clamp(1, 3600);
                    }
                }
                "start_in_grid" | "start_view" => match clean_val.to_ascii_lowercase().as_str() {
                    "grid" | "true" | "1" => config.start_in_grid = true,
                    "single" | "false" | "0" => config.start_in_grid = false,
                    _ => {}
                },
                "line_numbers" => {
                    if let Ok(b) = clean_val.parse::<bool>() {
                        config.line_numbers = b;
                    }
                }
                "word_wrap" => {
                    if let Ok(b) = clean_val.parse::<bool>() {
                        config.word_wrap = b;
                    }
                }
                _ => {}
            }
        }

        config
    }

    /// Serialize configuration to a formatted TOML string with documentation comments.
    pub fn to_toml_string(&self) -> String {
        let sort_str = match self.sort_mode {
            SortMode::NameAsc => "name_asc",
            SortMode::DateDesc => "date_desc",
        };
        let start_str = if self.start_in_grid { "grid" } else { "single" };

        format!(
            "# Glanvu configuration file\n\
             # Location: {}\n\n\
             # Sort mode for folder navigation and grid view: \"name_asc\" or \"date_desc\"\n\
             sort_mode = \"{sort_str}\"\n\n\
             # Font size in pixels for text and code documents: 13, 16, 19, 23, 28\n\
             text_font_size = {}\n\n\
             # Slideshow interval in seconds\n\
             slideshow_interval_secs = {}\n\n\
             # Starting view mode: \"single\" or \"grid\"\n\
             start_view = \"{start_str}\"\n\n\
             # Show line numbers in text and code documents\n\
             line_numbers = {}\n\n\
             # Enable word wrap in text and code documents\n\
             word_wrap = {}\n",
            Self::display_path(),
            self.text_font_size,
            self.slideshow_interval_secs,
            self.line_numbers,
            self.word_wrap,
        )
    }

    /// Load user configuration from disk, falling back to defaults if not found or unreadable.
    pub fn load() -> Self {
        Self::load_from_path(&Self::config_path())
    }

    /// Load configuration from a specific file path.
    pub fn load_from_path(path: &Path) -> Self {
        if let Ok(content) = fs::read_to_string(path) {
            Self::from_toml_str(&content)
        } else {
            Self::default()
        }
    }

    /// Save configuration atomically to disk.
    pub fn save(&self) -> io::Result<()> {
        Self::save_to_path(self, &Self::config_path())
    }

    /// Save configuration atomically to a specific path.
    pub fn save_to_path(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let file_stem = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("config");
        let tmp_file_name = format!(".{file_stem}.{pid}.{nanos}.tmp");
        let tmp_path = path.with_file_name(tmp_file_name);
        let content = self.to_toml_string();
        fs::write(&tmp_path, content)?;
        if let Err(e) = fs::rename(&tmp_path, path) {
            let _ = fs::remove_file(&tmp_path);
            return Err(e);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_values() {
        let cfg = UserConfig::default();
        assert_eq!(cfg.sort_mode, SortMode::NameAsc);
        assert_eq!(cfg.text_font_size, 19);
        assert_eq!(cfg.slideshow_interval_secs, 3);
        assert!(!cfg.start_in_grid);
        assert!(cfg.line_numbers);
        assert!(cfg.word_wrap);
        assert_eq!(
            cfg.text_font_size_idx(),
            glanvu_core::DEFAULT_FONT_SIZE_INDEX
        );
    }

    #[test]
    fn parse_valid_toml() {
        let toml = r#"
            # My settings
            sort_mode = "date_desc"
            text_font_size = 22 # larger text
            slideshow_interval_secs = 5
            start_view = "grid"
            line_numbers = false
            word_wrap = false
        "#;
        let cfg = UserConfig::from_toml_str(toml);
        assert_eq!(cfg.sort_mode, SortMode::DateDesc);
        assert_eq!(cfg.text_font_size, 22);
        assert_eq!(cfg.slideshow_interval_secs, 5);
        assert!(cfg.start_in_grid);
        assert!(!cfg.line_numbers);
        assert!(!cfg.word_wrap);
    }

    #[test]
    fn parse_partial_toml_with_missing_keys_keeps_defaults() {
        let toml = r#"
            sort_mode = "date_desc"
        "#;
        let cfg = UserConfig::from_toml_str(toml);
        assert_eq!(cfg.sort_mode, SortMode::DateDesc);
        assert_eq!(cfg.text_font_size, 19);
        assert_eq!(cfg.slideshow_interval_secs, 3);
        assert!(!cfg.start_in_grid);
        assert!(cfg.line_numbers);
        assert!(cfg.word_wrap);
    }

    #[test]
    fn parse_corrupted_values_falls_back() {
        let toml = r#"
            sort_mode = "invalid_mode"
            text_font_size = 9999
            slideshow_interval_secs = 0
            line_numbers = "not_a_bool"
        "#;
        let cfg = UserConfig::from_toml_str(toml);
        assert_eq!(cfg.sort_mode, SortMode::NameAsc);
        assert_eq!(cfg.text_font_size, 19);
        assert_eq!(cfg.slideshow_interval_secs, 1); // clamped to min 1
        assert!(cfg.line_numbers);
    }

    #[test]
    fn roundtrip_serialization() {
        let cfg = UserConfig {
            sort_mode: SortMode::DateDesc,
            text_font_size: 28,
            slideshow_interval_secs: 10,
            start_in_grid: true,
            line_numbers: false,
            word_wrap: false,
        };

        let toml_str = cfg.to_toml_string();
        let parsed = UserConfig::from_toml_str(&toml_str);
        assert_eq!(cfg, parsed);
    }

    #[test]
    fn save_and_load_disk() {
        let tmp_dir = std::env::temp_dir().join("glanvu_test_config");
        let _ = fs::create_dir_all(&tmp_dir);
        let path = tmp_dir.join("test_config.toml");

        let cfg = UserConfig {
            sort_mode: SortMode::DateDesc,
            text_font_size: 16,
            slideshow_interval_secs: 7,
            ..UserConfig::default()
        };
        cfg.save_to_path(&path).expect("save failed");

        let loaded = UserConfig::load_from_path(&path);
        assert_eq!(cfg, loaded);

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir_all(&tmp_dir);
    }
}
