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

        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let format = SourceFormat::from_extension(ext).unwrap_or(SourceFormat::Txt);

        if format == SourceFormat::Json {
            let is_minified_or_wide =
                content.lines().count() <= 1 || content.lines().any(|l| l.chars().count() > 140);
            if is_minified_or_wide {
                if let Some(pretty) = pretty_format_json(&content) {
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
                });
            } else {
                let indent_spaces = expanded.chars().take_while(|&c| c == ' ').count();
                let chunks = wrap_line(&expanded, WRAP_COLS);
                for (chunk_idx, chunk) in chunks.into_iter().enumerate() {
                    let row_starts_in_string = in_string;
                    let row_starts_escaped = escaped;
                    if format == SourceFormat::Json || format == SourceFormat::Jsonl {
                        update_string_state(&chunk, &mut in_string, &mut escaped);
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
                    });
                }
            }
            in_string = false;
            escaped = false;
        }

        if visual_rows.is_empty() {
            visual_rows.push(VisualRow {
                line_number: Some(1),
                text: String::new(),
                indent_cols: 0,
                starts_in_string: false,
                starts_escaped: false,
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
}
