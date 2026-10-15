// SPDX-License-Identifier: Apache-2.0

//! Listing the viewable images in a folder, for navigation (next/prev).

use std::path::{Path, PathBuf};

use crate::format::SourceFormat;

/// Whether a path has a supported image extension or filename (case-insensitive).
pub fn is_supported_path(path: &Path) -> bool {
    SourceFormat::from_path(path).is_some()
}

/// Whether a path's extension marks it as SVG — the one format whose decode strategy differs
/// enough (vector, no lazy header read, crisp-on-settle re-raster) that callers across the
/// viewer and batch CLI need to branch on it. Single source of truth for that check.
pub fn is_svg_path(path: &Path) -> bool {
    SourceFormat::from_path(path) == Some(SourceFormat::Svg)
}

/// Whether a path's extension marks it as PDF — the other format (besides SVG) whose decode
/// strategy is special-cased across the viewer and batch CLI: paginated, and rendered via the
/// native PDFium library rather than the `image` crate. Single source of truth for that check,
/// mirroring `is_svg_path`.
pub fn is_pdf_path(path: &Path) -> bool {
    SourceFormat::from_path(path) == Some(SourceFormat::Pdf)
}

/// Whether a path's extension marks it as a text-based format (.txt, .log, .json, .jsonl, .xml, .ini, .toml, .yaml, .yml, .md, .markdown, .cfg, .conf, .env, .properties) —
/// paginated and rendered with monospace typography and line numbers. Single source of truth for that check.
pub fn is_text_path(path: &Path) -> bool {
    matches!(
        SourceFormat::from_path(path),
        Some(
            SourceFormat::Txt
                | SourceFormat::Json
                | SourceFormat::Jsonl
                | SourceFormat::Xml
                | SourceFormat::Ini
                | SourceFormat::Toml
                | SourceFormat::Yaml
                | SourceFormat::Md
        )
    )
}

/// Whether a path's extension marks it as JSON (.json).
pub fn is_json_path(path: &Path) -> bool {
    SourceFormat::from_path(path) == Some(SourceFormat::Json)
}

/// Whether a path's extension marks it as XML (.xml).
pub fn is_xml_path(path: &Path) -> bool {
    SourceFormat::from_path(path) == Some(SourceFormat::Xml)
}

/// Whether a path's extension marks it as INI or related config (.ini, .cfg, .conf, .env, .properties).
pub fn is_ini_path(path: &Path) -> bool {
    SourceFormat::from_path(path) == Some(SourceFormat::Ini)
}

/// Whether a path's extension marks it as TOML (.toml).
pub fn is_toml_path(path: &Path) -> bool {
    SourceFormat::from_path(path) == Some(SourceFormat::Toml)
}

/// Whether a path's extension marks it as YAML (.yaml, .yml).
pub fn is_yaml_path(path: &Path) -> bool {
    SourceFormat::from_path(path) == Some(SourceFormat::Yaml)
}

/// Whether a path's extension marks it as Markdown (.md, .markdown).
pub fn is_md_path(path: &Path) -> bool {
    SourceFormat::from_path(path) == Some(SourceFormat::Md)
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
        for name in ["B.png", "a.JPG", "data.bin", "c.webp", "notes.txt", "sub"] {
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

        assert_eq!(names, vec!["a.JPG", "B.png", "c.webp", "notes.txt"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extension_check_is_case_insensitive() {
        assert!(is_supported_path(Path::new("/x/PHOTO.JPEG")));
        assert!(is_supported_path(Path::new("/x/a.WebP")));
        assert!(is_supported_path(Path::new("/x/a.pdf")));
        assert!(is_supported_path(Path::new("/x/a.TXT")));
        assert!(is_supported_path(Path::new("/x/sys.LOG")));
        assert!(is_supported_path(Path::new("/x/data.JSON")));
        assert!(is_supported_path(Path::new("/x/records.jsonl")));
        assert!(!is_supported_path(Path::new("/x/data.bin")));
        assert!(!is_supported_path(Path::new("/x/noext")));
    }

    #[test]
    fn is_pdf_path_is_case_insensitive() {
        assert!(is_pdf_path(Path::new("/x/doc.pdf")));
        assert!(is_pdf_path(Path::new("/x/doc.PDF")));
        assert!(!is_pdf_path(Path::new("/x/doc.svg")));
        assert!(!is_pdf_path(Path::new("/x/doc.png")));
    }

    #[test]
    fn is_text_path_is_case_insensitive() {
        assert!(is_text_path(Path::new("/x/notes.txt")));
        assert!(is_text_path(Path::new("/x/notes.TXT")));
        assert!(is_text_path(Path::new("/x/app.log")));
        assert!(is_text_path(Path::new("/x/app.LOG")));
        assert!(is_text_path(Path::new("/x/data.json")));
        assert!(is_text_path(Path::new("/x/data.JSON")));
        assert!(is_text_path(Path::new("/x/records.jsonl")));
        assert!(is_text_path(Path::new("/x/records.JSONL")));
        assert!(is_text_path(Path::new("/x/config.xml")));
        assert!(is_text_path(Path::new("/x/config.XML")));
        assert!(is_text_path(Path::new("/x/settings.ini")));
        assert!(is_text_path(Path::new("/x/settings.INI")));
        assert!(is_text_path(Path::new("/x/Cargo.toml")));
        assert!(is_text_path(Path::new("/x/Cargo.TOML")));
        assert!(is_text_path(Path::new("/x/setup.cfg")));
        assert!(is_text_path(Path::new("/x/nginx.conf")));
        assert!(is_text_path(Path::new("/x/.env")));
        assert!(is_text_path(Path::new("/x/.env.local")));
        assert!(is_text_path(Path::new("/x/app.properties")));
        assert!(is_text_path(Path::new("/x/.editorconfig")));
        assert!(is_text_path(Path::new("/x/.gitconfig")));
        assert!(!is_text_path(Path::new("/x/doc.pdf")));
    }

    #[test]
    fn is_json_path_is_case_insensitive() {
        assert!(is_json_path(Path::new("/x/data.json")));
        assert!(is_json_path(Path::new("/x/data.JSON")));
        assert!(!is_json_path(Path::new("/x/records.jsonl")));
        assert!(!is_json_path(Path::new("/x/notes.txt")));
    }

    #[test]
    fn is_xml_path_is_case_insensitive() {
        assert!(is_xml_path(Path::new("/x/doc.xml")));
        assert!(is_xml_path(Path::new("/x/doc.XML")));
        assert!(!is_xml_path(Path::new("/x/doc.svg")));
        assert!(!is_xml_path(Path::new("/x/data.json")));
    }

    #[test]
    fn is_ini_path_is_case_insensitive() {
        assert!(is_ini_path(Path::new("/x/config.ini")));
        assert!(is_ini_path(Path::new("/x/config.INI")));
        assert!(is_ini_path(Path::new("/x/setup.cfg")));
        assert!(is_ini_path(Path::new("/x/nginx.conf")));
        assert!(is_ini_path(Path::new("/x/.env")));
        assert!(is_ini_path(Path::new("/x/.env.local")));
        assert!(is_ini_path(Path::new("/x/app.properties")));
        assert!(!is_ini_path(Path::new("/x/notes.txt")));
    }

    #[test]
    fn is_toml_path_is_case_insensitive() {
        assert!(is_toml_path(Path::new("/x/Cargo.toml")));
        assert!(is_toml_path(Path::new("/x/Cargo.TOML")));
        assert!(!is_toml_path(Path::new("/x/config.ini")));
        assert!(!is_toml_path(Path::new("/x/notes.txt")));
    }

    #[test]
    fn is_yaml_path_is_case_insensitive() {
        assert!(is_yaml_path(Path::new("/x/docker-compose.yaml")));
        assert!(is_yaml_path(Path::new("/x/docker-compose.YAML")));
        assert!(is_yaml_path(Path::new("/x/ci.yml")));
        assert!(is_yaml_path(Path::new("/x/ci.YML")));
        assert!(!is_yaml_path(Path::new("/x/Cargo.toml")));
        assert!(!is_yaml_path(Path::new("/x/config.ini")));
    }

    #[test]
    fn is_md_path_is_case_insensitive() {
        assert!(is_md_path(Path::new("/x/README.md")));
        assert!(is_md_path(Path::new("/x/README.MD")));
        assert!(is_md_path(Path::new("/x/spec.markdown")));
        assert!(is_md_path(Path::new("/x/spec.MARKDOWN")));
        assert!(!is_md_path(Path::new("/x/Cargo.toml")));
        assert!(!is_md_path(Path::new("/x/config.yaml")));
    }

    #[test]
    fn bak_extensions_resolve_to_underlying_format() {
        assert!(is_supported_path(Path::new("/x/config.toml.bak")));
        assert!(is_toml_path(Path::new("/x/config.toml.bak")));
        assert!(is_text_path(Path::new("/x/config.toml.bak")));

        assert!(is_supported_path(Path::new("/x/.env.bak")));
        assert!(is_ini_path(Path::new("/x/.env.bak")));
        assert!(is_text_path(Path::new("/x/.env.bak")));

        assert!(is_supported_path(Path::new("/x/photo.png.bak")));
        assert!(is_supported_path(Path::new("/x/doc.pdf.bak")));
        assert!(is_pdf_path(Path::new("/x/doc.pdf.bak")));

        assert!(is_supported_path(Path::new("/x/vector.svg.bak")));
        assert!(is_svg_path(Path::new("/x/vector.svg.bak")));

        // Unsupported underlying format with .bak remains unsupported
        assert!(!is_supported_path(Path::new("/x/binary.bin.bak")));
        assert!(!is_supported_path(Path::new("/x/unknown.bak")));
    }
}
