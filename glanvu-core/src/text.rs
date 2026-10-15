// SPDX-License-Identifier: Apache-2.0

//! Plain text rendering support for Glanvu.
//!
//! Formats like `.txt` and `.log` are paginated into documents with a dark theme, line numbers,
//! and clean monospace typography, rasterized to [`DecodedImage`] via the existing vector pipeline.

use std::path::Path;

use crate::decode::{decode_svg_bytes, DecodedImage};
use crate::error::{Error, Result};
use crate::format::SourceFormat;

pub const TEXT_PAGE_WIDTH: u32 = 1200;
pub const TEXT_PAGE_HEIGHT: u32 = 1600;
const LINES_PER_PAGE: usize = 58;
const WRAP_COLS: usize = 92;
const TAB_SIZE: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum PartialDelim {
    #[default]
    None,
    /// Chunk ended with `<`
    OpenAngle,
    /// Chunk ended with `<!`
    Bang,
    /// Chunk ended with `<!-`
    CommentOpenDash,
    /// Inside comment, chunk ended with `-`
    CommentCloseDash,
    /// Inside comment, chunk ended with `--`
    CommentCloseDoubleDash,
    /// Chunk ended with `<![` .. `<![CDATA` (matched 3..=8 chars of `<![CDATA[`)
    CdataOpen(u8),
    /// Inside CDATA, chunk ended with `]`
    CdataCloseBracket,
    /// Inside CDATA, chunk ended with `]]`
    CdataCloseDoubleBracket,
}

/// A visual row on a rendered page.
#[derive(Debug, Clone)]
struct VisualRow {
    /// 1-based source line number, or `None` if this is a wrapped continuation row.
    line_number: Option<usize>,
    /// Text to display on this row.
    text: String,
    /// Number of columns of parent indentation to display before continuation text.
    indent_cols: usize,
    /// Whether this row starts inside an unclosed string literal from a previous wrapped row.
    starts_in_string: bool,
    /// Whether this row starts with the first character escaped by a trailing backslash.
    starts_escaped: bool,
    /// Whether this row starts inside an unclosed XML comment `<!-- ... -->` or INI/TOML comment.
    starts_in_comment: bool,
    /// Whether this row starts inside an unclosed XML opening tag `<tag ...`.
    starts_in_tag: bool,
    /// Whether this row starts inside an unclosed XML CDATA `<![CDATA[ ... ]]>`.
    starts_in_cdata: bool,
    /// Whether this row starts inside an unclosed INI/TOML section header `[...]` or `[[...]]`.
    starts_in_section: bool,
    /// Whether this row is a continuation chunk of an INI/TOML key-value line.
    is_value_continuation: bool,
    /// Partial XML delimiter carried from the end of the previous visual row.
    partial_delim: PartialDelim,
}

/// A parsed plain text document, paginated and ready to render.
#[derive(Debug, Clone)]
pub struct TextDocument {
    content: String,
    pages: Vec<Vec<VisualRow>>,
    format: SourceFormat,
}

impl TextDocument {
    /// Parse and paginate a text document from in-memory UTF-8 bytes with default Txt format.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        Self::from_bytes_with_format(bytes, SourceFormat::Txt)
    }

    /// Parse and paginate a text document from in-memory UTF-8 bytes with an explicit format.
    pub fn from_bytes_with_format(bytes: &[u8], format: SourceFormat) -> Result<Self> {
        if bytes.contains(&0) {
            return Err(Error::Decode(
                "binary file with null bytes is not plain text".to_string(),
            ));
        }
        let content = std::str::from_utf8(bytes)
            .map_err(|e| Error::Decode(format!("invalid UTF-8 text: {e}")))?
            .to_string();
        Ok(Self::from_string_with_format(content, format))
    }

    /// Load and paginate a text document from disk.
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let bytes = std::fs::read(path).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if bytes.contains(&0) {
            return Err(Error::Decode(
                "binary file with null bytes is not plain text".to_string(),
            ));
        }
        let mut content = std::str::from_utf8(&bytes)
            .map_err(|e| Error::Decode(format!("invalid UTF-8 text: {e}")))?
            .to_string();

        let format = SourceFormat::from_path(path).unwrap_or(SourceFormat::Txt);

        if format == SourceFormat::Json {
            let is_minified_or_wide =
                content.lines().count() <= 1 || content.lines().any(|l| l.chars().count() > 140);
            if is_minified_or_wide {
                if let Some(pretty) = pretty_format_json(&content) {
                    content = pretty;
                }
            }
        } else if format == SourceFormat::Xml {
            let is_minified_or_wide =
                content.lines().count() <= 1 || content.lines().any(|l| l.chars().count() > 140);
            if is_minified_or_wide {
                if let Some(pretty) = pretty_format_xml(&content) {
                    content = pretty;
                }
            }
        }

        Ok(Self::from_string_with_format(content, format))
    }

    /// Build a paginated document from an already-decoded UTF-8 string with default Txt format.
    pub fn from_string(content: String) -> Self {
        Self::from_string_with_format(content, SourceFormat::Txt)
    }

    /// Build a paginated document from an already-decoded UTF-8 string with an explicit format.
    pub fn from_string_with_format(content: String, format: SourceFormat) -> Self {
        let mut visual_rows = Vec::new();
        let mut in_string = false;
        let mut escaped = false;
        let mut in_comment = false;
        let mut in_tag = false;
        let mut in_cdata = false;
        let mut partial_delim = PartialDelim::None;

        for (line_idx, raw_line) in content.lines().enumerate() {
            let cleaned = sanitize_line(raw_line);
            let expanded = expand_tabs(&cleaned, TAB_SIZE);
            if expanded.is_empty() {
                visual_rows.push(VisualRow {
                    line_number: Some(line_idx + 1),
                    text: String::new(),
                    indent_cols: 0,
                    starts_in_string: in_string,
                    starts_escaped: escaped,
                    starts_in_comment: in_comment,
                    starts_in_tag: in_tag,
                    starts_in_cdata: in_cdata,
                    starts_in_section: false,
                    is_value_continuation: false,
                    partial_delim,
                });
            } else {
                let is_ini_or_toml = format == SourceFormat::Ini || format == SourceFormat::Toml;
                let trimmed = expanded.trim_start();
                let is_ini_comment = is_ini_or_toml
                    && (trimmed.starts_with(';')
                        || trimmed.starts_with('#')
                        || trimmed.starts_with('!'));
                let is_ini_section = is_ini_or_toml && trimmed.starts_with('[');
                let is_ini_kv = is_ini_or_toml
                    && !is_ini_comment
                    && !is_ini_section
                    && trimmed.contains(['=', ':']);
                let mut in_ini_section = false;

                let indent_spaces = expanded.chars().take_while(|&c| c == ' ').count();
                let chunks = wrap_line(&expanded, WRAP_COLS);
                for (chunk_idx, chunk) in chunks.into_iter().enumerate() {
                    let row_starts_in_string = in_string;
                    let row_starts_escaped = escaped;
                    let row_starts_in_comment = in_comment;
                    let row_starts_in_tag = in_tag;
                    let row_starts_in_cdata = in_cdata;
                    let row_starts_in_section = in_ini_section;
                    let row_is_value_continuation = is_ini_kv && chunk_idx > 0;
                    let row_partial_delim = partial_delim;

                    match format {
                        SourceFormat::Json | SourceFormat::Jsonl => {
                            update_string_state(&chunk, &mut in_string, &mut escaped);
                        }
                        SourceFormat::Xml => {
                            update_xml_state(
                                &chunk,
                                &mut in_string,
                                &mut in_comment,
                                &mut in_tag,
                                &mut in_cdata,
                                &mut partial_delim,
                            );
                        }
                        SourceFormat::Ini | SourceFormat::Toml => {
                            if is_ini_comment {
                                in_comment = true;
                            } else if is_ini_section {
                                if chunk_idx == 0 {
                                    let has_close = if chunk.trim_start().starts_with("[[") {
                                        chunk.contains("]]")
                                    } else {
                                        chunk.contains(']')
                                    };
                                    in_ini_section = !has_close;
                                } else if in_ini_section && chunk.contains(']') {
                                    in_ini_section = false;
                                }
                            } else {
                                update_string_state(&chunk, &mut in_string, &mut escaped);
                            }
                        }
                        _ => {}
                    }

                    visual_rows.push(VisualRow {
                        line_number: if chunk_idx == 0 {
                            Some(line_idx + 1)
                        } else {
                            None
                        },
                        text: chunk,
                        indent_cols: if chunk_idx > 0 && indent_spaces < WRAP_COLS / 2 {
                            indent_spaces
                        } else {
                            0
                        },
                        starts_in_string: row_starts_in_string,
                        starts_escaped: row_starts_escaped,
                        starts_in_comment: row_starts_in_comment,
                        starts_in_tag: row_starts_in_tag,
                        starts_in_cdata: row_starts_in_cdata,
                        starts_in_section: row_starts_in_section,
                        is_value_continuation: row_is_value_continuation,
                        partial_delim: row_partial_delim,
                    });
                }
            }
            if format != SourceFormat::Xml {
                in_string = false;
                escaped = false;
                in_comment = false;
                in_tag = false;
                in_cdata = false;
                partial_delim = PartialDelim::None;
            }
        }

        if visual_rows.is_empty() {
            visual_rows.push(VisualRow {
                line_number: Some(1),
                text: String::new(),
                indent_cols: 0,
                starts_in_string: false,
                starts_escaped: false,
                starts_in_comment: false,
                starts_in_tag: false,
                starts_in_cdata: false,
                starts_in_section: false,
                is_value_continuation: false,
                partial_delim: PartialDelim::None,
            });
        }

        let pages = visual_rows
            .chunks(LINES_PER_PAGE)
            .map(|c| c.to_vec())
            .collect();

        Self {
            content,
            pages,
            format,
        }
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
            if !row.text.is_empty() || row.indent_cols > 0 {
                let inner = match self.format {
                    SourceFormat::Json | SourceFormat::Jsonl => highlight_json_row(
                        &row.text,
                        row.indent_cols,
                        row.starts_in_string,
                        row.starts_escaped,
                    ),
                    SourceFormat::Xml => highlight_xml_row(
                        &row.text,
                        row.indent_cols,
                        row.starts_in_string,
                        row.starts_in_comment,
                        row.starts_in_tag,
                        row.starts_in_cdata,
                        row.partial_delim,
                    ),
                    SourceFormat::Ini | SourceFormat::Toml => highlight_ini_row(
                        &row.text,
                        row.indent_cols,
                        row.starts_in_string,
                        row.starts_in_comment,
                        row.starts_in_section,
                        row.is_value_continuation,
                    ),
                    _ => {
                        let mut s = String::with_capacity(row.indent_cols + row.text.len());
                        for _ in 0..row.indent_cols {
                            s.push(' ');
                        }
                        s.push_str(&escape_xml(&row.text));
                        s
                    }
                };
                svg.push_str(&format!(
                    r##"<text x="110" y="{y}" fill="#e4e4e7" font-family="ui-monospace, Menlo, Monaco, Consolas, monospace" font-size="19" xml:space="preserve">{inner}</text>
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

    let indent_spaces = line.chars().take_while(|&c| c == ' ').count();
    let cont_max = if indent_spaces > 0 && indent_spaces < max_cols / 2 {
        max_cols.saturating_sub(indent_spaces)
    } else {
        max_cols
    };

    let mut result = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let mut start = 0;

    // First chunk uses full max_cols
    let first_end = (start + max_cols).min(chars.len());
    let search_start = start + max_cols.saturating_sub(max_cols / 5);
    let break_pos = chars[search_start..first_end]
        .iter()
        .rposition(|&c| c.is_whitespace())
        .map(|pos| search_start + pos + 1);
    let actual_first_end = break_pos.unwrap_or(first_end);
    result.push(chars[start..actual_first_end].iter().collect());
    start = actual_first_end;

    // Continuation chunks use cont_max and preserve exact characters
    while start < chars.len() {
        let end = (start + cont_max).min(chars.len());
        if end == chars.len() {
            result.push(chars[start..end].iter().collect());
            break;
        }

        let search_start = start + cont_max.saturating_sub(cont_max / 5);
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

const RAINBOW_PALETTE: &[&str] = &[
    "#eab308", // Level 1: Amber / Gold
    "#38bdf8", // Level 2: Sky Blue
    "#34d399", // Level 3: Emerald
    "#a78bfa", // Level 4: Purple
    "#fb7185", // Level 5: Coral
];

fn update_string_state(text: &str, in_string: &mut bool, escaped: &mut bool) {
    for c in text.chars() {
        if *in_string {
            if *escaped {
                *escaped = false;
            } else if c == '\\' {
                *escaped = true;
            } else if c == '"' {
                *in_string = false;
            }
        } else if c == '"' {
            *in_string = true;
        }
    }
}

fn update_xml_state(
    text: &str,
    in_string: &mut bool,
    in_comment: &mut bool,
    in_tag: &mut bool,
    in_cdata: &mut bool,
    partial_delim: &mut PartialDelim,
) {
    let chars: Vec<char> = text.chars().collect();
    let mut idx = 0;

    // First, resolve any partial delimiter carried from the previous chunk
    match *partial_delim {
        PartialDelim::CommentCloseDoubleDash => {
            *partial_delim = PartialDelim::None;
            if idx < chars.len() && chars[idx] == '>' {
                idx += 1;
                *in_comment = false;
            }
        }
        PartialDelim::CommentCloseDash => {
            *partial_delim = PartialDelim::None;
            if idx + 1 < chars.len() && chars[idx] == '-' && chars[idx + 1] == '>' {
                idx += 2;
                *in_comment = false;
            } else if idx < chars.len() && chars[idx] == '-' && idx + 1 == chars.len() {
                idx += 1;
                *partial_delim = PartialDelim::CommentCloseDoubleDash;
            }
        }
        PartialDelim::CdataCloseDoubleBracket => {
            *partial_delim = PartialDelim::None;
            if idx < chars.len() && chars[idx] == '>' {
                idx += 1;
                *in_cdata = false;
            }
        }
        PartialDelim::CdataCloseBracket => {
            *partial_delim = PartialDelim::None;
            if idx + 1 < chars.len() && chars[idx] == ']' && chars[idx + 1] == '>' {
                idx += 2;
                *in_cdata = false;
            } else if idx < chars.len() && chars[idx] == ']' && idx + 1 == chars.len() {
                idx += 1;
                *partial_delim = PartialDelim::CdataCloseDoubleBracket;
            }
        }
        PartialDelim::CommentOpenDash => {
            *partial_delim = PartialDelim::None;
            if idx < chars.len() && chars[idx] == '-' {
                idx += 1;
                *in_comment = true;
            }
        }
        PartialDelim::Bang => {
            *partial_delim = PartialDelim::None;
            if idx + 1 < chars.len() && chars[idx] == '-' && chars[idx + 1] == '-' {
                idx += 2;
                *in_comment = true;
            } else if idx < chars.len() && chars[idx] == '-' && idx + 1 == chars.len() {
                idx += 1;
                *partial_delim = PartialDelim::CommentOpenDash;
            } else if idx + 6 < chars.len()
                && chars[idx..idx + 7] == ['[', 'C', 'D', 'A', 'T', 'A', '[']
            {
                idx += 7;
                *in_cdata = true;
            } else if idx < chars.len() && chars[idx] == '[' {
                let full = ['[', 'C', 'D', 'A', 'T', 'A', '['];
                let mut matched = 3;
                let mut k = 0;
                while idx < chars.len() && k < full.len() && chars[idx] == full[k] {
                    idx += 1;
                    k += 1;
                    matched += 1;
                }
                if k == full.len() {
                    *in_cdata = true;
                } else if idx == chars.len() {
                    *partial_delim = PartialDelim::CdataOpen((matched - 1) as u8);
                }
            }
        }
        PartialDelim::OpenAngle => {
            *partial_delim = PartialDelim::None;
            if idx + 2 < chars.len()
                && chars[idx] == '!'
                && chars[idx + 1] == '-'
                && chars[idx + 2] == '-'
            {
                idx += 3;
                *in_comment = true;
            } else if idx + 1 < chars.len()
                && chars[idx] == '!'
                && chars[idx + 1] == '-'
                && idx + 2 == chars.len()
            {
                idx += 2;
                *partial_delim = PartialDelim::CommentOpenDash;
            } else if idx < chars.len() && chars[idx] == '!' && idx + 1 == chars.len() {
                idx += 1;
                *partial_delim = PartialDelim::Bang;
            } else if idx + 7 < chars.len()
                && chars[idx..idx + 8] == ['!', '[', 'C', 'D', 'A', 'T', 'A', '[']
            {
                idx += 8;
                *in_cdata = true;
            } else if idx < chars.len() && chars[idx] != '/' {
                *in_tag = true;
            }
        }
        PartialDelim::CdataOpen(len) => {
            *partial_delim = PartialDelim::None;
            let full = "<![CDATA[";
            let suffix: Vec<char> = full.chars().skip(len as usize).collect();
            let mut k = 0;
            while idx < chars.len() && k < suffix.len() && chars[idx] == suffix[k] {
                idx += 1;
                k += 1;
            }
            if k == suffix.len() {
                *in_cdata = true;
            } else if idx == chars.len() {
                *partial_delim = PartialDelim::CdataOpen(len + k as u8);
            }
        }
        PartialDelim::None => {}
    }

    while idx < chars.len() {
        if *in_comment {
            if chars[idx] == '-' {
                if idx + 2 < chars.len() && chars[idx + 1] == '-' && chars[idx + 2] == '>' {
                    idx += 3;
                    *in_comment = false;
                } else if idx + 1 < chars.len() && chars[idx + 1] == '-' {
                    if idx + 2 == chars.len() {
                        *partial_delim = PartialDelim::CommentCloseDoubleDash;
                        break;
                    }
                    idx += 2;
                } else {
                    if idx + 1 == chars.len() {
                        *partial_delim = PartialDelim::CommentCloseDash;
                        break;
                    }
                    idx += 1;
                }
            } else {
                idx += 1;
            }
        } else if *in_cdata {
            if chars[idx] == ']' {
                if idx + 2 < chars.len() && chars[idx + 1] == ']' && chars[idx + 2] == '>' {
                    idx += 3;
                    *in_cdata = false;
                } else if idx + 1 < chars.len() && chars[idx + 1] == ']' {
                    if idx + 2 == chars.len() {
                        *partial_delim = PartialDelim::CdataCloseDoubleBracket;
                        break;
                    }
                    idx += 2;
                } else {
                    if idx + 1 == chars.len() {
                        *partial_delim = PartialDelim::CdataCloseBracket;
                        break;
                    }
                    idx += 1;
                }
            } else {
                idx += 1;
            }
        } else if *in_string {
            if chars[idx] == '"' || chars[idx] == '\'' {
                *in_string = false;
            }
            idx += 1;
        } else if *in_tag {
            if chars[idx] == '"' || chars[idx] == '\'' {
                *in_string = true;
            } else if chars[idx] == '>' {
                *in_tag = false;
            }
            idx += 1;
        } else if chars[idx] == '<' {
            if idx + 3 < chars.len()
                && chars[idx + 1] == '!'
                && chars[idx + 2] == '-'
                && chars[idx + 3] == '-'
            {
                idx += 4;
                *in_comment = true;
            } else if idx + 8 < chars.len()
                && chars[idx..idx + 9] == ['<', '!', '[', 'C', 'D', 'A', 'T', 'A', '[']
            {
                idx += 9;
                *in_cdata = true;
            } else if idx + 1 == chars.len() {
                *partial_delim = PartialDelim::OpenAngle;
                break;
            } else if idx + 2 == chars.len() && chars[idx + 1] == '!' {
                *partial_delim = PartialDelim::Bang;
                break;
            } else if idx + 3 == chars.len() && chars[idx + 1] == '!' && chars[idx + 2] == '-' {
                *partial_delim = PartialDelim::CommentOpenDash;
                break;
            } else if idx + 2 < chars.len() && chars[idx + 1] == '!' && chars[idx + 2] == '[' {
                let full = ['<', '!', '[', 'C', 'D', 'A', 'T', 'A', '['];
                let mut matched = 3;
                while idx + matched < chars.len() && chars[idx + matched] == full[matched] {
                    matched += 1;
                }
                if idx + matched == chars.len() {
                    *partial_delim = PartialDelim::CdataOpen(matched as u8);
                    break;
                }
                idx += matched;
            } else if chars.get(idx + 1) != Some(&'/') {
                *in_tag = true;
                idx += 1;
            } else {
                idx += 1;
            }
        } else {
            idx += 1;
        }
    }
}

fn highlight_json_row(
    text: &str,
    indent_cols: usize,
    starts_in_string: bool,
    mut escaped: bool,
) -> String {
    let mut out = String::with_capacity(text.len() * 2 + indent_cols * 15);

    let leading_spaces = text.chars().take_while(|&c| c == ' ').count();
    let total_indent = if indent_cols > 0 {
        indent_cols
    } else {
        leading_spaces
    };
    let num_levels = total_indent / 2;
    let remainder_spaces = total_indent % 2;

    for i in 0..num_levels {
        let color = RAINBOW_PALETTE[i % RAINBOW_PALETTE.len()];
        out.push_str(&format!(
            r##"<tspan fill="{color}" fill-opacity="0.45">│ </tspan>"##
        ));
    }
    for _ in 0..remainder_spaces {
        out.push(' ');
    }

    let rest = if indent_cols > 0 {
        text
    } else {
        &text[leading_spaces..]
    };
    if rest.is_empty() {
        return out;
    }

    let mut chars = rest.chars().peekable();

    if starts_in_string {
        let mut string_content = String::new();
        let mut closed = false;
        for sc in chars.by_ref() {
            string_content.push(sc);
            if escaped {
                escaped = false;
            } else if sc == '\\' {
                escaped = true;
            } else if sc == '"' {
                closed = true;
                break;
            }
        }
        let escaped_str = escape_xml(&string_content);
        out.push_str(&format!(r##"<tspan fill="#86efac">{escaped_str}</tspan>"##));
        if !closed {
            return out;
        }
    }

    while let Some(c) = chars.next() {
        match c {
            '"' => {
                let mut string_content = String::from('"');
                let mut str_escaped = false;
                for sc in chars.by_ref() {
                    string_content.push(sc);
                    if str_escaped {
                        str_escaped = false;
                    } else if sc == '\\' {
                        str_escaped = true;
                    } else if sc == '"' {
                        break;
                    }
                }

                // Check if followed by colon (i.e. this is an object key)
                let mut peek_ahead = chars.clone();
                let mut is_key = false;
                for pc in peek_ahead.by_ref() {
                    if pc.is_whitespace() {
                        continue;
                    }
                    if pc == ':' {
                        is_key = true;
                    }
                    break;
                }

                let color = if is_key { "#7dd3fc" } else { "#86efac" };
                let escaped_str = escape_xml(&string_content);
                out.push_str(&format!(r##"<tspan fill="{color}">{escaped_str}</tspan>"##));
            }
            ':' => {
                out.push_str(r##"<tspan fill="#71717a">:</tspan>"##);
            }
            ',' => {
                out.push_str(r##"<tspan fill="#71717a">,</tspan>"##);
            }
            '{' | '}' | '[' | ']' => {
                out.push_str(&format!(r##"<tspan fill="#cbd5e1">{c}</tspan>"##));
            }
            c if c == '-' || c.is_ascii_digit() => {
                let mut num_str = String::from(c);
                while let Some(&nc) = chars.peek() {
                    if nc.is_ascii_digit()
                        || nc == '.'
                        || nc == 'e'
                        || nc == 'E'
                        || nc == '+'
                        || nc == '-'
                    {
                        num_str.push(chars.next().unwrap());
                    } else {
                        break;
                    }
                }
                out.push_str(&format!(r##"<tspan fill="#fdba74">{num_str}</tspan>"##));
            }
            c if c.is_alphabetic() => {
                let mut word = String::from(c);
                while let Some(&nc) = chars.peek() {
                    if nc.is_alphabetic() {
                        word.push(chars.next().unwrap());
                    } else {
                        break;
                    }
                }
                match word.as_str() {
                    "true" | "false" | "null" => {
                        out.push_str(&format!(r##"<tspan fill="#f472b6">{word}</tspan>"##));
                    }
                    _ => {
                        let escaped = escape_xml(&word);
                        out.push_str(&escaped);
                    }
                }
            }
            c => {
                let s = c.to_string();
                out.push_str(&escape_xml(&s));
            }
        }
    }

    out
}

fn highlight_xml_row(
    text: &str,
    indent_cols: usize,
    starts_in_string: bool,
    starts_in_comment: bool,
    starts_in_tag: bool,
    starts_in_cdata: bool,
    partial_delim: PartialDelim,
) -> String {
    let mut out = String::with_capacity(text.len() * 2 + indent_cols * 15);

    let leading_spaces = text.chars().take_while(|&c| c == ' ').count();
    let total_indent = if indent_cols > 0 {
        indent_cols
    } else {
        leading_spaces
    };
    let num_levels = total_indent / 2;
    let remainder_spaces = total_indent % 2;

    for i in 0..num_levels {
        let color = RAINBOW_PALETTE[i % RAINBOW_PALETTE.len()];
        out.push_str(&format!(
            r##"<tspan fill="{color}" fill-opacity="0.45">│ </tspan>"##
        ));
    }
    for _ in 0..remainder_spaces {
        out.push(' ');
    }

    let rest = if indent_cols > 0 {
        text
    } else {
        &text[leading_spaces..]
    };
    if rest.is_empty() {
        return out;
    }

    let mut in_comment = starts_in_comment;
    let mut in_string = starts_in_string;
    let mut in_tag = starts_in_tag;
    let mut in_cdata = starts_in_cdata;

    let chars: Vec<char> = rest.chars().collect();
    let mut idx = 0;

    match partial_delim {
        PartialDelim::CommentCloseDoubleDash => {
            if idx < chars.len() && chars[idx] == '>' {
                out.push_str(r##"<tspan fill="#71717a">&gt;</tspan>"##);
                idx += 1;
                in_comment = false;
            }
        }
        PartialDelim::CommentCloseDash => {
            if idx + 1 < chars.len() && chars[idx] == '-' && chars[idx + 1] == '>' {
                out.push_str(r##"<tspan fill="#71717a">-&gt;</tspan>"##);
                idx += 2;
                in_comment = false;
            } else if idx < chars.len() && chars[idx] == '-' {
                out.push_str(r##"<tspan fill="#71717a">-</tspan>"##);
                idx += 1;
            }
        }
        PartialDelim::CdataCloseDoubleBracket => {
            if idx < chars.len() && chars[idx] == '>' {
                out.push_str(r##"<tspan fill="#f472b6">&gt;</tspan>"##);
                idx += 1;
                in_cdata = false;
            }
        }
        PartialDelim::CdataCloseBracket => {
            if idx + 1 < chars.len() && chars[idx] == ']' && chars[idx + 1] == '>' {
                out.push_str(r##"<tspan fill="#f472b6">]&gt;</tspan>"##);
                idx += 2;
                in_cdata = false;
            } else if idx < chars.len() && chars[idx] == ']' {
                out.push_str(r##"<tspan fill="#f472b6">]</tspan>"##);
                idx += 1;
            }
        }
        PartialDelim::CommentOpenDash => {
            if idx < chars.len() && chars[idx] == '-' {
                out.push_str(r##"<tspan fill="#71717a">-</tspan>"##);
                idx += 1;
                in_comment = true;
            }
        }
        PartialDelim::Bang => {
            if idx + 1 < chars.len() && chars[idx] == '-' && chars[idx + 1] == '-' {
                out.push_str(r##"<tspan fill="#71717a">--</tspan>"##);
                idx += 2;
                in_comment = true;
            } else if idx < chars.len() && chars[idx] == '-' {
                out.push_str(r##"<tspan fill="#71717a">-</tspan>"##);
                idx += 1;
            } else if idx + 6 < chars.len()
                && chars[idx..idx + 7] == ['[', 'C', 'D', 'A', 'T', 'A', '[']
            {
                out.push_str(r##"<tspan fill="#f472b6">[CDATA[</tspan>"##);
                idx += 7;
                in_cdata = true;
            }
        }
        PartialDelim::OpenAngle => {
            if idx + 2 < chars.len()
                && chars[idx] == '!'
                && chars[idx + 1] == '-'
                && chars[idx + 2] == '-'
            {
                out.push_str(r##"<tspan fill="#71717a">!--</tspan>"##);
                idx += 3;
                in_comment = true;
            } else if idx + 1 < chars.len() && chars[idx] == '!' && chars[idx + 1] == '-' {
                out.push_str(r##"<tspan fill="#71717a">!-</tspan>"##);
                idx += 2;
            } else if idx < chars.len() && chars[idx] == '!' {
                out.push_str(r##"<tspan fill="#71717a">!</tspan>"##);
                idx += 1;
            } else if idx + 7 < chars.len()
                && chars[idx..idx + 8] == ['!', '[', 'C', 'D', 'A', 'T', 'A', '[']
            {
                out.push_str(r##"<tspan fill="#f472b6">![CDATA[</tspan>"##);
                idx += 8;
                in_cdata = true;
            } else if idx < chars.len() && chars[idx] == '/' {
                out.push_str(r##"<tspan fill="#71717a">/</tspan>"##);
                idx += 1;
                let mut tag_name = String::new();
                while idx < chars.len() && chars[idx] != '>' && !chars[idx].is_whitespace() {
                    tag_name.push(chars[idx]);
                    idx += 1;
                }
                out.push_str(&format!(
                    r##"<tspan fill="#7dd3fc">{}</tspan>"##,
                    escape_xml(&tag_name)
                ));
                while idx < chars.len() && chars[idx] != '>' {
                    out.push(chars[idx]);
                    idx += 1;
                }
                if idx < chars.len() && chars[idx] == '>' {
                    out.push_str(r##"<tspan fill="#71717a">&gt;</tspan>"##);
                    idx += 1;
                }
            } else if idx < chars.len() && chars[idx] == '?' {
                in_tag = true;
                out.push_str(r##"<tspan fill="#fdba74">?</tspan>"##);
                idx += 1;
            } else {
                in_tag = true;
            }
        }
        PartialDelim::CdataOpen(len) => {
            let full = "<![CDATA[";
            let suffix = &full[len as usize..];
            let suffix_chars: Vec<char> = suffix.chars().collect();
            if idx + suffix_chars.len() <= chars.len()
                && chars[idx..idx + suffix_chars.len()] == suffix_chars[..]
            {
                out.push_str(&format!(
                    r##"<tspan fill="#f472b6">{}</tspan>"##,
                    escape_xml(suffix)
                ));
                idx += suffix_chars.len();
                in_cdata = true;
            }
        }
        PartialDelim::None => {}
    }

    while idx < chars.len() {
        if in_comment {
            let mut comment_content = String::new();
            while idx < chars.len() {
                comment_content.push(chars[idx]);
                if chars[idx] == '>' && idx >= 2 && chars[idx - 1] == '-' && chars[idx - 2] == '-' {
                    in_comment = false;
                    idx += 1;
                    break;
                }
                idx += 1;
            }
            out.push_str(&format!(
                r##"<tspan fill="#71717a">{}</tspan>"##,
                escape_xml(&comment_content)
            ));
        } else if in_cdata {
            let mut cdata_content = String::new();
            while idx < chars.len() {
                cdata_content.push(chars[idx]);
                if chars[idx] == '>' && idx >= 2 && chars[idx - 1] == ']' && chars[idx - 2] == ']' {
                    in_cdata = false;
                    idx += 1;
                    break;
                }
                idx += 1;
            }
            out.push_str(&format!(
                r##"<tspan fill="#f472b6">{}</tspan>"##,
                escape_xml(&cdata_content)
            ));
        } else if in_string {
            let mut string_content = String::new();
            while idx < chars.len() {
                let c = chars[idx];
                string_content.push(c);
                idx += 1;
                if c == '"' || c == '\'' {
                    in_string = false;
                    break;
                }
            }
            out.push_str(&format!(
                r##"<tspan fill="#86efac">{}</tspan>"##,
                escape_xml(&string_content)
            ));
        } else if in_tag {
            let c = chars[idx];
            if c.is_whitespace() {
                out.push(c);
                idx += 1;
            } else if c == '>' {
                out.push_str(r##"<tspan fill="#71717a">&gt;</tspan>"##);
                in_tag = false;
                idx += 1;
            } else if c == '/' && idx + 1 < chars.len() && chars[idx + 1] == '>' {
                out.push_str(r##"<tspan fill="#71717a">/&gt;</tspan>"##);
                in_tag = false;
                idx += 2;
            } else if c == '?' && idx + 1 < chars.len() && chars[idx + 1] == '>' {
                out.push_str(r##"<tspan fill="#fdba74">?&gt;</tspan>"##);
                in_tag = false;
                idx += 2;
            } else if c == '=' {
                out.push_str(r##"<tspan fill="#71717a">=</tspan>"##);
                idx += 1;
            } else if c == '"' || c == '\'' {
                let quote = c;
                let mut val = String::from(quote);
                idx += 1;
                while idx < chars.len() {
                    let sc = chars[idx];
                    val.push(sc);
                    idx += 1;
                    if sc == quote {
                        break;
                    }
                }
                if !val.ends_with(quote) || val.len() == 1 {
                    in_string = true;
                }
                out.push_str(&format!(
                    r##"<tspan fill="#86efac">{}</tspan>"##,
                    escape_xml(&val)
                ));
            } else {
                let mut attr_name = String::new();
                while idx < chars.len() {
                    let ac = chars[idx];
                    if ac.is_whitespace() || ac == '=' || ac == '/' || ac == '>' || ac == '?' {
                        break;
                    }
                    attr_name.push(ac);
                    idx += 1;
                }
                out.push_str(&format!(
                    r##"<tspan fill="#c084fc">{}</tspan>"##,
                    escape_xml(&attr_name)
                ));
            }
        } else {
            let c = chars[idx];
            if c == '<' {
                if idx + 3 < chars.len()
                    && chars[idx + 1] == '!'
                    && chars[idx + 2] == '-'
                    && chars[idx + 3] == '-'
                {
                    in_comment = true;
                    let mut comment = String::from("<!--");
                    idx += 4;
                    while idx < chars.len() {
                        comment.push(chars[idx]);
                        if chars[idx] == '>'
                            && idx >= 2
                            && chars[idx - 1] == '-'
                            && chars[idx - 2] == '-'
                        {
                            in_comment = false;
                            idx += 1;
                            break;
                        }
                        idx += 1;
                    }
                    out.push_str(&format!(
                        r##"<tspan fill="#71717a">{}</tspan>"##,
                        escape_xml(&comment)
                    ));
                } else if idx + 8 < chars.len()
                    && chars[idx..idx + 9] == ['<', '!', '[', 'C', 'D', 'A', 'T', 'A', '[']
                {
                    in_cdata = true;
                    let mut cdata = String::new();
                    for &ch in &chars[idx..idx + 9] {
                        cdata.push(ch);
                    }
                    idx += 9;
                    while idx < chars.len() {
                        cdata.push(chars[idx]);
                        if chars[idx] == '>'
                            && idx >= 2
                            && chars[idx - 1] == ']'
                            && chars[idx - 2] == ']'
                        {
                            in_cdata = false;
                            idx += 1;
                            break;
                        }
                        idx += 1;
                    }
                    out.push_str(&format!(
                        r##"<tspan fill="#f472b6">{}</tspan>"##,
                        escape_xml(&cdata)
                    ));
                } else if idx + 3 == chars.len() && chars[idx + 1] == '!' && chars[idx + 2] == '-' {
                    out.push_str(r##"<tspan fill="#71717a">&lt;!-</tspan>"##);
                    break;
                } else if idx + 2 == chars.len() && chars[idx + 1] == '!' {
                    out.push_str(r##"<tspan fill="#71717a">&lt;!</tspan>"##);
                    break;
                } else if idx + 1 == chars.len() {
                    out.push_str(r##"<tspan fill="#71717a">&lt;</tspan>"##);
                    break;
                } else if idx + 2 < chars.len() && chars[idx + 1] == '!' && chars[idx + 2] == '[' {
                    let full = ['<', '!', '[', 'C', 'D', 'A', 'T', 'A', '['];
                    let mut matched = 3;
                    while idx + matched < chars.len() && chars[idx + matched] == full[matched] {
                        matched += 1;
                    }
                    if idx + matched == chars.len() {
                        let partial: String = chars[idx..chars.len()].iter().collect();
                        out.push_str(&format!(
                            r##"<tspan fill="#f472b6">{}</tspan>"##,
                            escape_xml(&partial)
                        ));
                        break;
                    }
                    let mut decl = String::from("<![");
                    idx += 3;
                    while idx < chars.len() && chars[idx] != '>' {
                        decl.push(chars[idx]);
                        idx += 1;
                    }
                    if idx < chars.len() && chars[idx] == '>' {
                        decl.push('>');
                        idx += 1;
                    }
                    out.push_str(&format!(
                        r##"<tspan fill="#cbd5e1">{}</tspan>"##,
                        escape_xml(&decl)
                    ));
                } else if idx + 1 < chars.len() && chars[idx + 1] == '?' {
                    in_tag = true;
                    out.push_str(r##"<tspan fill="#fdba74">&lt;?</tspan>"##);
                    idx += 2;
                    let mut target = String::new();
                    while idx < chars.len()
                        && (chars[idx].is_alphanumeric() || chars[idx] == '_' || chars[idx] == '-')
                    {
                        target.push(chars[idx]);
                        idx += 1;
                    }
                    if !target.is_empty() {
                        out.push_str(&format!(
                            r##"<tspan fill="#7dd3fc">{}</tspan>"##,
                            escape_xml(&target)
                        ));
                    }
                } else if idx + 1 < chars.len() && chars[idx + 1] == '!' {
                    let mut decl = String::from("<!");
                    idx += 2;
                    while idx < chars.len() && chars[idx] != '>' {
                        decl.push(chars[idx]);
                        idx += 1;
                    }
                    if idx < chars.len() && chars[idx] == '>' {
                        decl.push('>');
                        idx += 1;
                    }
                    out.push_str(&format!(
                        r##"<tspan fill="#cbd5e1">{}</tspan>"##,
                        escape_xml(&decl)
                    ));
                } else if idx + 1 < chars.len() && chars[idx + 1] == '/' {
                    out.push_str(r##"<tspan fill="#71717a">&lt;/</tspan>"##);
                    idx += 2;
                    let mut tag_name = String::new();
                    while idx < chars.len() && chars[idx] != '>' && !chars[idx].is_whitespace() {
                        tag_name.push(chars[idx]);
                        idx += 1;
                    }
                    out.push_str(&format!(
                        r##"<tspan fill="#7dd3fc">{}</tspan>"##,
                        escape_xml(&tag_name)
                    ));
                    while idx < chars.len() && chars[idx] != '>' {
                        out.push(chars[idx]);
                        idx += 1;
                    }
                    if idx < chars.len() && chars[idx] == '>' {
                        out.push_str(r##"<tspan fill="#71717a">&gt;</tspan>"##);
                        idx += 1;
                    }
                } else {
                    out.push_str(r##"<tspan fill="#71717a">&lt;</tspan>"##);
                    idx += 1;
                    let mut tag_name = String::new();
                    while idx < chars.len()
                        && !chars[idx].is_whitespace()
                        && chars[idx] != '>'
                        && chars[idx] != '/'
                    {
                        tag_name.push(chars[idx]);
                        idx += 1;
                    }
                    out.push_str(&format!(
                        r##"<tspan fill="#7dd3fc">{}</tspan>"##,
                        escape_xml(&tag_name)
                    ));
                    in_tag = true;
                }
            } else {
                let mut text_buf = String::new();
                while idx < chars.len() && chars[idx] != '<' {
                    text_buf.push(chars[idx]);
                    idx += 1;
                }
                out.push_str(&escape_xml(&text_buf));
            }
        }
    }

    out
}

fn highlight_ini_row(
    text: &str,
    indent_cols: usize,
    starts_in_string: bool,
    starts_in_comment: bool,
    starts_in_section: bool,
    is_value_continuation: bool,
) -> String {
    let mut out = String::with_capacity(text.len() * 2 + indent_cols * 15);

    let leading_spaces = text.chars().take_while(|&c| c == ' ').count();
    let total_indent = if indent_cols > 0 {
        indent_cols
    } else {
        leading_spaces
    };
    let num_levels = total_indent / 2;
    let remainder_spaces = total_indent % 2;

    for i in 0..num_levels {
        let color = RAINBOW_PALETTE[i % RAINBOW_PALETTE.len()];
        out.push_str(&format!(
            r##"<tspan fill="{color}" fill-opacity="0.45">│ </tspan>"##
        ));
    }
    for _ in 0..remainder_spaces {
        out.push(' ');
    }

    let rest = if indent_cols > 0 {
        text
    } else {
        &text[leading_spaces..]
    };
    if rest.is_empty() {
        return out;
    }

    if starts_in_comment {
        out.push_str(&format!(
            r##"<tspan fill="#71717a">{}</tspan>"##,
            escape_xml(rest)
        ));
        return out;
    }

    if starts_in_section {
        if let Some(idx) = rest.find(']') {
            let (sec_end, close_len) = if rest[idx..].starts_with("]]") {
                (idx, 2)
            } else {
                (idx, 1)
            };
            let name_part = &rest[..sec_end];
            let close_part = &rest[sec_end..sec_end + close_len];
            let trailing = &rest[sec_end + close_len..];

            out.push_str(&format!(
                r##"<tspan fill="#7dd3fc" font-weight="bold">{}</tspan>"##,
                escape_xml(name_part)
            ));
            out.push_str(&format!(
                r##"<tspan fill="#cbd5e1">{}</tspan>"##,
                escape_xml(close_part)
            ));
            if !trailing.is_empty() {
                out.push_str(&format!(
                    r##"<tspan fill="#71717a">{}</tspan>"##,
                    escape_xml(trailing)
                ));
            }
        } else {
            out.push_str(&format!(
                r##"<tspan fill="#7dd3fc" font-weight="bold">{}</tspan>"##,
                escape_xml(rest)
            ));
        }
        return out;
    }

    if starts_in_string {
        let mut string_content = String::new();
        let mut closed = false;
        let mut chars = rest.chars();
        for c in chars.by_ref() {
            string_content.push(c);
            if c == '"' || c == '\'' {
                closed = true;
                break;
            }
        }
        out.push_str(&format!(
            r##"<tspan fill="#86efac">{}</tspan>"##,
            escape_xml(&string_content)
        ));
        if !closed {
            return out;
        }
        let remaining: String = chars.collect();
        out.push_str(&escape_xml(&remaining));
        return out;
    }

    if rest.starts_with(';') || rest.starts_with('#') || rest.starts_with('!') {
        out.push_str(&format!(
            r##"<tspan fill="#71717a">{}</tspan>"##,
            escape_xml(rest)
        ));
        return out;
    }

    if let Some(stripped) = rest.strip_prefix("[[") {
        if let Some(end_idx) = stripped.find("]]") {
            let section_name = &stripped[..end_idx];
            let trailing = &stripped[end_idx + 2..];
            out.push_str(r##"<tspan fill="#cbd5e1">[[</tspan>"##);
            out.push_str(&format!(
                r##"<tspan fill="#7dd3fc" font-weight="bold">{}</tspan>"##,
                escape_xml(section_name)
            ));
            out.push_str(r##"<tspan fill="#cbd5e1">]]</tspan>"##);
            if !trailing.is_empty() {
                out.push_str(&format!(
                    r##"<tspan fill="#71717a">{}</tspan>"##,
                    escape_xml(trailing)
                ));
            }
        } else {
            out.push_str(r##"<tspan fill="#cbd5e1">[[</tspan>"##);
            out.push_str(&format!(
                r##"<tspan fill="#7dd3fc" font-weight="bold">{}</tspan>"##,
                escape_xml(stripped)
            ));
        }
        return out;
    } else if let Some(stripped) = rest.strip_prefix('[') {
        if let Some(end_idx) = stripped.find(']') {
            let section_name = &stripped[..end_idx];
            let trailing = &stripped[end_idx + 1..];
            out.push_str(r##"<tspan fill="#cbd5e1">[</tspan>"##);
            out.push_str(&format!(
                r##"<tspan fill="#7dd3fc" font-weight="bold">{}</tspan>"##,
                escape_xml(section_name)
            ));
            out.push_str(r##"<tspan fill="#cbd5e1">]</tspan>"##);
            if !trailing.is_empty() {
                out.push_str(&format!(
                    r##"<tspan fill="#71717a">{}</tspan>"##,
                    escape_xml(trailing)
                ));
            }
        } else {
            out.push_str(r##"<tspan fill="#cbd5e1">[</tspan>"##);
            out.push_str(&format!(
                r##"<tspan fill="#7dd3fc" font-weight="bold">{}</tspan>"##,
                escape_xml(stripped)
            ));
        }
        return out;
    }

    if is_value_continuation {
        let (val_part, comment_part) = split_ini_comment(rest);
        highlight_ini_value(&mut out, val_part);
        if let Some(c) = comment_part {
            out.push_str(&format!(
                r##"<tspan fill="#71717a">{}</tspan>"##,
                escape_xml(c)
            ));
        }
        return out;
    }

    if let Some(delim_pos) = rest.find(['=', ':']) {
        let key = &rest[..delim_pos];
        let delim = &rest[delim_pos..delim_pos + 1];
        let val = &rest[delim_pos + 1..];

        let key_trimmed = key.trim_end();
        let key_trailing = &key[key_trimmed.len()..];

        out.push_str(&format!(
            r##"<tspan fill="#93c5fd">{}</tspan>"##,
            escape_xml(key_trimmed)
        ));
        out.push_str(key_trailing);
        out.push_str(&format!(
            r##"<tspan fill="#71717a">{}</tspan>"##,
            escape_xml(delim)
        ));

        let (val_part, comment_part) = split_ini_comment(val);
        highlight_ini_value(&mut out, val_part);
        if let Some(c) = comment_part {
            out.push_str(&format!(
                r##"<tspan fill="#71717a">{}</tspan>"##,
                escape_xml(c)
            ));
        }
        return out;
    }

    out.push_str(&escape_xml(rest));
    out
}

fn highlight_ini_value(out: &mut String, val: &str) {
    let leading_ws = val
        .chars()
        .take_while(|c| c.is_whitespace())
        .collect::<String>();
    let trailing_ws = val
        .chars()
        .rev()
        .take_while(|c| c.is_whitespace())
        .collect::<String>();
    let trimmed = val.trim();

    out.push_str(&leading_ws);
    if trimmed.is_empty() {
        return;
    }

    if (trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2)
        || (trimmed.starts_with('\'') && trimmed.ends_with('\'') && trimmed.len() >= 2)
    {
        out.push_str(&format!(
            r##"<tspan fill="#86efac">{}</tspan>"##,
            escape_xml(trimmed)
        ));
    } else {
        match trimmed.to_ascii_lowercase().as_str() {
            "true" | "false" | "yes" | "no" | "on" | "off" | "none" | "null" => {
                out.push_str(&format!(
                    r##"<tspan fill="#f472b6">{}</tspan>"##,
                    escape_xml(trimmed)
                ));
            }
            _ if is_ini_number(trimmed) => {
                out.push_str(&format!(
                    r##"<tspan fill="#fdba74">{}</tspan>"##,
                    escape_xml(trimmed)
                ));
            }
            _ => {
                out.push_str(&escape_xml(trimmed));
            }
        }
    }
    out.push_str(&trailing_ws);
}

fn is_ini_number(s: &str) -> bool {
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit());
    }
    let s = s
        .strip_prefix('+')
        .or_else(|| s.strip_prefix('-'))
        .unwrap_or(s);
    if s.is_empty() {
        return false;
    }
    let mut has_dot = false;
    let mut has_digit = false;
    for c in s.chars() {
        if c.is_ascii_digit() {
            has_digit = true;
        } else if c == '.' && !has_dot {
            has_dot = true;
        } else {
            return false;
        }
    }
    has_digit
}

fn split_ini_comment(val: &str) -> (&str, Option<&str>) {
    let mut in_quote: Option<char> = None;
    for (idx, c) in val.char_indices() {
        if let Some(q) = in_quote {
            if c == q {
                in_quote = None;
            }
        } else if c == '"' || c == '\'' {
            in_quote = Some(c);
        } else if (c == ';' || c == '#' || c == '!')
            && (idx == 0 || val.as_bytes()[idx - 1].is_ascii_whitespace())
        {
            return (&val[..idx], Some(&val[idx..]));
        }
    }
    (val, None)
}

const MAX_JSON_DEPTH: usize = 64;

fn skip_whitespace(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else {
            break;
        }
    }
}

/// Strictly validate that `input` conforms to JSON syntax (RFC 8259) with bounded nesting depth.
fn validate_json(input: &str) -> bool {
    let mut chars = input.chars().peekable();
    skip_whitespace(&mut chars);
    if chars.peek().is_none() {
        return false;
    }
    if !parse_value(&mut chars, 0) {
        return false;
    }
    skip_whitespace(&mut chars);
    chars.next().is_none()
}

fn parse_value(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, depth: usize) -> bool {
    if depth >= MAX_JSON_DEPTH {
        return false;
    }
    skip_whitespace(chars);
    match chars.peek() {
        Some('"') => parse_string(chars),
        Some('{') => parse_object(chars, depth),
        Some('[') => parse_array(chars, depth),
        Some('t') => parse_literal(chars, "true"),
        Some('f') => parse_literal(chars, "false"),
        Some('n') => parse_literal(chars, "null"),
        Some('-' | '0'..='9') => parse_number(chars),
        _ => false,
    }
}

fn parse_literal(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, expected: &str) -> bool {
    for exp in expected.chars() {
        if chars.next() != Some(exp) {
            return false;
        }
    }
    true
}

fn parse_string(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> bool {
    if chars.next() != Some('"') {
        return false;
    }
    while let Some(c) = chars.next() {
        if c == '"' {
            return true;
        }
        if c == '\\' {
            match chars.next() {
                Some('"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't') => {}
                Some('u') => {
                    for _ in 0..4 {
                        match chars.next() {
                            Some(h) if h.is_ascii_hexdigit() => {}
                            _ => return false,
                        }
                    }
                }
                _ => return false,
            }
        } else if (c as u32) < 0x20 {
            return false;
        }
    }
    false
}

fn parse_object(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, depth: usize) -> bool {
    chars.next(); // consume '{'
    skip_whitespace(chars);
    if chars.peek() == Some(&'}') {
        chars.next();
        return true;
    }
    loop {
        skip_whitespace(chars);
        if !parse_string(chars) {
            return false;
        }
        skip_whitespace(chars);
        if chars.next() != Some(':') {
            return false;
        }
        if !parse_value(chars, depth + 1) {
            return false;
        }
        skip_whitespace(chars);
        match chars.next() {
            Some(',') => continue,
            Some('}') => return true,
            _ => return false,
        }
    }
}

fn parse_array(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, depth: usize) -> bool {
    chars.next(); // consume '['
    skip_whitespace(chars);
    if chars.peek() == Some(&']') {
        chars.next();
        return true;
    }
    loop {
        if !parse_value(chars, depth + 1) {
            return false;
        }
        skip_whitespace(chars);
        match chars.next() {
            Some(',') => continue,
            Some(']') => return true,
            _ => return false,
        }
    }
}

fn parse_number(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> bool {
    if chars.peek() == Some(&'-') {
        chars.next();
    }
    match chars.next() {
        Some('0') => {}
        Some('1'..='9') => {
            while let Some(&d) = chars.peek() {
                if d.is_ascii_digit() {
                    chars.next();
                } else {
                    break;
                }
            }
        }
        _ => return false,
    }

    if chars.peek() == Some(&'.') {
        chars.next();
        let mut has_digit = false;
        while let Some(&d) = chars.peek() {
            if d.is_ascii_digit() {
                chars.next();
                has_digit = true;
            } else {
                break;
            }
        }
        if !has_digit {
            return false;
        }
    }

    if matches!(chars.peek(), Some(&'e' | &'E')) {
        chars.next();
        if matches!(chars.peek(), Some(&'+' | &'-')) {
            chars.next();
        }
        let mut has_digit = false;
        while let Some(&d) = chars.peek() {
            if d.is_ascii_digit() {
                chars.next();
                has_digit = true;
            } else {
                break;
            }
        }
        if !has_digit {
            return false;
        }
    }

    true
}

/// Format minified or long-line JSON into 2-space indented multiline JSON.
///
/// Returns `None` if the input is not strictly valid JSON, exceeds nesting limits, or is empty,
/// allowing callers to safely fall back to raw text.
pub fn pretty_format_json(input: &str) -> Option<String> {
    if !validate_json(input) {
        return None;
    }

    let max_output_size = input.len().saturating_mul(4).max(1024 * 1024);
    let mut out = String::with_capacity(input.len().saturating_add(input.len() / 2));
    let mut indent_level: usize = 0;
    let mut in_string = false;
    let mut escaped = false;
    let mut stack = Vec::new();

    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        if out.len() > max_output_size {
            return None;
        }

        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }

        match c {
            '"' => {
                in_string = true;
                out.push('"');
            }
            '{' => {
                let mut peek_chars = chars.clone();
                let mut is_empty_obj = false;
                for nc in peek_chars.by_ref() {
                    if nc.is_whitespace() {
                        continue;
                    }
                    if nc == '}' {
                        is_empty_obj = true;
                    }
                    break;
                }

                if is_empty_obj {
                    out.push_str("{}");
                    for nc in chars.by_ref() {
                        if nc == '}' {
                            break;
                        }
                    }
                } else {
                    if indent_level >= MAX_JSON_DEPTH {
                        return None;
                    }
                    stack.push('}');
                    indent_level += 1;
                    out.push('{');
                    out.push('\n');
                    push_indent(&mut out, indent_level);
                }
            }
            '[' => {
                let mut peek_chars = chars.clone();
                let mut is_empty_arr = false;
                for nc in peek_chars.by_ref() {
                    if nc.is_whitespace() {
                        continue;
                    }
                    if nc == ']' {
                        is_empty_arr = true;
                    }
                    break;
                }

                if is_empty_arr {
                    out.push_str("[]");
                    for nc in chars.by_ref() {
                        if nc == ']' {
                            break;
                        }
                    }
                } else {
                    if indent_level >= MAX_JSON_DEPTH {
                        return None;
                    }
                    stack.push(']');
                    indent_level += 1;
                    out.push('[');
                    out.push('\n');
                    push_indent(&mut out, indent_level);
                }
            }
            '}' => {
                if stack.pop() != Some('}') {
                    return None;
                }
                indent_level = indent_level.saturating_sub(1);
                out.push('\n');
                push_indent(&mut out, indent_level);
                out.push('}');
            }
            ']' => {
                if stack.pop() != Some(']') {
                    return None;
                }
                indent_level = indent_level.saturating_sub(1);
                out.push('\n');
                push_indent(&mut out, indent_level);
                out.push(']');
            }
            ':' => {
                out.push(':');
                out.push(' ');
            }
            ',' => {
                out.push(',');
                out.push('\n');
                push_indent(&mut out, indent_level);
            }
            c if c.is_whitespace() => {}
            c => {
                out.push(c);
            }
        }
    }

    if in_string || !stack.is_empty() {
        return None;
    }

    Some(out)
}

fn push_indent(out: &mut String, level: usize) {
    for _ in 0..level {
        out.push_str("  ");
    }
}

const MAX_XML_DEPTH: usize = 64;

/// Format minified or long-line XML into 2-space indented multiline XML.
///
/// Returns `None` if the input is empty or invalid/unbalanced, allowing callers to safely
/// fall back to raw text.
pub fn pretty_format_xml(input: &str) -> Option<String> {
    let input = input.trim_start_matches('\u{feff}').trim();
    if input.is_empty() {
        return None;
    }

    let mut out = String::with_capacity(input.len().saturating_add(input.len() / 2));
    let mut tag_stack: Vec<String> = Vec::new();
    let mut pos = 0;
    let bytes = input.as_bytes();

    while pos < bytes.len() {
        while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos >= bytes.len() {
            break;
        }

        if bytes[pos] == b'<' {
            if pos + 4 <= bytes.len() && &bytes[pos..pos + 4] == b"<!--" {
                let comment_end = input[pos + 4..].find("-->").map(|i| pos + 4 + i + 3)?;
                let comment = &input[pos..comment_end];
                push_indent(&mut out, tag_stack.len());
                out.push_str(comment.trim());
                out.push('\n');
                pos = comment_end;
            } else if pos + 9 <= bytes.len() && &bytes[pos..pos + 9] == b"<![CDATA[" {
                let cdata_end = input[pos + 9..].find("]]>").map(|i| pos + 9 + i + 3)?;
                let cdata = &input[pos..cdata_end];
                push_indent(&mut out, tag_stack.len());
                out.push_str(cdata);
                out.push('\n');
                pos = cdata_end;
            } else if pos + 2 <= bytes.len() && &bytes[pos..pos + 2] == b"<?" {
                let pi_end = input[pos + 2..].find("?>").map(|i| pos + 2 + i + 2)?;
                let pi = &input[pos..pi_end];
                push_indent(&mut out, tag_stack.len());
                out.push_str(pi.trim());
                out.push('\n');
                pos = pi_end;
            } else if pos + 9 <= bytes.len() && &bytes[pos..pos + 9] == b"<!DOCTYPE" {
                let mut dpos = pos + 9;
                let mut in_bracket = false;
                let mut found = false;
                while dpos < bytes.len() {
                    if bytes[dpos] == b'[' {
                        in_bracket = true;
                    } else if bytes[dpos] == b']' {
                        in_bracket = false;
                    } else if bytes[dpos] == b'>' && !in_bracket {
                        found = true;
                        dpos += 1;
                        break;
                    }
                    dpos += 1;
                }
                if !found {
                    return None;
                }
                let dt = &input[pos..dpos];
                push_indent(&mut out, tag_stack.len());
                out.push_str(dt.trim());
                out.push('\n');
                pos = dpos;
            } else if pos + 2 <= bytes.len() && &bytes[pos..pos + 2] == b"</" {
                let tag_end = find_tag_end(input, pos)?;
                let tag = &input[pos..tag_end];
                let tname = get_tag_name(tag)?;
                let expected = tag_stack.pop()?;
                if expected != tname {
                    return None;
                }
                push_indent(&mut out, tag_stack.len());
                out.push_str(tag);
                out.push('\n');
                pos = tag_end;
            } else {
                let tag_end = find_tag_end(input, pos)?;
                let tag = &input[pos..tag_end];
                let is_self_closing = tag.ends_with("/>");

                if !is_self_closing {
                    if let Some(tname) = get_tag_name(tag) {
                        let after_tag = &input[tag_end..];
                        if let Some(next_tag_start) = after_tag.find('<') {
                            let text_between = &after_tag[..next_tag_start];
                            let next_part = &after_tag[next_tag_start..];
                            let expected_closing = format!("</{tname}>");
                            if next_part.starts_with(&expected_closing)
                                && !text_between.contains('\n')
                            {
                                push_indent(&mut out, tag_stack.len());
                                out.push_str(tag);
                                out.push_str(text_between.trim());
                                out.push_str(&expected_closing);
                                out.push('\n');
                                pos = tag_end + next_tag_start + expected_closing.len();
                                continue;
                            }
                        }

                        if tag_stack.len() >= MAX_XML_DEPTH {
                            return None;
                        }
                        push_indent(&mut out, tag_stack.len());
                        out.push_str(tag);
                        out.push('\n');
                        pos = tag_end;
                        tag_stack.push(tname.to_string());
                    } else {
                        return None;
                    }
                } else {
                    push_indent(&mut out, tag_stack.len());
                    out.push_str(tag);
                    out.push('\n');
                    pos = tag_end;
                }
            }
        } else {
            let text_end = input[pos..]
                .find('<')
                .map(|i| pos + i)
                .unwrap_or(input.len());
            let text = &input[pos..text_end];
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                push_indent(&mut out, tag_stack.len());
                out.push_str(trimmed);
                out.push('\n');
            }
            pos = text_end;
        }
    }

    if tag_stack.is_empty() && !out.is_empty() {
        Some(out)
    } else {
        None
    }
}

fn find_tag_end(s: &str, start: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = start;
    let mut in_quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = in_quote {
            if b == q {
                in_quote = None;
            }
        } else if b == b'"' || b == b'\'' {
            in_quote = Some(b);
        } else if b == b'>' {
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

fn get_tag_name(tag: &str) -> Option<&str> {
    let inner = tag.strip_prefix('<')?.strip_suffix('>')?;
    let inner = inner.strip_prefix('/').unwrap_or(inner);
    let inner = inner.strip_suffix('/').unwrap_or(inner);
    let name = inner.split_whitespace().next()?;
    if name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == ':' || c == '.')
    {
        Some(name)
    } else {
        None
    }
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

    #[test]
    fn pretty_format_json_formats_minified() {
        let minified = r#"{"name":"Glanvu","version":1,"active":true,"tags":["fast","viewer"],"meta":{"empty_obj":{},"empty_arr":[]},"escaped":"quote: \" \\ hello"}"#;
        let formatted = pretty_format_json(minified).expect("should format valid JSON");
        let expected = r#"{
  "name": "Glanvu",
  "version": 1,
  "active": true,
  "tags": [
    "fast",
    "viewer"
  ],
  "meta": {
    "empty_obj": {},
    "empty_arr": []
  },
  "escaped": "quote: \" \\ hello"
}"#;
        assert_eq!(formatted, expected);
    }

    #[test]
    fn pretty_format_json_rejects_malformed() {
        assert!(pretty_format_json("").is_none());
        assert!(pretty_format_json("   ").is_none());
        assert!(pretty_format_json(r#"{"unclosed": "string"#).is_none());
        assert!(pretty_format_json(r#"{"unclosed_brace": 1"#).is_none());
        assert!(pretty_format_json(r#"[1, 2, }"#).is_none());
        // Malformed-but-balanced inputs
        assert!(pretty_format_json(r#"{ unquoted: 1 }"#).is_none());
        assert!(pretty_format_json(r#"{"a": 1 2}"#).is_none());
        assert!(pretty_format_json(r#"[1,,2]"#).is_none());
        assert!(pretty_format_json(r#"{"a": 1,}"#).is_none());
        assert!(pretty_format_json(r#"{"a": : 1}"#).is_none());
        // Nesting depth limit
        let deep = "[".repeat(65) + &"]".repeat(65);
        assert!(pretty_format_json(&deep).is_none());
        // Unicode escapes and escaped quotes
        let with_escapes = r#"{"msg":"\u0048\u0065\u006C\u006C\u006F\t\n\"world\""}"#;
        assert!(pretty_format_json(with_escapes).is_some());
    }

    #[test]
    fn text_document_loads_and_formats_json() {
        let dir = std::env::temp_dir().join("glanvu-text-json-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let json_path = dir.join("minified.json");
        std::fs::write(&json_path, r#"{"a":1,"b":[2,3]}"#).unwrap();
        let doc = TextDocument::load(&json_path).unwrap();
        assert_eq!(
            doc.content(),
            "{\n  \"a\": 1,\n  \"b\": [\n    2,\n    3\n  ]\n}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn text_document_paginates_large_json() {
        let items: Vec<String> = (1..=100)
            .map(|i| format!(r#"{{"id":{i},"name":"item{i}"}}"#))
            .collect();
        let minified_array = format!("[{}]", items.join(","));
        let formatted = pretty_format_json(&minified_array).unwrap();
        let doc = TextDocument::from_string(formatted);
        assert!(doc.page_count() > 1);
        let img = doc.render_fit(0).unwrap();
        assert_eq!(img.width, TEXT_PAGE_WIDTH);
    }

    #[test]
    fn text_document_loads_jsonl_preserving_lines() {
        let dir = std::env::temp_dir().join("glanvu-text-jsonl-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let jsonl_path = dir.join("data.jsonl");
        let lines = "{\"id\": 1, \"status\": \"ok\"}\n{\"id\": 2, \"status\": \"done\"}";
        std::fs::write(&jsonl_path, lines).unwrap();
        let doc = TextDocument::load(&jsonl_path).unwrap();
        assert_eq!(doc.content(), lines);
        assert_eq!(doc.pages[0][0].line_number, Some(1));
        assert_eq!(doc.pages[0][0].text, "{\"id\": 1, \"status\": \"ok\"}");
        assert_eq!(doc.pages[0][1].line_number, Some(2));
        assert_eq!(doc.pages[0][1].text, "{\"id\": 2, \"status\": \"done\"}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn highlight_json_row_renders_syntax_and_rainbow_guides() {
        let row = r#"    "name": "Glanvu","#; // 4 spaces -> Level 1 & 2 guides
        let svg = highlight_json_row(row, 0, false, false);
        // Level 1: gold/amber guide
        assert!(svg.contains(r##"fill="#eab308""##));
        // Level 2: sky blue guide
        assert!(svg.contains(r##"fill="#38bdf8""##));
        // Vertical guide symbol
        assert!(svg.contains('│'));
        // Property key color (cyan)
        assert!(svg.contains(r##"fill="#7dd3fc">&quot;name&quot;</tspan>"##));
        // String value color (mint green)
        assert!(svg.contains(r##"fill="#86efac">&quot;Glanvu&quot;</tspan>"##));
    }

    #[test]
    fn highlight_json_row_handles_multiline_string_continuation() {
        let cont_row = r#"continues here with numbers 123 and colons: and closes here","#;
        let svg = highlight_json_row(cont_row, 4, true, false);
        assert!(svg.contains(r##"fill="#86efac">continues here with numbers 123 and colons: and closes here&quot;</tspan>"##));
        assert!(svg.contains(r##"fill="#71717a">,</tspan>"##));
    }

    #[test]
    fn highlight_json_row_handles_escaped_quote_at_chunk_start() {
        let cont_row = r#""quote inside string\" and closes here","#;
        let svg = highlight_json_row(cont_row, 4, true, true);
        assert!(svg.contains(
            r##"fill="#86efac">&quot;quote inside string\&quot; and closes here&quot;</tspan>"##
        ));
        assert!(svg.contains(r##"fill="#71717a">,</tspan>"##));
    }

    #[test]
    fn pretty_format_xml_formats_minified() {
        let minified = r#"<?xml version="1.0"?><catalog><book id="1"><title>Rust Guide</title></book></catalog>"#;
        let formatted = pretty_format_xml(minified).expect("should format valid XML");
        let expected = "<?xml version=\"1.0\"?>\n<catalog>\n  <book id=\"1\">\n    <title>Rust Guide</title>\n  </book>\n</catalog>\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn pretty_format_xml_rejects_malformed() {
        assert!(pretty_format_xml("").is_none());
        assert!(pretty_format_xml("   ").is_none());
        assert!(pretty_format_xml("<unclosed>").is_none());
        assert!(pretty_format_xml("</mismatched>").is_none());
        assert!(pretty_format_xml("<open></mismatched>").is_none());
        assert!(pretty_format_xml("<root><child></root></child>").is_none());
    }

    #[test]
    fn text_document_loads_and_formats_xml() {
        let dir = std::env::temp_dir().join("glanvu-text-xml-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let xml_path = dir.join("minified.xml");
        std::fs::write(&xml_path, "<root><child>value</child></root>").unwrap();
        let doc = TextDocument::load(&xml_path).unwrap();
        assert_eq!(doc.content(), "<root>\n  <child>value</child>\n</root>\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn highlight_xml_row_renders_syntax_and_rainbow_guides() {
        let row = r#"    <book id="1" in_stock="true">"#;
        let svg = highlight_xml_row(row, 0, false, false, false, false, PartialDelim::None);
        // Rainbow guides
        assert!(svg.contains(r##"fill="#eab308""##));
        assert!(svg.contains(r##"fill="#38bdf8""##));
        assert!(svg.contains('│'));
        // Tag name (cyan)
        assert!(svg.contains(r##"fill="#7dd3fc">book</tspan>"##));
        // Attribute names (purple)
        assert!(svg.contains(r##"fill="#c084fc">id</tspan>"##));
        assert!(svg.contains(r##"fill="#c084fc">in_stock</tspan>"##));
        // Attribute values (mint green)
        assert!(svg.contains(r##"fill="#86efac">&quot;1&quot;</tspan>"##));
        assert!(svg.contains(r##"fill="#86efac">&quot;true&quot;</tspan>"##));
    }

    #[test]
    fn highlight_xml_row_handles_multiline_comment_and_string() {
        // Comment continuation
        let cont_comment = "still comment here --> after comment";
        let svg_comment = highlight_xml_row(
            cont_comment,
            0,
            false,
            true,
            false,
            false,
            PartialDelim::None,
        );
        assert!(svg_comment.contains(r##"fill="#71717a">still comment here --&gt;</tspan>"##));

        // String attribute continuation
        let cont_string = r#"still string" next_attr="val">"#;
        let svg_string =
            highlight_xml_row(cont_string, 0, true, false, true, false, PartialDelim::None);
        assert!(svg_string.contains(r##"fill="#86efac">still string&quot;</tspan>"##));
        assert!(svg_string.contains(r##"fill="#c084fc">next_attr</tspan>"##));
    }

    #[test]
    fn highlight_xml_row_handles_multiline_cdata() {
        let cont_cdata = "still cdata content ]]> and regular text";
        let svg = highlight_xml_row(cont_cdata, 0, false, false, false, true, PartialDelim::None);
        assert!(svg.contains(r##"fill="#f472b6">still cdata content ]]&gt;</tspan>"##));
    }

    #[test]
    fn xml_split_comment_open_across_wrapped_chunks() {
        // Opener "<!--" split across 92-col chunk boundary: 89 'a's + "<!-" on chunk 1, "-" on chunk 2
        let prefix = "a".repeat(89);
        let content = format!("{prefix}<!-- split comment body -->");
        let doc = TextDocument::from_string_with_format(content, SourceFormat::Xml);
        assert_eq!(doc.pages[0].len(), 2);
        assert_eq!(doc.pages[0][1].partial_delim, PartialDelim::CommentOpenDash);
        let svg = doc.page_svg(0).unwrap();
        // Both chunk 1 and chunk 2 delimiters/content must be colored as comment (#71717a)
        assert!(svg.contains(r##"fill="#71717a">&lt;!-</tspan>"##));
        assert!(svg.contains(r##"fill="#71717a">-</tspan>"##));
        assert!(svg.contains(r##"fill="#71717a"> split comment body --&gt;</tspan>"##));

        // Also test split after `<`: 91 'b's + "<" on chunk 1, "!--" on chunk 2
        let prefix2 = "b".repeat(91);
        let content2 = format!("{prefix2}<!-- split 2 -->");
        let doc2 = TextDocument::from_string_with_format(content2, SourceFormat::Xml);
        assert_eq!(doc2.pages[0].len(), 2);
        assert_eq!(doc2.pages[0][1].partial_delim, PartialDelim::OpenAngle);
        let svg2 = doc2.page_svg(0).unwrap();
        assert!(svg2.contains(r##"fill="#71717a">&lt;</tspan>"##));
        assert!(svg2.contains(r##"fill="#71717a">!--</tspan>"##));
        assert!(svg2.contains(r##"fill="#71717a"> split 2 --&gt;</tspan>"##));
    }

    #[test]
    fn xml_split_comment_close_across_wrapped_chunks() {
        // Closer "-->" split across chunk boundary: chunk 1 ends with "--", chunk 2 starts with "> <tag/>"
        let mut content = String::from("<!-- ");
        content.push_str(&"x".repeat(85)); // 5 + 85 = 90 chars
        content.push_str("--> <following_tag/>");
        let doc = TextDocument::from_string_with_format(content, SourceFormat::Xml);
        assert_eq!(doc.pages[0].len(), 2);
        assert_eq!(
            doc.pages[0][1].partial_delim,
            PartialDelim::CommentCloseDoubleDash
        );
        let svg = doc.page_svg(0).unwrap();
        // Comment closer '>' completed on chunk 2, following tag recognized as tag not comment
        assert!(svg.contains(r##"fill="#71717a">&gt;</tspan>"##));
        assert!(svg.contains(r##"fill="#7dd3fc">following_tag</tspan>"##));
    }

    #[test]
    fn xml_split_cdata_open_across_wrapped_chunks() {
        // Opener "<![CDATA[" split across chunk boundary: 88 'a's + "<![C" (len 4) on chunk 1, "DATA[..." on chunk 2
        let prefix = "a".repeat(88);
        let content = format!("{prefix}<![CDATA[split cdata body]]>");
        let doc = TextDocument::from_string_with_format(content, SourceFormat::Xml);
        assert_eq!(doc.pages[0].len(), 2);
        assert_eq!(doc.pages[0][1].partial_delim, PartialDelim::CdataOpen(4));
        let svg = doc.page_svg(0).unwrap();
        // Both chunk 1 partial opener and chunk 2 rest must be colored in CDATA pink (#f472b6)
        assert!(svg.contains(r##"fill="#f472b6">&lt;![C</tspan>"##));
        assert!(svg.contains(r##"fill="#f472b6">DATA[</tspan>"##));
        assert!(svg.contains(r##"fill="#f472b6">split cdata body]]&gt;</tspan>"##));
    }

    #[test]
    fn xml_split_cdata_close_across_wrapped_chunks() {
        // Closer "]]>" split across chunk boundary: chunk 1 ends with "]]", chunk 2 starts with "><after/>"
        let mut content = String::from("<![CDATA[");
        content.push_str(&"y".repeat(81)); // 9 + 81 = 90 chars
        content.push_str("]]><after_cdata/>");
        let doc = TextDocument::from_string_with_format(content, SourceFormat::Xml);
        assert_eq!(doc.pages[0].len(), 2);
        assert_eq!(
            doc.pages[0][1].partial_delim,
            PartialDelim::CdataCloseDoubleBracket
        );
        let svg = doc.page_svg(0).unwrap();
        // CDATA closer completed on chunk 2, following tag recognized as tag not CDATA
        assert!(svg.contains(r##"fill="#f472b6">&gt;</tspan>"##));
        assert!(svg.contains(r##"fill="#7dd3fc">after_cdata</tspan>"##));
    }

    #[test]
    fn xml_interrupted_comment_closer_does_not_falsely_close() {
        // Chunk 1 ends with single '-' inside comment.
        // Chunk 2 starts with '-x' (not followed by '>').
        // Chunk 3 starts with '>'.
        // The '>' in chunk 3 must NOT close the comment because the delimiter was interrupted by 'x'.
        let mut content = String::from("<!-- ");
        content.push_str(&"a".repeat(86)); // 5 + 86 = 91 chars
        content.push('-'); // char 92 -> chunk 1 ends with '-'
                           // Continuation chunks wrap at 92 (no indent spaces)
                           // Chunk 2 starts with '-x' and fills 92 chars
        content.push_str("-x");
        content.push_str(&"b".repeat(90)); // chunk 2 is 92 chars
                                           // Chunk 3 starts with '>' and text
        content.push_str("> still in comment -->");
        let doc = TextDocument::from_string_with_format(content, SourceFormat::Xml);
        assert_eq!(doc.pages[0].len(), 3);
        // Chunk 1 ends in CommentCloseDash
        assert_eq!(
            doc.pages[0][1].partial_delim,
            PartialDelim::CommentCloseDash
        );
        // Chunk 2 started with '-x' which interrupted the delimiter, so chunk 3 must NOT inherit CommentCloseDoubleDash
        assert_eq!(doc.pages[0][2].partial_delim, PartialDelim::None);
        assert!(doc.pages[0][2].starts_in_comment);
        let svg = doc.page_svg(0).unwrap();
        assert!(svg.contains(r##"fill="#71717a">&gt; still in comment --&gt;</tspan>"##));
    }

    #[test]
    fn xml_interrupted_cdata_closer_does_not_falsely_close() {
        // Chunk 1 ends with single ']' inside CDATA.
        // Chunk 2 starts with ']x' (interrupted).
        // Chunk 3 starts with '>'.
        // The '>' in chunk 3 must NOT close CDATA.
        let mut content = String::from("<![CDATA[");
        content.push_str(&"c".repeat(82)); // 9 + 82 = 91 chars
        content.push(']'); // char 92 -> chunk 1 ends with ']'
                           // Chunk 2 starts with ']x' and fills 92 chars
        content.push_str("]x");
        content.push_str(&"d".repeat(90)); // chunk 2 is 92 chars
                                           // Chunk 3 starts with '>' and text
        content.push_str("> still in cdata ]]>");
        let doc = TextDocument::from_string_with_format(content, SourceFormat::Xml);
        assert_eq!(doc.pages[0].len(), 3);
        // Chunk 1 ends in CdataCloseBracket
        assert_eq!(
            doc.pages[0][1].partial_delim,
            PartialDelim::CdataCloseBracket
        );
        // Chunk 2 started with ']x' which interrupted delimiter, so chunk 3 must NOT inherit CdataCloseDoubleBracket
        assert_eq!(doc.pages[0][2].partial_delim, PartialDelim::None);
        assert!(doc.pages[0][2].starts_in_cdata);
        let svg = doc.page_svg(0).unwrap();
        assert!(svg.contains(r##"fill="#f472b6">&gt; still in cdata ]]&gt;</tspan>"##));
    }

    #[test]
    fn text_document_loads_ini() {
        let dir = std::env::temp_dir().join("glanvu-text-ini-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let ini_path = dir.join("config.ini");
        let content = "[database]\nhost = 127.0.0.1\nport = 5432\nenabled = true\n; comment";
        std::fs::write(&ini_path, content).unwrap();
        let doc = TextDocument::load(&ini_path).unwrap();
        assert_eq!(doc.content(), content);
        assert_eq!(doc.page_count(), 1);
        let img = doc.render_fit(0).unwrap();
        assert_eq!(img.width, TEXT_PAGE_WIDTH);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn highlight_ini_row_renders_syntax() {
        // Section header
        let sec = "[server]";
        let sec_svg = highlight_ini_row(sec, 0, false, false, false, false);
        assert!(sec_svg.contains(r##"fill="#7dd3fc" font-weight="bold">server</tspan>"##));

        // Key-value pair with number and inline comment
        let kv = "    port = 8080 ; http port";
        let kv_svg = highlight_ini_row(kv, 0, false, false, false, false);
        // Rainbow guides
        assert!(kv_svg.contains('│'));
        // Key (sky blue)
        assert!(kv_svg.contains(r##"fill="#93c5fd">port</tspan>"##));
        // Number (orange)
        assert!(kv_svg.contains(r##"fill="#fdba74">8080</tspan>"##));
        // Comment (zinc-500)
        assert!(kv_svg.contains(r##"fill="#71717a">; http port</tspan>"##));

        // Key-value with boolean
        let bool_kv = "enabled = true";
        let bool_svg = highlight_ini_row(bool_kv, 0, false, false, false, false);
        assert!(bool_svg.contains(r##"fill="#f472b6">true</tspan>"##));

        // TOML table array [[bin]]
        let toml_arr = "[[bin]]";
        let toml_arr_svg = highlight_ini_row(toml_arr, 0, false, false, false, false);
        assert!(toml_arr_svg.contains(r##"fill="#cbd5e1">[[</tspan>"##));
        assert!(toml_arr_svg.contains(r##"fill="#7dd3fc" font-weight="bold">bin</tspan>"##));
        assert!(toml_arr_svg.contains(r##"fill="#cbd5e1">]]</tspan>"##));

        // Java properties comment with !
        let prop_comment = "! this is a comment";
        let prop_comment_svg = highlight_ini_row(prop_comment, 0, false, false, false, false);
        assert!(prop_comment_svg.contains(r##"fill="#71717a">! this is a comment</tspan>"##));

        // Java properties key: value
        let prop_kv = "app.name: Glanvu ! inline comment";
        let prop_svg = highlight_ini_row(prop_kv, 0, false, false, false, false);
        assert!(prop_svg.contains(r##"fill="#93c5fd">app.name</tspan>"##));
        assert!(prop_svg.contains(r##"fill="#71717a">:</tspan>"##));
        assert!(prop_svg.contains(r##"fill="#71717a">! inline comment</tspan>"##));
    }

    #[test]
    fn toml_wrapped_section_header_and_comment_across_chunks() {
        // Long TOML section header that wraps across chunks
        let long_sec = "[hooks.state.\"omo@sisyphuslabs:hooks/pre-tool-use-enforcing-unlimited-goal-budget.json:pre_tool_use:0:0\"]";
        let doc = TextDocument::from_string_with_format(long_sec.to_string(), SourceFormat::Toml);
        assert_eq!(doc.pages[0].len(), 2);
        assert!(doc.pages[0][1].starts_in_section);
        let svg = doc.page_svg(0).unwrap();
        // Section opener and closing brackets in grey
        assert!(svg.contains(r##"fill="#cbd5e1">[</tspan>"##));
        assert!(svg.contains(r##"fill="#cbd5e1">]</tspan>"##));
        // Section name in blue bold on both chunk 1 and chunk 2
        assert!(svg.contains(r##"fill="#7dd3fc" font-weight="bold">hooks.state.&quot;omo@sisyphuslabs:hooks/pre-tool-use-enforcing-unlimited-goal-budget.json:pre_t</tspan>"##));
        assert!(svg.contains(r##"fill="#7dd3fc" font-weight="bold">ool_use:0:0&quot;</tspan>"##));
        // Must NOT falsely parse ':' as a key-value delimiter or color string as green
        assert!(!svg.contains(r##"fill="#86efac">ool_use"##));

        // Long comment that wraps across chunks
        let long_comment = "# This is a very long comment that has a url: https://example.com/api/v1?query=sample-query-string-padding-limit";
        let doc_c =
            TextDocument::from_string_with_format(long_comment.to_string(), SourceFormat::Toml);
        assert_eq!(doc_c.pages[0].len(), 2);
        assert!(doc_c.pages[0][1].starts_in_comment);
        let svg_c = doc_c.page_svg(0).unwrap();
        // Must be in comment grey on both chunks, not falsely treated as key-value
        assert!(!svg_c.contains(r##"fill="#93c5fd""##));
    }

    #[test]
    fn text_document_loads_toml_and_env() {
        let dir = std::env::temp_dir().join("glanvu-text-toml-env-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // TOML file
        let toml_path = dir.join("Cargo.toml");
        let toml_content =
            "[package]\nname = \"glanvu\"\nversion = \"0.2.0\"\n[[bin]]\nname = \"glanvu\"";
        std::fs::write(&toml_path, toml_content).unwrap();
        let doc = TextDocument::load(&toml_path).unwrap();
        assert_eq!(doc.content(), toml_content);
        assert_eq!(doc.format, SourceFormat::Toml);
        let img = doc.render_fit(0).unwrap();
        assert_eq!(img.width, TEXT_PAGE_WIDTH);

        // .env dotfile
        let env_path = dir.join(".env");
        let env_content =
            "PORT=8080\nDEBUG=true\n# database url\nDATABASE_URL=\"postgres://localhost/db\"";
        std::fs::write(&env_path, env_content).unwrap();
        let doc_env = TextDocument::load(&env_path).unwrap();
        assert_eq!(doc_env.content(), env_content);
        assert_eq!(doc_env.format, SourceFormat::Ini);
        let img_env = doc_env.render_fit(0).unwrap();
        assert_eq!(img_env.width, TEXT_PAGE_WIDTH);

        // .bak file
        let bak_path = dir.join("config.toml.bak");
        let bak_content = "[server]\nhost = \"localhost\"\nport = 8080";
        std::fs::write(&bak_path, bak_content).unwrap();
        let doc_bak = TextDocument::load(&bak_path).unwrap();
        assert_eq!(doc_bak.content(), bak_content);
        assert_eq!(doc_bak.format, SourceFormat::Toml);
        let img_bak = doc_bak.render_fit(0).unwrap();
        assert_eq!(img_bak.width, TEXT_PAGE_WIDTH);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
