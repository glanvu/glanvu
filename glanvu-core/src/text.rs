// SPDX-License-Identifier: Apache-2.0

//! Plain text rendering support for Glanvu.
//!
//! Formats like `.txt` and `.log` are paginated into documents with a dark theme, line numbers,
//! and clean monospace typography, rasterized to [`DecodedImage`] via the existing vector pipeline.

use std::path::Path;

use crate::decode::{decode_svg_bytes, DecodedImage};
use crate::error::{Error, Result};

pub const TEXT_PAGE_WIDTH: u32 = 1200;
pub const TEXT_PAGE_HEIGHT: u32 = 1600;
const LINES_PER_PAGE: usize = 58;
const WRAP_COLS: usize = 92;
const TAB_SIZE: usize = 4;

/// A visual row on a rendered page.
#[derive(Debug, Clone)]
struct VisualRow {
    /// 1-based source line number, or `None` if this is a wrapped continuation row.
    line_number: Option<usize>,
    /// Text to display on this row.
    text: String,
}

/// A parsed plain text document, paginated and ready to render.
#[derive(Debug, Clone)]
pub struct TextDocument {
    content: String,
    pages: Vec<Vec<VisualRow>>,
}

impl TextDocument {
    /// Parse and paginate a text document from in-memory UTF-8 bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.contains(&0) {
            return Err(Error::Decode(
                "binary file with null bytes is not plain text".to_string(),
            ));
        }
        let content = std::str::from_utf8(bytes)
            .map_err(|e| Error::Decode(format!("invalid UTF-8 text: {e}")))?
            .to_string();
        Ok(Self::from_string(content))
    }

    /// Load and paginate a text document from disk.
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let bytes = std::fs::read(path.as_ref()).map_err(|source| Error::Io {
            path: path.as_ref().to_path_buf(),
            source,
        })?;
        Self::from_bytes(&bytes)
    }

    /// Build a paginated document from an already-decoded UTF-8 string.
    pub fn from_string(content: String) -> Self {
        let mut visual_rows = Vec::new();

        for (line_idx, raw_line) in content.lines().enumerate() {
            let cleaned = sanitize_line(raw_line);
            let expanded = expand_tabs(&cleaned, TAB_SIZE);
            if expanded.is_empty() {
                visual_rows.push(VisualRow {
                    line_number: Some(line_idx + 1),
                    text: String::new(),
                });
            } else {
                let chunks = wrap_line(&expanded, WRAP_COLS);
                for (chunk_idx, chunk) in chunks.into_iter().enumerate() {
                    visual_rows.push(VisualRow {
                        line_number: if chunk_idx == 0 {
                            Some(line_idx + 1)
                        } else {
                            None
                        },
                        text: chunk,
                    });
                }
            }
        }

        if visual_rows.is_empty() {
            visual_rows.push(VisualRow {
                line_number: Some(1),
                text: String::new(),
            });
        }

        let pages = visual_rows
            .chunks(LINES_PER_PAGE)
            .map(|c| c.to_vec())
            .collect();

        Self { content, pages }
    }

    /// Raw text content of the document.
    pub fn content(&self) -> &str {
        &self.content
    }

    /// Number of pages in this document (always >= 1).
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Dimensions in pixels of a rendered page.
    pub fn page_size(&self, _page_index: usize) -> Result<(f32, f32)> {
        Ok((TEXT_PAGE_WIDTH as f32, TEXT_PAGE_HEIGHT as f32))
    }

    /// Generate the SVG representation of a given page (0-based).
    pub fn page_svg(&self, page_index: usize) -> Result<String> {
        let rows = self
            .pages
            .get(page_index)
            .ok_or_else(|| Error::Decode(format!("page {page_index} out of range")))?;

        let mut svg = String::with_capacity(8192);
        svg.push_str(&format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="{TEXT_PAGE_WIDTH}" height="{TEXT_PAGE_HEIGHT}" viewBox="0 0 {TEXT_PAGE_WIDTH} {TEXT_PAGE_HEIGHT}">
<rect width="100%" height="100%" fill="#121214"/>
<line x1="92" y1="25" x2="92" y2="{}" stroke="#27272a" stroke-width="1"/>
"##,
            TEXT_PAGE_HEIGHT - 25
        ));

        let top_y = 35;
        let line_height = 26;

        for (row_idx, row) in rows.iter().enumerate() {
            let y = top_y + (row_idx as i32 + 1) * line_height;
            if let Some(num) = row.line_number {
                svg.push_str(&format!(
                    r##"<text x="80" y="{y}" text-anchor="end" fill="#52525b" font-family="ui-monospace, Menlo, Monaco, Consolas, monospace" font-size="16">{num}</text>
"##
                ));
            }
            if !row.text.is_empty() {
                let escaped = escape_xml(&row.text);
                svg.push_str(&format!(
                    r##"<text x="110" y="{y}" fill="#e4e4e7" font-family="ui-monospace, Menlo, Monaco, Consolas, monospace" font-size="19" xml:space="preserve">{escaped}</text>
"##
                ));
            }
        }

        svg.push_str("</svg>");
        Ok(svg)
    }

    /// Render a page (0-based) at explicit output dimensions `(out_w, out_h)`.
    pub fn render_page(&self, page_index: usize, out_w: u32, out_h: u32) -> Result<DecodedImage> {
        let svg = self.page_svg(page_index)?;
        decode_svg_bytes(svg.as_bytes(), Some((out_w, out_h)))
    }

    /// Render a page (0-based) at its native resolution (1200x1600).
    pub fn render_fit(&self, page_index: usize) -> Result<DecodedImage> {
        self.render_page(page_index, TEXT_PAGE_WIDTH, TEXT_PAGE_HEIGHT)
    }
}

fn expand_tabs(s: &str, tab_size: usize) -> String {
    let mut out = String::with_capacity(s.len());
    let mut col = 0;
    for c in s.chars() {
        if c == '\t' {
            let spaces = tab_size - (col % tab_size);
            for _ in 0..spaces {
                out.push(' ');
            }
            col += spaces;
        } else {
            out.push(c);
            col += 1;
        }
    }
    out
}

fn wrap_line(line: &str, max_cols: usize) -> Vec<String> {
    if line.chars().count() <= max_cols {
        return vec![line.to_string()];
    }

    let mut result = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let mut start = 0;

    while start < chars.len() {
        let end = (start + max_cols).min(chars.len());
        if end == chars.len() {
            result.push(chars[start..end].iter().collect());
            break;
        }

        // Try breaking at last space if within the last 20% of the chunk
        let search_start = start + max_cols.saturating_sub(max_cols / 5);
        let break_pos = chars[search_start..end]
            .iter()
            .rposition(|&c| c.is_whitespace())
            .map(|pos| search_start + pos + 1);

        let actual_end = break_pos.unwrap_or(end);
        result.push(chars[start..actual_end].iter().collect());
        start = actual_end;
    }

    result
}

/// Strip ANSI escape sequences and non-printable control characters so terminal logs render cleanly.
fn sanitize_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next(); // consume '['
            while let Some(&next) = chars.peek() {
                if (0x20..=0x3F).contains(&(next as u32)) {
                    chars.next();
                } else {
                    break;
                }
            }
            if let Some(&next) = chars.peek() {
                if (0x40..=0x7E).contains(&(next as u32)) {
                    chars.next();
                }
            }
            continue;
        }
        if c == '\t' || !c.is_control() {
            out.push(c);
        }
    }
    out
}

fn escape_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if is_valid_xml_char(c) => out.push(c),
            _ => {} // Drop disallowed XML 1.0 control characters
        }
    }
    out
}

fn is_valid_xml_char(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_document_renders_page() {
        let doc = TextDocument::from_string("Hello, Glanvu!\nLine 2\nLine 3".to_string());
        assert_eq!(doc.page_count(), 1);
        let img = doc.render_fit(0).unwrap();
        assert_eq!(img.width, TEXT_PAGE_WIDTH);
        assert_eq!(img.height, TEXT_PAGE_HEIGHT);
        assert!(img.rgba.as_chunks::<4>().0.iter().any(|p| p[0] > 100));
    }

    #[test]
    fn text_document_paginates_long_content() {
        let lines: Vec<String> = (1..=120).map(|i| format!("Log entry {i}")).collect();
        let doc = TextDocument::from_string(lines.join("\n"));
        assert_eq!(doc.page_count(), 3); // 120 lines / 58 = 3 pages
    }

    #[test]
    fn text_document_handles_ansi_and_control_chars() {
        let content = "\x1b[31;1m[ERROR]\x1b[0m Failed with code \x07\x1b[33m404\x1b[0m";
        let doc = TextDocument::from_string(content.to_string());
        assert_eq!(doc.page_count(), 1);
        assert_eq!(doc.pages[0][0].text, "[ERROR] Failed with code 404");
        // Must rasterize to SVG without error (control chars dropped, ANSI stripped)
        let img = doc.render_fit(0).unwrap();
        assert_eq!(img.width, TEXT_PAGE_WIDTH);
    }

    #[test]
    fn text_document_rejects_null_bytes() {
        let err = TextDocument::from_bytes(b"hello\0world").unwrap_err();
        assert!(matches!(err, Error::Decode(_)));
    }

    #[test]
    fn text_document_wraps_long_lines() {
        let long_line = "A".repeat(250);
        let doc = TextDocument::from_string(long_line);
        assert_eq!(doc.pages[0].len(), 3);
        assert_eq!(doc.pages[0][0].line_number, Some(1));
        assert_eq!(doc.pages[0][1].line_number, None);
        assert_eq!(doc.pages[0][2].line_number, None);
    }
}
