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

/// Layout orientation and target canvas aspect for paginated text documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PageLayout {
    /// Standard vertical document canvas (1200 × 1600), ~58 lines per page.
    #[default]
    Portrait,
    /// Widescreen panoramic canvas (1600 × 1000, 16:10), ~35 lines per page.
    Landscape,
}

impl PageLayout {
    /// Canvas pixel dimensions (width, height).
    pub fn dimensions(self) -> (u32, u32) {
        match self {
            Self::Portrait => (TEXT_PAGE_WIDTH, TEXT_PAGE_HEIGHT),
            Self::Landscape => (1600, 1000),
        }
    }

    /// Number of visual rows per page in this layout.
    pub fn lines_per_page(self) -> usize {
        match self {
            Self::Portrait => LINES_PER_PAGE,
            Self::Landscape => 35,
        }
    }

    /// Choose layout based on window or viewport aspect ratio.
    pub fn from_window_size(width: f32, height: f32) -> Self {
        if width > height {
            Self::Landscape
        } else {
            Self::Portrait
        }
    }
}

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
    /// Which quote character opened the string if this row starts inside an unclosed string literal.
    string_quote: Option<char>,
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
    /// Whether this row is inside a fenced Markdown code block.
    starts_in_code_block: bool,
    /// Markdown heading level (1-6) if this row is part of a heading.
    heading_level: Option<u8>,
    /// Whether this row is a wrapped continuation of a Markdown blockquote.
    is_blockquote_continuation: bool,
    /// Whether this row is part of a YAML frontmatter block in Markdown.
    is_frontmatter: bool,
    /// Whether this row is part of a Markdown table.
    is_table_row: bool,
    /// Whether this row is a Markdown table header row.
    is_table_header: bool,
    /// Whether this row is a Markdown/CSV table separator line.
    is_table_separator: bool,
    /// Preserved cell boundaries for table rows.
    table_cells: Option<Vec<String>>,
    /// For wrapped table row chunks, the starting column index (0-based).
    table_col_start: usize,
    /// Whether this row is the last chunk of its source line (always true for non-wrapped rows).
    is_last_chunk: bool,
}

/// A parsed plain text document, paginated and ready to render.
#[derive(Debug, Clone)]
pub struct TextDocument {
    content: String,
    pages: Vec<Vec<VisualRow>>,
    format: SourceFormat,
    layout: PageLayout,
    render_mode: u8,
}

impl TextDocument {
    /// Active page layout orientation.
    pub fn layout(&self) -> PageLayout {
        self.layout
    }

    /// Active render mode index.
    pub fn render_mode(&self) -> u8 {
        self.render_mode
    }

    /// Source format of the document.
    pub fn format(&self) -> SourceFormat {
        self.format
    }

    /// Parse and paginate a text document from in-memory UTF-8 bytes with default Txt format.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        Self::from_bytes_with_options(bytes, SourceFormat::Txt, PageLayout::Portrait, 0)
    }

    /// Parse and paginate a text document from in-memory UTF-8 bytes with an explicit format.
    pub fn from_bytes_with_format(bytes: &[u8], format: SourceFormat) -> Result<Self> {
        Self::from_bytes_with_options(bytes, format, PageLayout::Portrait, 0)
    }

    /// Parse and paginate a text document from in-memory UTF-8 bytes with layout and render mode options.
    pub fn from_bytes_with_options(
        bytes: &[u8],
        format: SourceFormat,
        layout: PageLayout,
        render_mode: u8,
    ) -> Result<Self> {
        if bytes.contains(&0) {
            return Err(Error::Decode(
                "binary file with null bytes is not plain text".to_string(),
            ));
        }
        let content = std::str::from_utf8(bytes)
            .map_err(|e| Error::Decode(format!("invalid UTF-8 text: {e}")))?
            .to_string();
        Ok(Self::from_string_with_options(
            content,
            format,
            layout,
            render_mode,
        ))
    }

    /// Load and paginate a text document from disk.
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::load_with_options(path, PageLayout::Portrait, 0)
    }

    /// Load and paginate a text document from disk with layout and render mode options.
    pub fn load_with_options<P: AsRef<Path>>(
        path: P,
        layout: PageLayout,
        render_mode: u8,
    ) -> Result<Self> {
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

        Ok(Self::from_string_with_options(
            content,
            format,
            layout,
            render_mode,
        ))
    }

    /// Build a paginated document from an already-decoded UTF-8 string with default Txt format.
    pub fn from_string(content: String) -> Self {
        Self::from_string_with_options(content, SourceFormat::Txt, PageLayout::Portrait, 0)
    }

    /// Build a paginated document from an already-decoded UTF-8 string with an explicit format.
    pub fn from_string_with_format(content: String, format: SourceFormat) -> Self {
        Self::from_string_with_options(content, format, PageLayout::Portrait, 0)
    }

    /// Build a paginated document with layout and render mode options.
    pub fn from_string_with_options(
        content: String,
        format: SourceFormat,
        layout: PageLayout,
        render_mode: u8,
    ) -> Self {
        let is_csv_table =
            (format == SourceFormat::Csv || format == SourceFormat::Tsv) && render_mode == 1;
        if is_csv_table {
            let delimiter = if format == SourceFormat::Tsv {
                '\t'
            } else {
                detect_csv_delimiter(&content)
            };
            return Self::build_csv_table_document(content, format, layout, delimiter);
        }

        let mut visual_rows = Vec::new();
        let mut in_string = false;
        let mut string_quote: Option<char> = None;
        let mut escaped = false;
        let mut in_comment = false;
        let mut in_tag = false;
        let mut in_cdata = false;
        let mut partial_delim = PartialDelim::None;
        let mut code_fence_state: Option<(char, usize)> = None;
        let mut in_frontmatter = false;
        let mut past_frontmatter = false;

        let cleaned_lines: Vec<String> = content.lines().map(sanitize_line).collect();
        let is_md_reader = format == SourceFormat::Md && render_mode == 1;
        let mut aligned_md_tables = if is_md_reader {
            build_aligned_markdown_tables(&cleaned_lines, layout)
        } else {
            std::collections::HashMap::new()
        };
        for (line_idx, cleaned) in cleaned_lines.iter().enumerate() {
            let expanded = if format == SourceFormat::Tsv {
                cleaned.clone()
            } else {
                expand_tabs(cleaned, TAB_SIZE)
            };
            if expanded.is_empty() {
                visual_rows.push(VisualRow {
                    line_number: Some(line_idx + 1),
                    text: String::new(),
                    indent_cols: 0,
                    string_quote: None,
                    starts_in_string: in_string,
                    starts_escaped: escaped,
                    starts_in_comment: in_comment,
                    starts_in_tag: in_tag,
                    starts_in_cdata: in_cdata,
                    starts_in_section: false,
                    is_value_continuation: false,
                    partial_delim,
                    starts_in_code_block: code_fence_state.is_some(),
                    heading_level: None,
                    is_blockquote_continuation: false,
                    is_frontmatter: in_frontmatter,
                    is_table_row: false,
                    is_table_header: false,
                    is_table_separator: false,
                    table_cells: None,
                    table_col_start: 0,
                    is_last_chunk: true,
                });
            } else {
                let is_ini_or_toml = format == SourceFormat::Ini || format == SourceFormat::Toml;
                let is_yaml = format == SourceFormat::Yaml;
                let is_md = format == SourceFormat::Md;
                let trimmed = expanded.trim_start();

                let is_frontmatter_line = if is_md && !past_frontmatter {
                    if line_idx == 0 && trimmed == "---" {
                        in_frontmatter = true;
                        true
                    } else if in_frontmatter {
                        if trimmed == "---" || trimmed == "..." {
                            in_frontmatter = false;
                            past_frontmatter = true;
                        }
                        true
                    } else {
                        past_frontmatter = true;
                        false
                    }
                } else {
                    false
                };

                let is_ini_comment = is_ini_or_toml
                    && (trimmed.starts_with(';')
                        || trimmed.starts_with('#')
                        || trimmed.starts_with('!'));
                let is_yaml_comment = is_yaml && trimmed.starts_with('#');
                let is_ini_section = is_ini_or_toml && trimmed.starts_with('[');
                let is_ini_kv = is_ini_or_toml
                    && !is_ini_comment
                    && !is_ini_section
                    && trimmed.contains(['=', ':']);
                let is_yaml_kv = is_yaml
                    && !is_yaml_comment
                    && (find_yaml_colon(trimmed).is_some()
                        || trimmed
                            .strip_prefix("- ")
                            .is_some_and(|s| find_yaml_colon(s).is_some()));
                let mut in_ini_section = false;

                let (is_code_fence, is_closing_fence) = if is_md && !is_frontmatter_line {
                    if let Some((open_char, open_len)) = code_fence_state {
                        let count = trimmed.chars().take_while(|&c| c == open_char).count();
                        if count >= open_len && trimmed[count..].trim().is_empty() {
                            (true, true)
                        } else {
                            (false, false)
                        }
                    } else if parse_code_fence(trimmed).is_some() {
                        (true, false)
                    } else {
                        (false, false)
                    }
                } else {
                    (false, false)
                };

                let line_starts_in_code_block = code_fence_state.is_some();
                let is_blockquote = is_md
                    && !is_frontmatter_line
                    && !line_starts_in_code_block
                    && trimmed.starts_with('>');
                let heading_level = if is_md && !is_frontmatter_line && !line_starts_in_code_block {
                    parse_heading_level(trimmed)
                } else {
                    None
                };

                let row_is_table_separator = is_md
                    && !is_frontmatter_line
                    && !line_starts_in_code_block
                    && is_table_separator(trimmed);
                let is_list_item = is_markdown_list_item(trimmed);
                let is_table_row = is_md
                    && !is_frontmatter_line
                    && !line_starts_in_code_block
                    && heading_level.is_none()
                    && !is_blockquote
                    && !is_list_item
                    && ((trimmed.starts_with('|')
                        || (trimmed.ends_with('|') && trimmed.contains('|')))
                        || row_is_table_separator);
                let is_table_header = is_table_row
                    && !row_is_table_separator
                    && line_idx + 1 < cleaned_lines.len()
                    && is_table_separator(cleaned_lines[line_idx + 1].trim());

                let is_md_reader = is_md && render_mode == 1;
                let (line_to_wrap, row_table_cells) =
                    if let Some(aligned) = aligned_md_tables.remove(&line_idx) {
                        match aligned {
                            AlignedTableEntry::Separator(s) => (s, None),
                            AlignedTableEntry::Row(text, cells) => (text, Some(cells)),
                        }
                    } else if is_md_reader && !is_frontmatter_line && !line_starts_in_code_block {
                        (strip_markdown_urls_for_reader(&expanded), None)
                    } else {
                        (expanded.clone(), None)
                    };

                let indent_spaces = line_to_wrap.chars().take_while(|&c| c == ' ').count();
                let chunks = if is_md_reader && is_table_row {
                    vec![line_to_wrap]
                } else {
                    let wrap_limit = if is_md_reader {
                        if !line_starts_in_code_block {
                            match layout {
                                PageLayout::Portrait => 118,
                                PageLayout::Landscape => 150,
                            }
                        } else {
                            match layout {
                                PageLayout::Portrait => WRAP_COLS,
                                PageLayout::Landscape => 130,
                            }
                        }
                    } else {
                        match layout {
                            PageLayout::Portrait => WRAP_COLS,
                            PageLayout::Landscape => 130,
                        }
                    };
                    wrap_line(&line_to_wrap, wrap_limit)
                };
                let num_chunks = chunks.len();
                let mut running_table_col = 0usize;
                for (chunk_idx, chunk) in chunks.into_iter().enumerate() {
                    let is_chunk_0 = chunk_idx == 0;
                    let is_last_chunk = chunk_idx + 1 == num_chunks;
                    let chunk_table_col_start = running_table_col;
                    if is_table_row && !row_is_table_separator {
                        running_table_col = table_row_chunk_end_col(
                            &chunk,
                            running_table_col,
                            is_chunk_0,
                            is_last_chunk,
                        );
                    }
                    let row_string_quote = string_quote;
                    let row_starts_in_string = in_string || string_quote.is_some();
                    let row_starts_escaped = escaped;
                    let row_starts_in_comment = in_comment;
                    let row_starts_in_tag = in_tag;
                    let row_starts_in_cdata = in_cdata;
                    let row_starts_in_section = in_ini_section;
                    let row_is_value_continuation = (is_ini_kv || is_yaml_kv) && chunk_idx > 0;
                    let row_partial_delim = partial_delim;
                    let row_starts_in_code_block = if is_code_fence {
                        false
                    } else {
                        line_starts_in_code_block
                    };
                    let row_heading_level = heading_level;
                    let row_is_blockquote_continuation = is_blockquote && chunk_idx > 0;

                    match format {
                        SourceFormat::Json | SourceFormat::Jsonl => {
                            update_string_state(&chunk, &mut in_string, &mut escaped);
                            string_quote = if in_string { Some('"') } else { None };
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
                            string_quote = if in_string { Some('"') } else { None };
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
                                string_quote = if in_string { Some('"') } else { None };
                            }
                        }
                        SourceFormat::Yaml => {
                            if is_yaml_comment {
                                in_comment = true;
                            } else {
                                update_yaml_string_state(&chunk, &mut string_quote, &mut escaped);
                                in_string = string_quote.is_some();
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
                        string_quote: row_string_quote,
                        starts_in_string: row_starts_in_string,
                        starts_escaped: row_starts_escaped,
                        starts_in_comment: row_starts_in_comment,
                        starts_in_tag: row_starts_in_tag,
                        starts_in_cdata: row_starts_in_cdata,
                        starts_in_section: row_starts_in_section,
                        is_value_continuation: row_is_value_continuation,
                        partial_delim: row_partial_delim,
                        starts_in_code_block: row_starts_in_code_block,
                        heading_level: row_heading_level,
                        is_blockquote_continuation: row_is_blockquote_continuation,
                        is_frontmatter: is_frontmatter_line,
                        is_table_row,
                        is_table_header,
                        is_table_separator: row_is_table_separator,
                        table_cells: row_table_cells.clone(),
                        table_col_start: chunk_table_col_start,
                        is_last_chunk,
                    });
                }

                if is_code_fence {
                    if is_closing_fence {
                        code_fence_state = None;
                    } else if let Some((ch, count, _)) = parse_code_fence(trimmed) {
                        code_fence_state = Some((ch, count));
                    }
                }
            }
            if format != SourceFormat::Xml
                && format != SourceFormat::Yaml
                && format != SourceFormat::Md
            {
                string_quote = None;
                in_string = false;
                escaped = false;
                in_comment = false;
                in_tag = false;
                in_cdata = false;
                partial_delim = PartialDelim::None;
            } else if format == SourceFormat::Yaml {
                escaped = false;
                in_comment = false;
            } else if format == SourceFormat::Md {
                escaped = false;
                in_comment = false;
                string_quote = None;
            }
        }

        if visual_rows.is_empty() {
            visual_rows.push(VisualRow {
                line_number: Some(1),
                text: String::new(),
                indent_cols: 0,
                string_quote: None,
                starts_in_string: false,
                starts_escaped: false,
                starts_in_comment: false,
                starts_in_tag: false,
                starts_in_cdata: false,
                starts_in_section: false,
                is_value_continuation: false,
                partial_delim: PartialDelim::None,
                starts_in_code_block: false,
                heading_level: None,
                is_blockquote_continuation: false,
                is_frontmatter: false,
                is_table_row: false,
                is_table_header: false,
                is_table_separator: false,
                table_cells: None,
                table_col_start: 0,
                is_last_chunk: true,
            });
        }

        let lines_per_page = layout.lines_per_page();
        let pages = visual_rows
            .chunks(lines_per_page)
            .map(|c| c.to_vec())
            .collect();

        Self {
            content,
            pages,
            format,
            layout,
            render_mode,
        }
    }

    fn build_csv_table_document(
        content: String,
        format: SourceFormat,
        layout: PageLayout,
        delimiter: char,
    ) -> Self {
        let max_width: usize = match layout {
            PageLayout::Portrait => 114,
            PageLayout::Landscape => 154,
        };
        let max_cols = (max_width + 3) / 6;
        let records = parse_csv_rows(&content, delimiter);
        let total_cols = records.iter().map(|r| r.len()).max().unwrap_or(0);
        let (num_cols, has_overflow) = if total_cols > max_cols && max_cols > 1 {
            (max_cols - 1, true)
        } else {
            (total_cols.min(max_cols), false)
        };
        let omitted_cols = if has_overflow {
            total_cols - num_cols
        } else {
            0
        };
        let total_display_cols = if has_overflow { num_cols + 1 } else { num_cols };

        if records.is_empty() || total_display_cols == 0 {
            return Self {
                content,
                pages: vec![vec![VisualRow {
                    line_number: Some(1),
                    text: "(empty table)".to_string(),
                    indent_cols: 0,
                    string_quote: None,
                    starts_in_string: false,
                    starts_escaped: false,
                    starts_in_comment: false,
                    starts_in_tag: false,
                    starts_in_cdata: false,
                    starts_in_section: false,
                    is_value_continuation: false,
                    partial_delim: PartialDelim::None,
                    starts_in_code_block: false,
                    heading_level: None,
                    is_blockquote_continuation: false,
                    is_frontmatter: false,
                    is_table_row: true,
                    is_table_header: false,
                    is_table_separator: false,
                    table_cells: None,
                    table_col_start: 0,
                    is_last_chunk: true,
                }]],
                format,
                layout,
                render_mode: 1,
            };
        }

        // Determine column alignment: numeric columns right-aligned, text left-aligned
        let data_rows = if records.len() > 1 {
            &records[1..]
        } else {
            &records[..]
        };
        let mut align_right = vec![false; total_display_cols];
        for (c, align) in align_right.iter_mut().take(num_cols).enumerate() {
            let mut non_empty_count = 0usize;
            let mut numeric_count = 0usize;
            for r in data_rows {
                if let Some(cell) = r.get(c) {
                    let trimmed = cell.trim();
                    if !trimmed.is_empty() {
                        non_empty_count += 1;
                        if is_numeric_field(trimmed) {
                            numeric_count += 1;
                        }
                    }
                }
            }
            if non_empty_count > 0 && numeric_count * 10 >= non_empty_count * 7 {
                *align = true;
            }
        }

        // Available table character width
        let divider_chars = total_display_cols.saturating_sub(1) * 3; // " | " is 3 chars
        let available_space = max_width.saturating_sub(divider_chars);

        // Compute natural width per column
        let mut col_widths = vec![3usize; total_display_cols];
        for r in &records {
            for (c, cell) in r.iter().take(num_cols).enumerate() {
                let cell_len = cell.chars().count();
                col_widths[c] = col_widths[c].max(cell_len);
            }
        }
        let min_overflow_w = if has_overflow {
            let overflow_label = format!("+{omitted_cols} more");
            let len = overflow_label.chars().count();
            col_widths[num_cols] = col_widths[num_cols].max(len);
            len
        } else {
            0
        };

        // Proportionally scale / shrink wide columns if total exceeds available space
        let mut total_w: usize = col_widths.iter().sum();
        if total_w > available_space && available_space > 0 {
            while total_w > available_space {
                let mut max_w = 0;
                for (c, &w) in col_widths.iter().enumerate() {
                    let min_w = if has_overflow && c == num_cols {
                        min_overflow_w
                    } else {
                        1
                    };
                    if w > min_w && w > max_w {
                        max_w = w;
                    }
                }
                if max_w == 0 {
                    break;
                }
                for (c, w) in col_widths.iter_mut().enumerate() {
                    let min_w = if has_overflow && c == num_cols {
                        min_overflow_w
                    } else {
                        1
                    };
                    if *w == max_w && *w > min_w && total_w > available_space {
                        *w -= 1;
                        total_w -= 1;
                    }
                }
            }
        }

        // Helper to format a single record
        let format_row = |row: &[String], is_header: bool| -> (String, Vec<String>) {
            let mut formatted_cells = Vec::with_capacity(total_display_cols);
            for c in 0..num_cols {
                let width = col_widths[c];
                let raw_val = row.get(c).map(|s| s.as_str()).unwrap_or("");
                let sanitized: String = raw_val
                    .replace('│', "|")
                    .replace('┼', "+")
                    .replace('─', "-");
                let cell_chars: Vec<char> = sanitized.chars().collect();
                let truncated = if cell_chars.len() > width {
                    if width > 1 {
                        let keep = width - 1;
                        let mut s: String = cell_chars[..keep].iter().collect();
                        s.push('…');
                        s
                    } else {
                        "…".to_string()
                    }
                } else {
                    sanitized
                };
                let aligned = if align_right[c] {
                    format!("{truncated:>width$}")
                } else {
                    format!("{truncated:<width$}")
                };
                formatted_cells.push(aligned);
            }
            if has_overflow {
                let width = col_widths[num_cols];
                let label = if is_header {
                    format!("+{omitted_cols} more")
                } else {
                    "…".to_string()
                };
                let cell_chars: Vec<char> = label.chars().collect();
                let truncated = if cell_chars.len() > width {
                    if width > 1 {
                        let keep = width - 1;
                        let mut s: String = cell_chars[..keep].iter().collect();
                        s.push('…');
                        s
                    } else {
                        "…".to_string()
                    }
                } else {
                    label
                };
                formatted_cells.push(format!("{truncated:<width$}"));
            }
            let joined = formatted_cells.join(" | ");
            (joined, formatted_cells)
        };

        let (header_text, header_cells) = format_row(&records[0], true);

        // Separator row: "──────┼────────────────┼────────"
        let sep_parts: Vec<String> = col_widths.iter().map(|&w| "─".repeat(w)).collect();
        let sep_text = sep_parts.join("─┼─");

        let lines_per_page = layout.lines_per_page();
        let data_rows_per_page = lines_per_page.saturating_sub(2).max(1);

        let data_slice = if records.len() > 1 {
            &records[1..]
        } else {
            &[]
        };

        let mut pages = Vec::new();

        if data_slice.is_empty() {
            pages.push(vec![
                VisualRow {
                    line_number: Some(1),
                    text: header_text,
                    indent_cols: 0,
                    string_quote: None,
                    starts_in_string: false,
                    starts_escaped: false,
                    starts_in_comment: false,
                    starts_in_tag: false,
                    starts_in_cdata: false,
                    starts_in_section: false,
                    is_value_continuation: false,
                    partial_delim: PartialDelim::None,
                    starts_in_code_block: false,
                    heading_level: None,
                    is_blockquote_continuation: false,
                    is_frontmatter: false,
                    is_table_row: true,
                    is_table_header: true,
                    is_table_separator: false,
                    table_cells: Some(header_cells),
                    table_col_start: 0,
                    is_last_chunk: true,
                },
                VisualRow {
                    line_number: None,
                    text: sep_text,
                    indent_cols: 0,
                    string_quote: None,
                    starts_in_string: false,
                    starts_escaped: false,
                    starts_in_comment: false,
                    starts_in_tag: false,
                    starts_in_cdata: false,
                    starts_in_section: false,
                    is_value_continuation: false,
                    partial_delim: PartialDelim::None,
                    starts_in_code_block: false,
                    heading_level: None,
                    is_blockquote_continuation: false,
                    is_frontmatter: false,
                    is_table_row: true,
                    is_table_header: false,
                    is_table_separator: true,
                    table_cells: None,
                    table_col_start: 0,
                    is_last_chunk: true,
                },
            ]);
        } else {
            for chunk in data_slice.chunks(data_rows_per_page) {
                let mut page_rows = Vec::with_capacity(chunk.len() + 2);
                page_rows.push(VisualRow {
                    line_number: Some(1),
                    text: header_text.clone(),
                    indent_cols: 0,
                    string_quote: None,
                    starts_in_string: false,
                    starts_escaped: false,
                    starts_in_comment: false,
                    starts_in_tag: false,
                    starts_in_cdata: false,
                    starts_in_section: false,
                    is_value_continuation: false,
                    partial_delim: PartialDelim::None,
                    starts_in_code_block: false,
                    heading_level: None,
                    is_blockquote_continuation: false,
                    is_frontmatter: false,
                    is_table_row: true,
                    is_table_header: true,
                    is_table_separator: false,
                    table_cells: Some(header_cells.clone()),
                    table_col_start: 0,
                    is_last_chunk: true,
                });
                page_rows.push(VisualRow {
                    line_number: None,
                    text: sep_text.clone(),
                    indent_cols: 0,
                    string_quote: None,
                    starts_in_string: false,
                    starts_escaped: false,
                    starts_in_comment: false,
                    starts_in_tag: false,
                    starts_in_cdata: false,
                    starts_in_section: false,
                    is_value_continuation: false,
                    partial_delim: PartialDelim::None,
                    starts_in_code_block: false,
                    heading_level: None,
                    is_blockquote_continuation: false,
                    is_frontmatter: false,
                    is_table_row: true,
                    is_table_header: false,
                    is_table_separator: true,
                    table_cells: None,
                    table_col_start: 0,
                    is_last_chunk: true,
                });
                for rec in chunk {
                    let (rec_text, rec_cells) = format_row(rec, false);
                    page_rows.push(VisualRow {
                        line_number: None,
                        text: rec_text,
                        indent_cols: 0,
                        string_quote: None,
                        starts_in_string: false,
                        starts_escaped: false,
                        starts_in_comment: false,
                        starts_in_tag: false,
                        starts_in_cdata: false,
                        starts_in_section: false,
                        is_value_continuation: false,
                        partial_delim: PartialDelim::None,
                        starts_in_code_block: false,
                        heading_level: None,
                        is_blockquote_continuation: false,
                        is_frontmatter: false,
                        is_table_row: true,
                        is_table_header: false,
                        is_table_separator: false,
                        table_cells: Some(rec_cells),
                        table_col_start: 0,
                        is_last_chunk: true,
                    });
                }
                pages.push(page_rows);
            }
        }

        Self {
            content,
            pages,
            format,
            layout,
            render_mode: 1,
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
        let (pw, ph) = self.layout.dimensions();
        Ok((pw as f32, ph as f32))
    }

    /// Generate the SVG representation of a given page (0-based).
    pub fn page_svg(&self, page_index: usize) -> Result<String> {
        let rows = self
            .pages
            .get(page_index)
            .ok_or_else(|| Error::Decode(format!("page {page_index} out of range")))?;

        let (page_w, page_h) = self.layout.dimensions();
        let is_md_reader = self.format == SourceFormat::Md && self.render_mode == 1;
        let is_csv_table = (self.format == SourceFormat::Csv || self.format == SourceFormat::Tsv)
            && self.render_mode == 1;
        let is_advanced = is_md_reader || is_csv_table;
        let mut svg = String::with_capacity(8192);
        svg.push_str(&format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="{page_w}" height="{page_h}" viewBox="0 0 {page_w} {page_h}">
<rect width="100%" height="100%" fill="#121214"/>
"##
        ));
        if !is_advanced {
            svg.push_str(&format!(
                r##"<line x1="92" y1="25" x2="92" y2="{}" stroke="#27272a" stroke-width="1"/>
"##,
                page_h - 25
            ));
        }

        let top_y = 35;
        let line_height = 26;
        let text_x = if is_advanced { 48 } else { 110 };
        let default_font = if is_md_reader {
            "-apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, Helvetica, Arial, sans-serif"
        } else {
            "ui-monospace, Menlo, Monaco, Consolas, monospace"
        };

        for (row_idx, row) in rows.iter().enumerate() {
            let y = top_y + (row_idx as i32 + 1) * line_height;
            if !is_advanced {
                if let Some(num) = row.line_number {
                    svg.push_str(&format!(
                        r##"<text x="80" y="{y}" text-anchor="end" fill="#52525b" font-family="ui-monospace, Menlo, Monaco, Consolas, monospace" font-size="16">{num}</text>
"##
                    ));
                }
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
                    SourceFormat::Yaml => highlight_yaml_row(
                        &row.text,
                        row.indent_cols,
                        row.string_quote,
                        row.starts_in_comment,
                        row.is_value_continuation,
                    ),
                    SourceFormat::Md => highlight_md_row(
                        &row.text,
                        row.indent_cols,
                        row.starts_in_code_block,
                        row.heading_level,
                        row.is_blockquote_continuation,
                        row.is_frontmatter,
                        row.is_table_row,
                        row.is_table_header,
                        row.is_table_separator,
                        row.table_cells.as_deref(),
                        row.table_col_start,
                        row.line_number.is_some(),
                        row.is_last_chunk,
                        self.render_mode,
                        self.layout,
                    ),
                    SourceFormat::Csv | SourceFormat::Tsv => {
                        let delimiter = if self.format == SourceFormat::Tsv {
                            '\t'
                        } else {
                            detect_csv_delimiter(&self.content)
                        };
                        if self.render_mode == 1 {
                            highlight_csv_table_row(
                                &row.text,
                                row.is_table_header,
                                row.table_cells.as_deref(),
                            )
                        } else {
                            highlight_csv_raw_row(&row.text, delimiter, row.indent_cols)
                        }
                    }
                    _ => {
                        let mut s = String::with_capacity(row.indent_cols + row.text.len());
                        for _ in 0..row.indent_cols {
                            s.push(' ');
                        }
                        s.push_str(&escape_xml(&row.text));
                        s
                    }
                };
                let font_size = if is_csv_table || (is_md_reader && row.is_table_row) {
                    "15"
                } else if is_md_reader {
                    "18"
                } else {
                    "19"
                };
                let row_font = if is_csv_table
                    || (is_md_reader && (row.starts_in_code_block || row.is_table_row))
                {
                    "ui-monospace, Menlo, Monaco, Consolas, monospace"
                } else {
                    default_font
                };
                svg.push_str(&format!(
                    r##"<text x="{text_x}" y="{y}" fill="#e4e4e7" font-family="{row_font}" font-size="{font_size}" xml:space="preserve">{inner}</text>
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

    /// Render a page (0-based) at its native resolution according to layout.
    pub fn render_fit(&self, page_index: usize) -> Result<DecodedImage> {
        let (pw, ph) = self.layout.dimensions();
        self.render_page(page_index, pw, ph)
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

fn find_wrap_break_pos(
    chars: &[char],
    start: usize,
    end: usize,
    search_start: usize,
) -> Option<usize> {
    let candidates: Vec<usize> = (search_start..end)
        .filter(|&i| chars[i].is_whitespace())
        .collect();
    if candidates.is_empty() {
        return None;
    }

    let mut depth_at = vec![0i32; end - start];
    let mut d = 0i32;
    for (offset, &c) in chars[start..end].iter().enumerate() {
        if c == '[' || c == '(' {
            d += 1;
        }
        depth_at[offset] = d;
        if c == ']' || c == ')' {
            d = d.saturating_sub(1);
        }
    }

    for &pos in candidates.iter().rev() {
        let offset = pos - start;
        if depth_at[offset] == 0 {
            return Some(pos + 1);
        }
    }

    candidates.last().map(|&pos| pos + 1)
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
    let search_start = start + max_cols.saturating_sub(max_cols / 2);
    let break_pos = find_wrap_break_pos(&chars, start, first_end, search_start);
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

        let search_start = start + cont_max.saturating_sub(cont_max / 2);
        let break_pos = find_wrap_break_pos(&chars, start, end, search_start);

        let actual_end = break_pos.unwrap_or(end);
        result.push(chars[start..actual_end].iter().collect());
        start = actual_end;
    }

    result
}

/// Strip ANSI escape sequences and non-printable control characters so terminal logs render cleanly.
/// Also maps problematic emoji/presentation selectors to outline-safe Unicode symbols so resvg
/// does not trigger Apple Color Emoji font fallback on macOS (which turns whole lines into [?] tofu).
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
        // Variation selectors (e.g. \u{FE0F} emoji presentation selector) trigger
        // Apple Color Emoji fallback on macOS, turning entire lines into [?].
        if c == '\u{FE0F}' || c == '\u{FE0E}' {
            continue;
        }
        match c {
            '\u{2705}' => out.push('✓'),              // ✅ -> ✓
            '\u{274C}' | '\u{274E}' => out.push('✗'), // ❌ -> ✗
            '\u{270F}' => out.push('✎'),              // ✏ -> ✎
            '\u{2728}' => out.push('✦'),              // ✨ -> ✦
            '\u{26A0}' => out.push('⚠'),              // ⚠ -> ⚠ (safe without FE0F)
            '\u{2139}' => out.push('ℹ'),              // ℹ -> ℹ
            '\u{2699}' => out.push('⚙'),              // ⚙ -> ⚙
            '\u{2764}' => out.push('♥'),              // ❤ -> ♥
            '\u{2B50}' => out.push('★'),              // ⭐ -> ★
            // SMP emoji block (\u{1F000}..=\u{1FAFF})
            '\u{1F44D}' => out.push_str("[+1]"),        // 👍
            '\u{1F44E}' => out.push_str("[-1]"),        // 👎
            '\u{1F680}' => out.push('▲'),               // 🚀
            '\u{1F4A1}' => out.push('◆'),               // 💡
            '\u{1F525}' => out.push('★'),               // 🔥
            '\u{1F4CC}' | '\u{1F4CD}' => out.push('•'), // 📌 / 📍
            '\u{1F4C1}' | '\u{1F4C2}' => out.push_str("[dir]"), // 📁 / 📂
            '\u{1F4C4}' | '\u{1F4DD}' => out.push_str("[doc]"), // 📄 / 📝
            '\u{1F517}' => out.push_str("[link]"),      // 🔗
            '\u{1F50D}' | '\u{1F50E}' => out.push_str("[search]"), // 🔍
            '\u{1F4F8}' | '\u{1F5BC}' => out.push_str("[img]"), // 📸 / 🖼
            '\u{1F389}' => out.push('✦'),               // 🎉
            '\u{1F4E6}' => out.push('■'),               // 📦
            c if ('\u{1F000}'..='\u{1FAFF}').contains(&c) => out.push('•'), // General emoji fallback
            _ => {
                if c == '\t' || !c.is_control() {
                    out.push(c);
                }
            }
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

fn update_yaml_string_state(text: &str, string_quote: &mut Option<char>, escaped: &mut bool) {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = *string_quote {
            if q == '"' {
                if *escaped {
                    *escaped = false;
                } else if c == '\\' {
                    *escaped = true;
                } else if c == '"' {
                    *string_quote = None;
                }
            } else if q == '\'' && c == '\'' {
                if i + 1 < chars.len() && chars[i + 1] == '\'' {
                    i += 1;
                } else {
                    *string_quote = None;
                }
            }
        } else {
            if c == '#' && (i == 0 || chars[i - 1].is_whitespace()) {
                break;
            }
            if c == '"' {
                *string_quote = Some('"');
            } else if c == '\'' {
                *string_quote = Some('\'');
            }
        }
        i += 1;
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

fn highlight_yaml_row(
    text: &str,
    indent_cols: usize,
    string_quote: Option<char>,
    starts_in_comment: bool,
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

    if let Some(open_quote) = string_quote {
        let mut string_content = String::new();
        let mut closed = false;
        let mut chars = rest.chars().peekable();
        let mut escaped = false;
        while let Some(c) = chars.next() {
            string_content.push(c);
            if open_quote == '"' {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    closed = true;
                    break;
                }
            } else if open_quote == '\'' && c == '\'' {
                if chars.peek() == Some(&'\'') {
                    string_content.push(chars.next().unwrap());
                } else {
                    closed = true;
                    break;
                }
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

    if rest.starts_with('#') {
        out.push_str(&format!(
            r##"<tspan fill="#71717a">{}</tspan>"##,
            escape_xml(rest)
        ));
        return out;
    }

    // Directives (%YAML, %TAG)
    if rest.starts_with('%') {
        out.push_str(&format!(
            r##"<tspan fill="#c084fc">{}</tspan>"##,
            escape_xml(rest)
        ));
        return out;
    }

    // Document start / end markers (---, ...)
    if rest.starts_with("---") || rest.starts_with("...") {
        let marker = &rest[..3];
        let trailing = &rest[3..];
        out.push_str(&format!(
            r##"<tspan fill="#a78bfa" font-weight="bold">{}</tspan>"##,
            escape_xml(marker)
        ));
        if !trailing.is_empty() {
            let (trail_val, comment_part) = split_yaml_comment(trailing);
            if !trail_val.is_empty() {
                highlight_yaml_value(&mut out, trail_val);
            }
            if let Some(c) = comment_part {
                out.push_str(&format!(
                    r##"<tspan fill="#71717a">{}</tspan>"##,
                    escape_xml(c)
                ));
            }
        }
        return out;
    }

    if is_value_continuation {
        let (val_part, comment_part) = split_yaml_comment(rest);
        highlight_yaml_value(&mut out, val_part);
        if let Some(c) = comment_part {
            out.push_str(&format!(
                r##"<tspan fill="#71717a">{}</tspan>"##,
                escape_xml(c)
            ));
        }
        return out;
    }

    // List item bullet
    let (item_prefix, after_bullet) = if let Some(stripped) = rest.strip_prefix("- ") {
        (Some("- "), stripped)
    } else if rest == "-" {
        (Some("-"), "")
    } else {
        (None, rest)
    };

    if let Some(prefix) = item_prefix {
        out.push_str(&format!(
            r##"<tspan fill="#fdba74" font-weight="bold">{}</tspan>"##,
            escape_xml(prefix)
        ));
        if after_bullet.is_empty() {
            return out;
        }
    }

    if after_bullet.starts_with('#') {
        out.push_str(&format!(
            r##"<tspan fill="#71717a">{}</tspan>"##,
            escape_xml(after_bullet)
        ));
        return out;
    }

    if let Some(colon_pos) = find_yaml_colon(after_bullet) {
        let key = &after_bullet[..colon_pos];
        let delim = &after_bullet[colon_pos..colon_pos + 1];
        let val = &after_bullet[colon_pos + 1..];

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

        let (val_part, comment_part) = split_yaml_comment(val);
        highlight_yaml_value(&mut out, val_part);
        if let Some(c) = comment_part {
            out.push_str(&format!(
                r##"<tspan fill="#71717a">{}</tspan>"##,
                escape_xml(c)
            ));
        }
        return out;
    }

    let (val_part, comment_part) = split_yaml_comment(after_bullet);
    highlight_yaml_value(&mut out, val_part);
    if let Some(c) = comment_part {
        out.push_str(&format!(
            r##"<tspan fill="#71717a">{}</tspan>"##,
            escape_xml(c)
        ));
    }

    out
}

fn find_yaml_colon(s: &str) -> Option<usize> {
    let mut in_quote = None;
    let mut escaped = false;
    let chars: Vec<(usize, char)> = s.char_indices().collect();

    for i in 0..chars.len() {
        let (byte_idx, c) = chars[i];
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' && in_quote == Some('"') {
            escaped = true;
            continue;
        }
        if let Some(q) = in_quote {
            if c == q {
                in_quote = None;
            }
        } else if c == '"' || c == '\'' {
            in_quote = Some(c);
        } else if c == '#' && (i == 0 || chars[i - 1].1.is_whitespace()) {
            break;
        } else if c == ':' {
            let is_eol_or_ws = if i + 1 < chars.len() {
                let next_char = chars[i + 1].1;
                next_char == ' ' || next_char == '\t'
            } else {
                true
            };
            if is_eol_or_ws {
                return Some(byte_idx);
            }
        }
    }
    None
}

fn split_yaml_comment(s: &str) -> (&str, Option<&str>) {
    let mut in_quote = None;
    let mut escaped = false;
    let chars: Vec<(usize, char)> = s.char_indices().collect();

    for i in 0..chars.len() {
        let (byte_idx, c) = chars[i];
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' && in_quote == Some('"') {
            escaped = true;
            continue;
        }
        if let Some(q) = in_quote {
            if c == q {
                in_quote = None;
            }
        } else if c == '"' || c == '\'' {
            in_quote = Some(c);
        } else if c == '#' {
            let is_valid_start = if i == 0 {
                true
            } else {
                chars[i - 1].1.is_whitespace()
            };
            if is_valid_start {
                return (&s[..byte_idx], Some(&s[byte_idx..]));
            }
        }
    }
    (s, None)
}

fn highlight_yaml_value(out: &mut String, val: &str) {
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
    } else if (trimmed.starts_with('[') && trimmed.ends_with(']'))
        || (trimmed.starts_with('{') && trimmed.ends_with('}'))
    {
        highlight_yaml_flow(out, trimmed);
    } else if matches!(trimmed, "|" | "|-" | "|+" | ">" | ">-" | ">+") {
        out.push_str(&format!(
            r##"<tspan fill="#fdba74" font-weight="bold">{}</tspan>"##,
            escape_xml(trimmed)
        ));
    } else if trimmed == "[" || trimmed == "]" || trimmed == "{" || trimmed == "}" {
        out.push_str(&format!(
            r##"<tspan fill="#cbd5e1">{}</tspan>"##,
            escape_xml(trimmed)
        ));
    } else if trimmed.starts_with('&') || trimmed.starts_with('*') {
        let (name, rest_val) = split_first_word(trimmed);
        out.push_str(&format!(
            r##"<tspan fill="#a78bfa" font-weight="bold">{}</tspan>"##,
            escape_xml(name)
        ));
        if !rest_val.is_empty() {
            out.push(' ');
            highlight_yaml_value(out, rest_val);
        }
    } else if trimmed.starts_with('!') {
        let (tag, rest_val) = split_first_word(trimmed);
        out.push_str(&format!(
            r##"<tspan fill="#c084fc">{}</tspan>"##,
            escape_xml(tag)
        ));
        if !rest_val.is_empty() {
            out.push(' ');
            highlight_yaml_value(out, rest_val);
        }
    } else {
        match trimmed.to_ascii_lowercase().as_str() {
            "true" | "false" | "yes" | "no" | "on" | "off" | "null" | "~" => {
                out.push_str(&format!(
                    r##"<tspan fill="#f472b6">{}</tspan>"##,
                    escape_xml(trimmed)
                ));
            }
            _ if is_yaml_number(trimmed) => {
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

fn highlight_yaml_flow(out: &mut String, text: &str) {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '[' || c == ']' || c == '{' || c == '}' {
            out.push_str(&format!(
                r##"<tspan fill="#cbd5e1">{}</tspan>"##,
                escape_xml(&c.to_string())
            ));
            i += 1;
        } else if c == ',' || c == ':' {
            out.push_str(&format!(
                r##"<tspan fill="#71717a">{}</tspan>"##,
                escape_xml(&c.to_string())
            ));
            i += 1;
        } else if c.is_whitespace() {
            out.push(c);
            i += 1;
        } else if c == '"' || c == '\'' {
            let quote = c;
            let mut s = String::new();
            s.push(quote);
            i += 1;
            let mut escaped = false;
            while i < chars.len() {
                let ch = chars[i];
                s.push(ch);
                i += 1;
                if escaped {
                    escaped = false;
                } else if ch == '\\' && quote == '"' {
                    escaped = true;
                } else if ch == quote {
                    break;
                }
            }
            out.push_str(&format!(
                r##"<tspan fill="#86efac">{}</tspan>"##,
                escape_xml(&s)
            ));
        } else {
            let mut token = String::new();
            while i < chars.len()
                && !chars[i].is_whitespace()
                && !matches!(chars[i], '[' | ']' | '{' | '}' | ',' | ':')
            {
                token.push(chars[i]);
                i += 1;
            }
            let is_key = i < chars.len() && chars[i] == ':';
            if is_key {
                out.push_str(&format!(
                    r##"<tspan fill="#93c5fd">{}</tspan>"##,
                    escape_xml(&token)
                ));
            } else {
                match token.to_ascii_lowercase().as_str() {
                    "true" | "false" | "yes" | "no" | "on" | "off" | "null" | "~" => {
                        out.push_str(&format!(
                            r##"<tspan fill="#f472b6">{}</tspan>"##,
                            escape_xml(&token)
                        ));
                    }
                    _ if is_yaml_number(&token) => {
                        out.push_str(&format!(
                            r##"<tspan fill="#fdba74">{}</tspan>"##,
                            escape_xml(&token)
                        ));
                    }
                    _ if token.starts_with('&') || token.starts_with('*') => {
                        out.push_str(&format!(
                            r##"<tspan fill="#a78bfa" font-weight="bold">{}</tspan>"##,
                            escape_xml(&token)
                        ));
                    }
                    _ => {
                        out.push_str(&escape_xml(&token));
                    }
                }
            }
        }
    }
}

fn is_yaml_number(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return !hex.is_empty()
            && !hex.starts_with('_')
            && !hex.ends_with('_')
            && !hex.contains("__")
            && hex.chars().all(|c| c.is_ascii_hexdigit() || c == '_');
    }
    if let Some(oct) = s.strip_prefix("0o").or_else(|| s.strip_prefix("0O")) {
        return !oct.is_empty()
            && !oct.starts_with('_')
            && !oct.ends_with('_')
            && !oct.contains("__")
            && oct.chars().all(|c| ('0'..='7').contains(&c) || c == '_');
    }
    if let Some(bin) = s.strip_prefix("0b").or_else(|| s.strip_prefix("0B")) {
        return !bin.is_empty()
            && !bin.starts_with('_')
            && !bin.ends_with('_')
            && !bin.contains("__")
            && bin.chars().all(|c| c == '0' || c == '1' || c == '_');
    }

    let lower = s.to_ascii_lowercase();
    if matches!(lower.as_str(), ".inf" | "+.inf" | "-.inf" | ".nan") {
        return true;
    }

    let s = s
        .strip_prefix('+')
        .or_else(|| s.strip_prefix('-'))
        .unwrap_or(s);
    if s.is_empty() {
        return false;
    }

    let mut has_digit = false;
    let mut has_dot = false;
    let mut has_exp = false;
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        if c.is_ascii_digit() {
            has_digit = true;
            i += 1;
        } else if c == '.' && !has_dot && !has_exp {
            has_dot = true;
            i += 1;
        } else if (c == 'e' || c == 'E') && has_digit && !has_exp {
            has_exp = true;
            i += 1;
            if i < chars.len() && (chars[i] == '+' || chars[i] == '-') {
                i += 1;
            }
            if i >= chars.len() || !chars[i].is_ascii_digit() {
                return false;
            }
        } else if c == '_' {
            if i > 0
                && chars[i - 1].is_ascii_digit()
                && i + 1 < chars.len()
                && chars[i + 1].is_ascii_digit()
            {
                i += 1;
            } else {
                return false;
            }
        } else {
            return false;
        }
    }

    has_digit
}

fn split_first_word(s: &str) -> (&str, &str) {
    if let Some(idx) = s.find(|c: char| c.is_whitespace()) {
        (&s[..idx], s[idx..].trim_start())
    } else {
        (s, "")
    }
}

fn parse_code_fence(s: &str) -> Option<(char, usize, &str)> {
    let trimmed = s.trim_start();
    let fence_char = trimmed.chars().next()?;
    if fence_char == '`' || fence_char == '~' {
        let count = trimmed.chars().take_while(|&c| c == fence_char).count();
        if count >= 3 {
            return Some((fence_char, count, trimmed[count..].trim()));
        }
    }
    None
}

fn parse_heading_level(s: &str) -> Option<u8> {
    let count = s.chars().take_while(|&c| c == '#').count();
    if count > 0 && count <= 6 {
        let next = s.chars().nth(count);
        if next == Some(' ') || next == Some('\t') || next.is_none() {
            return Some(count as u8);
        }
    }
    None
}

fn is_horizontal_rule(s: &str) -> bool {
    let non_ws: Vec<char> = s.chars().filter(|c| !c.is_whitespace()).collect();
    if non_ws.len() < 3 {
        return false;
    }
    let first = non_ws[0];
    (first == '-' || first == '*' || first == '_') && non_ws.iter().all(|&c| c == first)
}

fn is_table_separator(s: &str) -> bool {
    let trimmed = s.trim();
    if !trimmed.starts_with('|') || !trimmed.ends_with('|') {
        return false;
    }
    trimmed
        .chars()
        .all(|c| c == '|' || c == '-' || c == ':' || c.is_whitespace())
}

fn is_markdown_list_item(trimmed: &str) -> bool {
    trimmed.starts_with("- ")
        || trimmed.starts_with("* ")
        || trimmed.starts_with("+ ")
        || (trimmed
            .find(". ")
            .is_some_and(|pos| pos > 0 && trimmed[..pos].chars().all(|c| c.is_ascii_digit())))
}

struct MarkdownLinkInfo {
    is_image: bool,
    text_start: usize,
    text_end: usize,
    url_start: usize,
    url_end: usize,
    next_index: usize,
}

fn parse_markdown_link(chars: &[char], i: usize) -> Option<MarkdownLinkInfo> {
    let (is_image, open_bracket) = if chars.get(i) == Some(&'!') && chars.get(i + 1) == Some(&'[') {
        if i > 0 && chars[i - 1] == '\\' {
            return None;
        }
        (true, i + 1)
    } else if chars.get(i) == Some(&'[') {
        if i > 0 && chars[i - 1] == '\\' {
            return None;
        }
        (false, i)
    } else {
        return None;
    };

    let text_start = open_bracket + 1;
    let mut depth = 1;
    let mut bracket_end = None;
    let mut j = text_start;
    while j < chars.len() {
        if chars[j] == '\\' {
            j += 2;
            continue;
        }
        if chars[j] == '[' {
            depth += 1;
        } else if chars[j] == ']' {
            depth -= 1;
            if depth == 0 {
                bracket_end = Some(j);
                break;
            }
        }
        j += 1;
    }

    let bracket_end = bracket_end?;
    if chars.get(bracket_end + 1) != Some(&'(') {
        return None;
    }

    let url_start = bracket_end + 2;
    let mut paren_depth = 1;
    let mut paren_end = None;
    let mut k = url_start;
    while k < chars.len() {
        if chars[k] == '\\' {
            k += 2;
            continue;
        }
        if chars[k] == '(' {
            paren_depth += 1;
        } else if chars[k] == ')' {
            paren_depth -= 1;
            if paren_depth == 0 {
                paren_end = Some(k);
                break;
            }
        }
        k += 1;
    }

    let paren_end = paren_end?;
    Some(MarkdownLinkInfo {
        is_image,
        text_start,
        text_end: bracket_end,
        url_start,
        url_end: paren_end,
        next_index: paren_end + 1,
    })
}

fn strip_markdown_urls_for_reader(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if let Some(link) = parse_markdown_link(&chars, i) {
            let text_content: String = chars[link.text_start..link.text_end].iter().collect();
            if link.is_image {
                out.push_str("![");
                out.push_str(&text_content);
                out.push_str("]()");
            } else {
                out.push('[');
                out.push_str(&text_content);
                out.push_str("]()");
            }
            i = link.next_index;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

fn find_matching_backtick_run(chars: &[char], start: usize, run_len: usize) -> Option<usize> {
    let mut k = start;
    while k < chars.len() {
        if chars[k] == '`' {
            let mut close_len = 1;
            while k + close_len < chars.len() && chars[k + close_len] == '`' {
                close_len += 1;
            }
            if close_len == run_len {
                return Some(k);
            }
            k += close_len;
        } else {
            k += 1;
        }
    }
    None
}

fn split_markdown_row(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let s = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let s = if let Some(prefix) = s.strip_suffix('|') {
        let num_backslashes = prefix.chars().rev().take_while(|&c| c == '\\').count();
        if num_backslashes % 2 == 0 {
            prefix
        } else {
            s
        }
    } else {
        s
    };
    let mut cells = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        if c == '`' {
            let mut run_len = 1;
            while i + run_len < chars.len() && chars[i + run_len] == '`' {
                run_len += 1;
            }
            if let Some(end) = find_matching_backtick_run(&chars, i + run_len, run_len) {
                for ch in &chars[i..end + run_len] {
                    current.push(*ch);
                }
                i = end + run_len;
            } else {
                for _ in 0..run_len {
                    current.push('`');
                }
                i += run_len;
            }
        } else if c == '\\' {
            let mut bs_len = 1;
            while i + bs_len < chars.len() && chars[i + bs_len] == '\\' {
                bs_len += 1;
            }
            if bs_len % 2 == 1 && chars.get(i + bs_len) == Some(&'|') {
                for _ in 0..(bs_len - 1) {
                    current.push('\\');
                }
                current.push('|');
                i += bs_len + 1;
            } else {
                for _ in 0..bs_len {
                    current.push('\\');
                }
                i += bs_len;
            }
        } else if c == '|' {
            cells.push(current.trim().to_string());
            current = String::new();
            i += 1;
        } else {
            current.push(c);
            i += 1;
        }
    }
    cells.push(current.trim().to_string());
    cells
}

fn markdown_visual_text(cell: &str) -> String {
    let chars: Vec<char> = cell.chars().collect();
    let mut out = String::with_capacity(chars.len());
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];

        // 1. Inline code: `code`
        if c == '`' {
            if let Some(close_idx) = chars[i + 1..].iter().position(|&ch| ch == '`') {
                let end = i + 1 + close_idx;
                let code_content: String = chars[i + 1..end].iter().collect();
                out.push_str(&code_content);
                i = end + 1;
                continue;
            }
        }

        // 2. Images & Links: ![alt](url) or [text](url)
        if let Some(link) = parse_markdown_link(&chars, i) {
            let text_content: String = chars[link.text_start..link.text_end].iter().collect();
            if link.is_image {
                out.push_str("🖼 ");
            }
            out.push_str(&text_content);
            i = link.next_index;
            continue;
        }

        // 3. Bold + Italic: ***text*** or ___text___
        if (i + 2 < chars.len() && chars[i] == '*' && chars[i + 1] == '*' && chars[i + 2] == '*')
            || (i + 2 < chars.len()
                && chars[i] == '_'
                && chars[i + 1] == '_'
                && chars[i + 2] == '_')
        {
            let delim = chars[i];
            let mut close_idx = None;
            for j in (i + 3)..chars.len().saturating_sub(2) {
                if chars[j] == delim && chars[j + 1] == delim && chars[j + 2] == delim {
                    close_idx = Some(j);
                    break;
                }
            }
            if let Some(end) = close_idx {
                let content: String = chars[i + 3..end].iter().collect();
                out.push_str(&content);
                i = end + 3;
                continue;
            }
        }

        // 4. Bold: **text** or __text__
        if (i + 1 < chars.len() && chars[i] == '*' && chars[i + 1] == '*')
            || (i + 1 < chars.len() && chars[i] == '_' && chars[i + 1] == '_')
        {
            let delim = chars[i];
            let mut close_idx = None;
            for j in (i + 2)..chars.len().saturating_sub(1) {
                if chars[j] == delim && chars[j + 1] == delim {
                    close_idx = Some(j);
                    break;
                }
            }
            if let Some(end) = close_idx {
                let content: String = chars[i + 2..end].iter().collect();
                out.push_str(&content);
                i = end + 2;
                continue;
            }
        }

        // 5. Strikethrough: ~~text~~
        if i + 1 < chars.len() && chars[i] == '~' && chars[i + 1] == '~' {
            let mut close_idx = None;
            for j in (i + 2)..chars.len().saturating_sub(1) {
                if chars[j] == '~' && chars[j + 1] == '~' {
                    close_idx = Some(j);
                    break;
                }
            }
            if let Some(end) = close_idx {
                let content: String = chars[i + 2..end].iter().collect();
                out.push_str(&content);
                i = end + 2;
                continue;
            }
        }

        // 6. Italic: *text* (when not followed by space and closing not preceded by space)
        if c == '*' && i + 1 < chars.len() && chars[i + 1] != ' ' {
            let mut close_idx = None;
            for j in (i + 1)..chars.len() {
                if chars[j] == '*' && chars[j - 1] != ' ' {
                    close_idx = Some(j);
                    break;
                }
            }
            if let Some(end) = close_idx {
                let content: String = chars[i + 1..end].iter().collect();
                out.push_str(&content);
                i = end + 1;
                continue;
            }
        }

        // 7. HTML tag: <tag ...>
        if c == '<'
            && i + 1 < chars.len()
            && (chars[i + 1].is_ascii_alphabetic() || chars[i + 1] == '/')
        {
            if let Some(close_idx) = chars[i + 1..].iter().position(|&ch| ch == '>') {
                let end = i + 1 + close_idx;
                let tag_str: String = chars[i..=end].iter().collect();
                out.push_str(&tag_str);
                i = end + 1;
                continue;
            }
        }

        out.push(c);
        i += 1;
    }

    out
}

fn char_display_width(c: char) -> usize {
    let u = c as u32;
    if u < 0x20 || (0x7F..=0x9F).contains(&u) {
        0
    } else if (0x1100..=0x115F).contains(&u)
        || (0x2329..=0x232A).contains(&u)
        || ((0x2E80..=0xA4CF).contains(&u) && u != 0x303F)
        || (0xAC00..=0xD7A3).contains(&u)
        || (0xF900..=0xFAFF).contains(&u)
        || (0xFE10..=0xFE19).contains(&u)
        || (0xFE30..=0xFE6F).contains(&u)
        || (0xFF01..=0xFF60).contains(&u)
        || (0xFFE0..=0xFFE6).contains(&u)
        || (0x1F000..=0x1FAFF).contains(&u)
        || (0x20000..=0x3FFFF).contains(&u)
    {
        2
    } else {
        1
    }
}

fn str_display_width(s: &str) -> usize {
    s.chars().map(char_display_width).sum()
}

fn take_display_width(s: &str, max_width: usize) -> String {
    let mut current_width = 0;
    let mut out = String::new();
    for c in s.chars() {
        let w = char_display_width(c);
        if current_width + w > max_width {
            break;
        }
        current_width += w;
        out.push(c);
    }
    out
}

fn truncate_balanced_link_text(text: &str, total_label_budget: usize) -> String {
    if total_label_budget <= 1 {
        return "…".to_string();
    }
    let content_budget = total_label_budget - 1;
    let candidate = take_display_width(text, content_budget);
    let mut unclosed = 0;
    let mut chars = candidate.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            chars.next();
        } else if c == '[' {
            unclosed += 1;
        } else if c == ']' && unclosed > 0 {
            unclosed -= 1;
        }
    }
    let take_budget = content_budget.saturating_sub(unclosed);
    let mut result = take_display_width(text, take_budget);
    let mut final_unclosed = 0;
    let mut res_chars = result.chars().peekable();
    while let Some(c) = res_chars.next() {
        if c == '\\' {
            res_chars.next();
        } else if c == '[' {
            final_unclosed += 1;
        } else if c == ']' && final_unclosed > 0 {
            final_unclosed -= 1;
        }
    }
    result.push('…');
    for _ in 0..final_unclosed {
        result.push(']');
    }
    result
}

fn truncate_markdown_cell(raw: &str, width: usize) -> String {
    if width <= 1 {
        return "…".to_string();
    }
    let mut budget = width - 1;
    let chars: Vec<char> = raw.chars().collect();
    let mut out = String::with_capacity(chars.len());
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];

        // 1. Inline code: `code`
        if c == '`' {
            if let Some(close_idx) = chars[i + 1..].iter().position(|&ch| ch == '`') {
                let end = i + 1 + close_idx;
                let code_content: String = chars[i + 1..end].iter().collect();
                let vlen = str_display_width(&code_content);
                if vlen <= budget {
                    out.push('`');
                    out.push_str(&code_content);
                    out.push('`');
                    budget -= vlen;
                    i = end + 1;
                    continue;
                } else {
                    let trunc = take_display_width(&code_content, budget);
                    out.push('`');
                    out.push_str(&trunc);
                    out.push('…');
                    out.push('`');
                    return out;
                }
            }
        }

        // 2. Images & Links: ![alt](url) or [text](url)
        if let Some(link) = parse_markdown_link(&chars, i) {
            let text_content: String = chars[link.text_start..link.text_end].iter().collect();
            let remaining_w = budget + 1;
            if link.is_image {
                // "🖼 " prefix has display width 3
                let vlen = 3 + str_display_width(&text_content);
                if vlen <= budget {
                    out.push_str(&chars[i..link.next_index].iter().collect::<String>());
                    budget -= vlen;
                    i = link.next_index;
                    continue;
                } else if remaining_w <= 3 {
                    out.push('…');
                    return out;
                } else {
                    let img_label_budget = remaining_w - 3;
                    let trunc = truncate_balanced_link_text(&text_content, img_label_budget);
                    out.push_str(&format!("![{trunc}]()"));
                    return out;
                }
            } else {
                let vlen = str_display_width(&text_content);
                if vlen <= budget {
                    out.push_str(&chars[i..link.next_index].iter().collect::<String>());
                    budget -= vlen;
                    i = link.next_index;
                    continue;
                } else {
                    let trunc = truncate_balanced_link_text(&text_content, remaining_w);
                    out.push_str(&format!("[{trunc}]()"));
                    return out;
                }
            }
        }

        // 3. Bold + Italic: ***text*** or ___text___
        if (i + 2 < chars.len() && chars[i] == '*' && chars[i + 1] == '*' && chars[i + 2] == '*')
            || (i + 2 < chars.len()
                && chars[i] == '_'
                && chars[i + 1] == '_'
                && chars[i + 2] == '_')
        {
            let delim = chars[i];
            let mut close_idx = None;
            for j in (i + 3)..chars.len().saturating_sub(2) {
                if chars[j] == delim && chars[j + 1] == delim && chars[j + 2] == delim {
                    close_idx = Some(j);
                    break;
                }
            }
            if let Some(end) = close_idx {
                let content: String = chars[i + 3..end].iter().collect();
                let vlen = str_display_width(&content);
                let delim_str: String = std::iter::repeat_n(delim, 3).collect();
                if vlen <= budget {
                    out.push_str(&delim_str);
                    out.push_str(&content);
                    out.push_str(&delim_str);
                    budget -= vlen;
                    i = end + 3;
                    continue;
                } else {
                    let trunc = take_display_width(&content, budget);
                    out.push_str(&format!("{delim_str}{trunc}…{delim_str}"));
                    return out;
                }
            }
        }

        // 4. Bold: **text** or __text__
        if (i + 1 < chars.len() && chars[i] == '*' && chars[i + 1] == '*')
            || (i + 1 < chars.len() && chars[i] == '_' && chars[i + 1] == '_')
        {
            let delim = chars[i];
            let mut close_idx = None;
            for j in (i + 2)..chars.len().saturating_sub(1) {
                if chars[j] == delim && chars[j + 1] == delim {
                    close_idx = Some(j);
                    break;
                }
            }
            if let Some(end) = close_idx {
                let content: String = chars[i + 2..end].iter().collect();
                let vlen = str_display_width(&content);
                let delim_str: String = std::iter::repeat_n(delim, 2).collect();
                if vlen <= budget {
                    out.push_str(&delim_str);
                    out.push_str(&content);
                    out.push_str(&delim_str);
                    budget -= vlen;
                    i = end + 2;
                    continue;
                } else {
                    let trunc = take_display_width(&content, budget);
                    out.push_str(&format!("{delim_str}{trunc}…{delim_str}"));
                    return out;
                }
            }
        }

        // 5. Strikethrough: ~~text~~
        if i + 1 < chars.len() && chars[i] == '~' && chars[i + 1] == '~' {
            let mut close_idx = None;
            for j in (i + 2)..chars.len().saturating_sub(1) {
                if chars[j] == '~' && chars[j + 1] == '~' {
                    close_idx = Some(j);
                    break;
                }
            }
            if let Some(end) = close_idx {
                let content: String = chars[i + 2..end].iter().collect();
                let vlen = str_display_width(&content);
                if vlen <= budget {
                    out.push_str("~~");
                    out.push_str(&content);
                    out.push_str("~~");
                    budget -= vlen;
                    i = end + 2;
                    continue;
                } else {
                    let trunc = take_display_width(&content, budget);
                    out.push_str(&format!("~~{trunc}…~~"));
                    return out;
                }
            }
        }

        // 6. Italic: *text* (when not followed by space and closing not preceded by space)
        if c == '*' && i + 1 < chars.len() && chars[i + 1] != ' ' {
            let mut close_idx = None;
            for j in (i + 1)..chars.len() {
                if chars[j] == '*' && chars[j - 1] != ' ' {
                    close_idx = Some(j);
                    break;
                }
            }
            if let Some(end) = close_idx {
                let content: String = chars[i + 1..end].iter().collect();
                let vlen = str_display_width(&content);
                if vlen <= budget {
                    out.push('*');
                    out.push_str(&content);
                    out.push('*');
                    budget -= vlen;
                    i = end + 1;
                    continue;
                } else {
                    let trunc = take_display_width(&content, budget);
                    out.push_str(&format!("*{trunc}…*"));
                    return out;
                }
            }
        }

        // 7. Plain character
        let cw = char_display_width(c);
        if cw <= budget {
            out.push(c);
            budget -= cw;
            i += 1;
        } else {
            out.push('…');
            return out;
        }
    }

    out.push('…');
    out
}

#[derive(Debug, Clone)]
enum AlignedTableEntry {
    Separator(String),
    Row(String, Vec<String>),
}

fn build_aligned_markdown_tables<S: AsRef<str>>(
    raw_lines: &[S],
    layout: PageLayout,
) -> std::collections::HashMap<usize, AlignedTableEntry> {
    let mut aligned_lines = std::collections::HashMap::new();
    let mut i = 0;
    let mut in_code_block: Option<(char, usize)> = None;
    let mut in_frontmatter = false;
    let mut past_frontmatter = false;

    let max_width: usize = match layout {
        PageLayout::Portrait => 114,
        PageLayout::Landscape => 154,
    };
    let max_cols = (max_width + 3) / 6;

    while i < raw_lines.len() {
        let trimmed = raw_lines[i].as_ref().trim();

        if !past_frontmatter {
            if i == 0 && trimmed == "---" {
                in_frontmatter = true;
                i += 1;
                continue;
            } else if in_frontmatter {
                if trimmed == "---" || trimmed == "..." {
                    in_frontmatter = false;
                    past_frontmatter = true;
                }
                i += 1;
                continue;
            }
        }

        if let Some((fence_char, fence_count)) = in_code_block {
            if let Some((c, count, info)) = parse_code_fence(trimmed) {
                if c == fence_char && count >= fence_count && info.is_empty() {
                    in_code_block = None;
                }
            }
            i += 1;
            continue;
        } else if let Some((c, count, _)) = parse_code_fence(trimmed) {
            in_code_block = Some((c, count));
            i += 1;
            continue;
        }

        let is_row = parse_heading_level(trimmed).is_none()
            && !trimmed.starts_with('>')
            && !is_markdown_list_item(trimmed)
            && (trimmed.starts_with('|') || (trimmed.ends_with('|') && trimmed.contains('|')));
        if is_row && i + 1 < raw_lines.len() && is_table_separator(raw_lines[i + 1].as_ref().trim())
        {
            let table_start = i;
            let sep_idx = i + 1;
            let mut table_end = sep_idx;
            while table_end + 1 < raw_lines.len() {
                let next_trimmed = raw_lines[table_end + 1].as_ref().trim();
                let next_is_row = parse_heading_level(next_trimmed).is_none()
                    && !next_trimmed.starts_with('>')
                    && !is_markdown_list_item(next_trimmed)
                    && (next_trimmed.starts_with('|')
                        || (next_trimmed.ends_with('|') && next_trimmed.contains('|')));
                if next_is_row && !is_table_separator(next_trimmed) {
                    table_end += 1;
                } else {
                    break;
                }
            }

            let header_cells = split_markdown_row(raw_lines[table_start].as_ref());
            let sep_cells = split_markdown_row(raw_lines[sep_idx].as_ref());
            let mut data_rows: Vec<Vec<String>> = Vec::new();
            for line in &raw_lines[sep_idx + 1..=table_end] {
                data_rows.push(split_markdown_row(line.as_ref()));
            }

            let total_cols = header_cells
                .len()
                .max(sep_cells.len())
                .max(data_rows.iter().map(|r| r.len()).max().unwrap_or(0));

            let (num_cols, has_overflow) = if total_cols > max_cols && max_cols > 1 {
                (max_cols - 1, true)
            } else {
                (total_cols.min(max_cols), false)
            };
            let omitted_cols = if has_overflow {
                total_cols - num_cols
            } else {
                0
            };
            let total_display_cols = if has_overflow { num_cols + 1 } else { num_cols };

            if total_display_cols > 0 {
                let mut align_right = vec![false; total_display_cols];
                for (c, align) in align_right.iter_mut().take(num_cols).enumerate() {
                    if let Some(sep_cell) = sep_cells.get(c) {
                        let t = sep_cell.trim();
                        if t.ends_with(':') && !t.starts_with(':') {
                            *align = true;
                            continue;
                        }
                        if t.starts_with(':') {
                            *align = false;
                            continue;
                        }
                    }
                    let mut non_empty = 0usize;
                    let mut numeric = 0usize;
                    for r in &data_rows {
                        if let Some(cell) = r.get(c) {
                            let t = cell.trim();
                            if !t.is_empty() {
                                non_empty += 1;
                                if is_numeric_field(t) {
                                    numeric += 1;
                                }
                            }
                        }
                    }
                    if non_empty > 0 && numeric * 10 >= non_empty * 7 {
                        *align = true;
                    }
                }

                let divider_chars = total_display_cols.saturating_sub(1) * 3;
                let available_space = max_width.saturating_sub(divider_chars);

                let min_col_w = if available_space >= total_display_cols * 3 {
                    3
                } else {
                    1
                };
                let mut col_widths = vec![min_col_w; total_display_cols];
                for (c, cell) in header_cells.iter().enumerate().take(num_cols) {
                    let vlen = str_display_width(&markdown_visual_text(cell));
                    col_widths[c] = col_widths[c].max(vlen);
                }
                for r in &data_rows {
                    for (c, cell) in r.iter().enumerate().take(num_cols) {
                        let vlen = str_display_width(&markdown_visual_text(cell));
                        col_widths[c] = col_widths[c].max(vlen);
                    }
                }
                let min_overflow_w = if has_overflow {
                    let overflow_label = format!("+{omitted_cols} more");
                    let len = overflow_label.chars().count();
                    col_widths[num_cols] = col_widths[num_cols].max(len);
                    len
                } else {
                    0
                };

                let mut total_w: usize = col_widths.iter().sum();
                if total_w > available_space && available_space > 0 {
                    while total_w > available_space {
                        let mut max_w = 0;
                        for (c, &w) in col_widths.iter().enumerate() {
                            let min_w = if has_overflow && c == num_cols {
                                min_overflow_w
                            } else {
                                1
                            };
                            if w > min_w && w > max_w {
                                max_w = w;
                            }
                        }
                        if max_w == 0 {
                            break;
                        }
                        for (c, w) in col_widths.iter_mut().enumerate() {
                            let min_w = if has_overflow && c == num_cols {
                                min_overflow_w
                            } else {
                                1
                            };
                            if *w == max_w && *w > min_w && total_w > available_space {
                                *w -= 1;
                                total_w -= 1;
                            }
                        }
                    }
                }

                let format_cells = |cells: &[String], is_header: bool| -> (String, Vec<String>) {
                    let mut formatted = Vec::with_capacity(total_display_cols);
                    for c in 0..num_cols {
                        let width = col_widths[c];
                        let raw = cells.get(c).map(|s| s.as_str()).unwrap_or("");
                        let visual = markdown_visual_text(raw);
                        let vlen = str_display_width(&visual);
                        let cell_str = if vlen > width {
                            let truncated = truncate_markdown_cell(raw, width);
                            let t_vlen = str_display_width(&markdown_visual_text(&truncated));
                            let pad = width.saturating_sub(t_vlen);
                            if align_right[c] {
                                format!("{}{truncated}", " ".repeat(pad))
                            } else {
                                format!("{truncated}{}", " ".repeat(pad))
                            }
                        } else {
                            let padding = width.saturating_sub(vlen);
                            if align_right[c] {
                                format!("{}{raw}", " ".repeat(padding))
                            } else {
                                format!("{raw}{}", " ".repeat(padding))
                            }
                        };
                        formatted.push(cell_str);
                    }
                    if has_overflow {
                        let width = col_widths[num_cols];
                        let label = if is_header {
                            format!("+{omitted_cols} more")
                        } else {
                            "…".to_string()
                        };
                        let cell_chars: Vec<char> = label.chars().collect();
                        let truncated = if cell_chars.len() > width {
                            if width > 1 {
                                let keep = width - 1;
                                let mut s: String = cell_chars[..keep].iter().collect();
                                s.push('…');
                                s
                            } else {
                                "…".to_string()
                            }
                        } else {
                            label
                        };
                        formatted.push(format!("{truncated:<width$}"));
                    }
                    let joined = formatted.join(" | ");
                    (joined, formatted)
                };

                let (hdr_text, hdr_cells) = format_cells(&header_cells, true);
                aligned_lines.insert(table_start, AlignedTableEntry::Row(hdr_text, hdr_cells));
                let sep_parts: Vec<String> = col_widths.iter().map(|&w| "─".repeat(w)).collect();
                aligned_lines.insert(sep_idx, AlignedTableEntry::Separator(sep_parts.join("─┼─")));
                for (row_offset, d_row) in data_rows.iter().enumerate() {
                    let line_idx = sep_idx + 1 + row_offset;
                    let (row_text, row_cells) = format_cells(d_row, false);
                    aligned_lines.insert(line_idx, AlignedTableEntry::Row(row_text, row_cells));
                }
            }

            i = table_end + 1;
        } else {
            i += 1;
        }
    }
    aligned_lines
}

#[allow(clippy::too_many_arguments)]
fn highlight_md_row(
    text: &str,
    indent_cols: usize,
    starts_in_code_block: bool,
    heading_level: Option<u8>,
    is_blockquote_continuation: bool,
    is_frontmatter: bool,
    is_table_row: bool,
    is_table_header: bool,
    is_table_separator: bool,
    table_cells: Option<&[String]>,
    table_col_start: usize,
    is_chunk_0: bool,
    is_last_chunk: bool,
    render_mode: u8,
    layout: PageLayout,
) -> String {
    if render_mode == 1 {
        highlight_md_reader_row(
            text,
            indent_cols,
            starts_in_code_block,
            heading_level,
            is_blockquote_continuation,
            is_frontmatter,
            is_table_row,
            is_table_header,
            is_table_separator,
            table_cells,
            layout,
        )
    } else {
        highlight_md_source_row(
            text,
            indent_cols,
            starts_in_code_block,
            heading_level,
            is_blockquote_continuation,
            is_table_row,
            is_table_separator,
            table_col_start,
            is_chunk_0,
            is_last_chunk,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn highlight_md_reader_row(
    text: &str,
    indent_cols: usize,
    starts_in_code_block: bool,
    heading_level: Option<u8>,
    is_blockquote_continuation: bool,
    is_frontmatter: bool,
    is_table_row: bool,
    is_table_header: bool,
    is_table_separator: bool,
    table_cells: Option<&[String]>,
    layout: PageLayout,
) -> String {
    let mut out = String::with_capacity(text.len() * 2 + indent_cols * 15);

    // Frontmatter block (YAML metadata header in Markdown)
    if is_frontmatter {
        let trimmed = text.trim();
        if trimmed == "---" || trimmed == "..." {
            out.push_str(r##"<tspan fill="#52525b">─── metadata ───</tspan>"##);
        } else {
            out.push_str(&format!(
                r##"<tspan fill="#71717a" font-style="italic">{}</tspan>"##,
                escape_xml(text)
            ));
        }
        return out;
    }

    // Content inside a fenced code block
    if starts_in_code_block {
        out.push_str(&format!(
            r##"<tspan fill="#86efac" font-family="ui-monospace, Menlo, Monaco, Consolas, monospace" font-size="16">{}</tspan>"##,
            escape_xml(text)
        ));
        return out;
    }

    // Table rows (aligned or raw) and separators bypass indentation, headings, and block formatting
    if is_table_row || is_table_separator {
        if is_table_separator {
            if text.contains('┼') || text.trim().starts_with('─') {
                out.push_str(&format!(
                    r##"<tspan fill="#3f3f46">{}</tspan>"##,
                    escape_xml(text)
                ));
            } else {
                let rule_len = if layout == PageLayout::Landscape {
                    108
                } else {
                    76
                };
                let rule = "─".repeat(rule_len);
                out.push_str(&format!(r##"<tspan fill="#3f3f46">{rule}</tspan>"##));
            }
            return out;
        }
        highlight_md_reader_table_row(&mut out, text, is_table_header, table_cells);
        return out;
    }

    // Fenced code block delimiters (opening or closing fence)
    let trimmed = text.trim();
    if let Some((_ch, _len, info)) = parse_code_fence(trimmed) {
        if !info.is_empty() {
            out.push_str(&format!(
                r##"<tspan fill="#52525b">╭─ </tspan><tspan fill="#7dd3fc" font-family="ui-monospace, Menlo, Monaco, Consolas, monospace" font-size="15">{}</tspan>"##,
                escape_xml(info)
            ));
        } else {
            let rule_len = if layout == PageLayout::Landscape {
                90
            } else {
                65
            };
            let rule = "─".repeat(rule_len);
            out.push_str(&format!(r##"<tspan fill="#52525b">╰{rule}</tspan>"##));
        }
        return out;
    }

    // Heading (chunk 0 or continuation)
    if let Some(level) = heading_level {
        let (color, weight, size) = match level {
            1 => ("#fbbf24", "bold", "24"), // H1: Amber Gold, 24px
            2 => ("#38bdf8", "bold", "21"), // H2: Sky Blue, 21px
            3 => ("#34d399", "bold", "19"), // H3: Emerald, 19px
            4 => ("#a78bfa", "bold", "18"), // H4: Purple, 18px
            5 => ("#f472b6", "bold", "17"), // H5: Pink, 17px
            _ => ("#94a3b8", "bold", "16"), // H6: Slate, 16px
        };
        let stripped = text.trim_start();
        let prefix_len = if stripped.starts_with('#') {
            stripped.chars().take_while(|&c| c == '#').count()
        } else {
            0
        };
        let content = if prefix_len > 0 {
            stripped[prefix_len..].trim_start()
        } else {
            stripped
        };
        out.push_str(&format!(
            r##"<tspan fill="{color}" font-weight="{weight}" font-size="{size}">"##
        ));
        highlight_md_reader_inline(&mut out, content, Some(color));
        out.push_str("</tspan>");
        return out;
    }

    // Blockquote
    if is_blockquote_continuation || trimmed.starts_with('>') {
        out.push_str(r##"<tspan fill="#38bdf8" font-weight="bold">▎ </tspan>"##);
        let content = if !is_blockquote_continuation && trimmed.starts_with('>') {
            let mut s = trimmed;
            while let Some(after) = s.strip_prefix('>') {
                s = after.trim_start();
            }
            s
        } else {
            trimmed
        };
        out.push_str(r##"<tspan fill="#cbd5e1" font-style="italic">"##);
        highlight_md_reader_inline(&mut out, content, None);
        out.push_str("</tspan>");
        return out;
    }

    let leading_spaces = text.chars().take_while(|&c| c == ' ').count();
    let total_indent = if indent_cols > 0 {
        indent_cols
    } else {
        leading_spaces
    };
    let num_levels = total_indent / 2;
    let remainder_spaces = total_indent % 2;

    if total_indent > 0 {
        for i in 0..num_levels {
            let color = RAINBOW_PALETTE[i % RAINBOW_PALETTE.len()];
            out.push_str(&format!(
                r##"<tspan fill="{color}" fill-opacity="0.45">│ </tspan>"##
            ));
        }
        for _ in 0..remainder_spaces {
            out.push(' ');
        }
    }

    let rest = if indent_cols > 0 {
        text
    } else {
        &text[leading_spaces..]
    };
    if rest.is_empty() {
        return out;
    }

    // HTML comment in Markdown
    if rest.starts_with("<!--") {
        out.push_str(&format!(
            r##"<tspan fill="#71717a">{}</tspan>"##,
            escape_xml(rest)
        ));
        return out;
    }

    // Horizontal rule
    if is_horizontal_rule(rest) {
        let rule_len = if layout == PageLayout::Landscape {
            108
        } else {
            76
        };
        let rule = "─".repeat(rule_len);
        out.push_str(&format!(r##"<tspan fill="#3f3f46">{rule}</tspan>"##));
        return out;
    }

    // Unordered bullet list / task list item
    let bullet_prefix = if rest.starts_with("- ") {
        Some("- ")
    } else if rest.starts_with("* ") {
        Some("* ")
    } else if rest.starts_with("+ ") {
        Some("+ ")
    } else {
        None
    };

    if let Some(prefix) = bullet_prefix {
        let after_bullet = &rest[prefix.len()..];
        if let Some(after_box) = after_bullet.strip_prefix("[ ] ") {
            out.push_str(r##"<tspan fill="#71717a">☐ </tspan>"##);
            highlight_md_reader_inline(&mut out, after_box, None);
        } else if let Some(after_box) = after_bullet
            .strip_prefix("[x] ")
            .or_else(|| after_bullet.strip_prefix("[X] "))
        {
            out.push_str(r##"<tspan fill="#34d399" font-weight="bold">☑ </tspan>"##);
            highlight_md_reader_inline(&mut out, after_box, None);
        } else {
            out.push_str(r##"<tspan fill="#fdba74" font-weight="bold">• </tspan>"##);
            highlight_md_reader_inline(&mut out, after_bullet, None);
        }
        return out;
    }

    // Ordered list item (e.g. "1. ")
    if let Some(dot_idx) = rest.find(". ") {
        let num_part = &rest[..dot_idx];
        if !num_part.is_empty() && num_part.chars().all(|c| c.is_ascii_digit()) {
            let prefix = &rest[..dot_idx + 2];
            let after_num = &rest[dot_idx + 2..];
            out.push_str(&format!(
                r##"<tspan fill="#93c5fd" font-weight="bold">{}</tspan>"##,
                escape_xml(prefix)
            ));
            highlight_md_reader_inline(&mut out, after_num, None);
            return out;
        }
    }

    // Regular paragraph
    highlight_md_reader_inline(&mut out, rest, None);
    out
}

fn highlight_md_reader_table_row(
    out: &mut String,
    text: &str,
    is_header: bool,
    table_cells: Option<&[String]>,
) {
    if let Some(cells) = table_cells {
        for (col_idx, part) in cells.iter().enumerate() {
            if col_idx > 0 {
                out.push_str(r##"<tspan fill="#3f3f46"> | </tspan>"##);
            }
            let color = CSV_PALETTE[col_idx % CSV_PALETTE.len()];
            if is_header {
                out.push_str(&format!(r##"<tspan fill="{color}" font-weight="bold">"##));
            } else {
                out.push_str(&format!(r##"<tspan fill="{color}">"##));
            }
            highlight_md_reader_inline(out, part, Some(color));
            out.push_str("</tspan>");
        }
        return;
    }

    let delim = if text.contains(" | ") {
        " | "
    } else if text.contains(" │ ") {
        " │ "
    } else {
        ""
    };

    if !delim.is_empty() {
        let parts: Vec<&str> = text.split(delim).collect();
        for (col_idx, part) in parts.iter().enumerate() {
            if col_idx > 0 {
                out.push_str(r##"<tspan fill="#3f3f46"> | </tspan>"##);
            }
            let color = CSV_PALETTE[col_idx % CSV_PALETTE.len()];
            if is_header {
                out.push_str(&format!(r##"<tspan fill="{color}" font-weight="bold">"##));
            } else {
                out.push_str(&format!(r##"<tspan fill="{color}">"##));
            }
            highlight_md_reader_inline(out, part, Some(color));
            out.push_str("</tspan>");
        }
        return;
    }

    let raw_cells: Vec<&str> = text.split('|').collect();
    if raw_cells.len() < 2 {
        highlight_md_reader_inline(out, text, None);
        return;
    }

    let has_leading_pipe = text.trim_start().starts_with('|');
    let has_trailing_pipe = text.trim_end().ends_with('|');

    let cells_slice = match (has_leading_pipe, has_trailing_pipe) {
        (true, true) if raw_cells.len() >= 2 => &raw_cells[1..raw_cells.len() - 1],
        (true, false) if !raw_cells.is_empty() => &raw_cells[1..],
        (false, true) if !raw_cells.is_empty() => &raw_cells[..raw_cells.len() - 1],
        _ => &raw_cells[..],
    };

    if has_leading_pipe {
        out.push_str(r##"<tspan fill="#3f3f46">|</tspan>"##);
    }

    for (idx, cell) in cells_slice.iter().enumerate() {
        let color = CSV_PALETTE[idx % CSV_PALETTE.len()];
        let trimmed = cell.trim();
        out.push(' ');
        if is_header {
            out.push_str(&format!(r##"<tspan fill="{color}" font-weight="bold">"##));
        } else {
            out.push_str(&format!(r##"<tspan fill="{color}">"##));
        }
        highlight_md_reader_inline(out, trimmed, Some(color));
        out.push_str("</tspan> ");
        if idx + 1 < cells_slice.len() || has_trailing_pipe {
            out.push_str(r##"<tspan fill="#3f3f46">|</tspan>"##);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn highlight_md_source_row(
    text: &str,
    indent_cols: usize,
    starts_in_code_block: bool,
    heading_level: Option<u8>,
    is_blockquote_continuation: bool,
    is_table_row: bool,
    is_table_separator: bool,
    table_col_start: usize,
    is_chunk_0: bool,
    is_last_chunk: bool,
) -> String {
    let mut out = String::with_capacity(text.len() + indent_cols + 64);

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

    // Fenced code block marker
    if let Some((_, _, _)) = parse_code_fence(rest.trim_start()) {
        let ws_len = rest.chars().take_while(|&c| c == ' ' || c == '\t').count();
        let ws = &rest[..ws_len];
        let content = &rest[ws_len..];
        out.push_str(ws);
        out.push_str(&format!(
            r##"<tspan fill="#71717a">{}</tspan>"##,
            escape_xml(content)
        ));
        return out;
    }

    // Content inside a fenced code block
    if starts_in_code_block {
        out.push_str(&format!(
            r##"<tspan fill="#86efac">{}</tspan>"##,
            escape_xml(rest)
        ));
        return out;
    }

    // Table rows and separators
    if is_table_row || is_table_separator {
        if is_table_separator {
            out.push_str(&format!(
                r##"<tspan fill="#71717a">{}</tspan>"##,
                escape_xml(rest)
            ));
            return out;
        }
        highlight_md_source_table_row(&mut out, rest, table_col_start, is_chunk_0, is_last_chunk);
        return out;
    }

    // Heading (chunk 0 or continuation)
    if let Some(level) = heading_level {
        let (color, weight) = match level {
            1 => ("#fbbf24", "bold"), // H1: Amber Gold
            2 => ("#38bdf8", "bold"), // H2: Sky Blue
            3 => ("#34d399", "bold"), // H3: Emerald
            4 => ("#a78bfa", "bold"), // H4: Purple
            5 => ("#f472b6", "bold"), // H5: Pink
            _ => ("#94a3b8", "bold"), // H6: Slate
        };
        let prefix_len = if rest.starts_with('#') {
            rest.chars().take_while(|&c| c == '#').count()
        } else {
            0
        };
        if prefix_len > 0 {
            let prefix = &rest[..prefix_len];
            let after_hash = &rest[prefix_len..];
            let ws_len = after_hash
                .chars()
                .take_while(|&c| c == ' ' || c == '\t')
                .count();
            let ws = &after_hash[..ws_len];
            let content = &after_hash[ws_len..];
            out.push_str(&format!(
                r##"<tspan fill="#71717a" font-weight="{weight}">{}</tspan>"##,
                escape_xml(prefix)
            ));
            out.push_str(ws);
            out.push_str(&format!(
                r##"<tspan fill="{color}" font-weight="{weight}">"##
            ));
            highlight_md_source_inline(&mut out, content, Some(color));
            out.push_str("</tspan>");
        } else {
            out.push_str(&format!(
                r##"<tspan fill="{color}" font-weight="{weight}">"##
            ));
            highlight_md_source_inline(&mut out, rest, Some(color));
            out.push_str("</tspan>");
        }
        return out;
    }

    // Blockquote continuation row
    if is_blockquote_continuation {
        out.push_str(r##"<tspan fill="#94a3b8" font-style="italic">"##);
        highlight_md_source_inline(&mut out, rest, None);
        out.push_str("</tspan>");
        return out;
    }

    // Blockquote start
    if rest.starts_with('>') {
        let quote_count = rest.chars().take_while(|&c| c == '>').count();
        let prefix = &rest[..quote_count];
        let after_quote = &rest[quote_count..];
        let ws_len = after_quote
            .chars()
            .take_while(|&c| c == ' ' || c == '\t')
            .count();
        let ws = &after_quote[..ws_len];
        let content = &after_quote[ws_len..];
        out.push_str(&format!(
            r##"<tspan fill="#38bdf8" font-weight="bold">{}</tspan>"##,
            escape_xml(prefix)
        ));
        out.push_str(ws);
        out.push_str(r##"<tspan fill="#94a3b8" font-style="italic">"##);
        highlight_md_source_inline(&mut out, content, None);
        out.push_str("</tspan>");
        return out;
    }

    // HTML comment in Markdown
    if rest.starts_with("<!--") {
        out.push_str(&format!(
            r##"<tspan fill="#71717a">{}</tspan>"##,
            escape_xml(rest)
        ));
        return out;
    }

    // Horizontal rule
    if is_horizontal_rule(rest) {
        out.push_str(&format!(
            r##"<tspan fill="#71717a">{}</tspan>"##,
            escape_xml(rest)
        ));
        return out;
    }

    // Unordered bullet list / task list item
    let bullet_prefix = if rest.starts_with("- ") {
        Some("- ")
    } else if rest.starts_with("* ") {
        Some("* ")
    } else if rest.starts_with("+ ") {
        Some("+ ")
    } else {
        None
    };

    if let Some(prefix) = bullet_prefix {
        out.push_str(&format!(
            r##"<tspan fill="#fdba74" font-weight="bold">{}</tspan>"##,
            escape_xml(prefix)
        ));
        let after_bullet = &rest[prefix.len()..];
        if let Some(after_box) = after_bullet.strip_prefix("[ ] ") {
            out.push_str(r##"<tspan fill="#71717a" font-weight="bold">[ ] </tspan>"##);
            highlight_md_source_inline(&mut out, after_box, None);
        } else if let Some(after_box) = after_bullet
            .strip_prefix("[x] ")
            .or_else(|| after_bullet.strip_prefix("[X] "))
        {
            out.push_str(r##"<tspan fill="#34d399" font-weight="bold">[x] </tspan>"##);
            highlight_md_source_inline(&mut out, after_box, None);
        } else {
            highlight_md_source_inline(&mut out, after_bullet, None);
        }
        return out;
    }

    // Ordered list item (e.g. "1. ")
    if let Some(dot_idx) = rest.find(". ") {
        let num_part = &rest[..dot_idx];
        if !num_part.is_empty() && num_part.chars().all(|c| c.is_ascii_digit()) {
            let prefix = &rest[..dot_idx + 2];
            let after_num = &rest[dot_idx + 2..];
            out.push_str(&format!(
                r##"<tspan fill="#93c5fd" font-weight="bold">{}</tspan>"##,
                escape_xml(prefix)
            ));
            highlight_md_source_inline(&mut out, after_num, None);
            return out;
        }
    }

    // Regular paragraph
    highlight_md_source_inline(&mut out, rest, None);
    out
}

fn find_table_pipe_indices(chars: &[char]) -> Vec<usize> {
    let mut pipe_indices = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '`' {
            let mut run_len = 1;
            while i + run_len < chars.len() && chars[i + run_len] == '`' {
                run_len += 1;
            }
            if let Some(end) = find_matching_backtick_run(chars, i + run_len, run_len) {
                i = end + run_len;
            } else {
                i += run_len;
            }
        } else if c == '\\' {
            let mut bs_len = 1;
            while i + bs_len < chars.len() && chars[i + bs_len] == '\\' {
                bs_len += 1;
            }
            if bs_len % 2 == 1 && chars.get(i + bs_len) == Some(&'|') {
                i += bs_len + 1;
            } else {
                i += bs_len;
            }
        } else if c == '|' {
            pipe_indices.push(i);
            i += 1;
        } else {
            i += 1;
        }
    }
    pipe_indices
}

fn table_row_chunk_end_col(
    text: &str,
    start_col: usize,
    is_chunk_0: bool,
    is_last_chunk: bool,
) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let pipe_indices = find_table_pipe_indices(&chars);
    if pipe_indices.is_empty() {
        return start_col;
    }
    let has_leading_border =
        is_chunk_0 && chars[..pipe_indices[0]].iter().all(|c| c.is_whitespace());
    let has_trailing_border = is_last_chunk
        && pipe_indices.len() > (if has_leading_border { 1 } else { 0 })
        && chars[pipe_indices.last().copied().unwrap() + 1..]
            .iter()
            .all(|c| c.is_whitespace());
    let content_len = if has_trailing_border {
        pipe_indices.len().saturating_sub(1)
    } else {
        pipe_indices.len()
    };
    if has_leading_border {
        content_len.saturating_sub(1)
    } else {
        start_col + content_len
    }
}

fn highlight_md_source_table_row(
    out: &mut String,
    text: &str,
    start_col: usize,
    is_chunk_0: bool,
    is_last_chunk: bool,
) {
    let chars: Vec<char> = text.chars().collect();
    let pipe_indices = find_table_pipe_indices(&chars);

    if pipe_indices.is_empty() {
        let color = CSV_PALETTE[start_col % CSV_PALETTE.len()];
        out.push_str(&format!(r##"<tspan fill="{color}">"##));
        highlight_md_source_inline(out, text, Some(color));
        out.push_str("</tspan>");
        return;
    }

    let has_leading_border =
        is_chunk_0 && chars[..pipe_indices[0]].iter().all(|c| c.is_whitespace());
    let has_trailing_border = is_last_chunk
        && pipe_indices.len() > (if has_leading_border { 1 } else { 0 })
        && chars[pipe_indices.last().copied().unwrap() + 1..]
            .iter()
            .all(|c| c.is_whitespace());

    let (content_pipes, trailing_pipe) = if has_trailing_border {
        let last_idx = pipe_indices.len() - 1;
        (&pipe_indices[..last_idx], Some(pipe_indices[last_idx]))
    } else {
        (&pipe_indices[..], None)
    };

    let mut col_idx = if has_leading_border { 0 } else { start_col };
    let mut curr_char_idx = 0usize;

    if has_leading_border {
        let leading_ws: String = chars[..content_pipes[0]].iter().collect();
        out.push_str(&leading_ws);
        out.push_str(r##"<tspan fill="#71717a">|</tspan>"##);
        curr_char_idx = content_pipes[0] + 1;
        for &pipe_idx in &content_pipes[1..] {
            let cell: String = chars[curr_char_idx..pipe_idx].iter().collect();
            if !cell.is_empty() {
                let color = CSV_PALETTE[col_idx % CSV_PALETTE.len()];
                out.push_str(&format!(r##"<tspan fill="{color}">"##));
                highlight_md_source_inline(out, &cell, Some(color));
                out.push_str("</tspan>");
            }
            col_idx += 1;
            out.push_str(r##"<tspan fill="#71717a">|</tspan>"##);
            curr_char_idx = pipe_idx + 1;
        }
    } else {
        for &pipe_idx in content_pipes {
            let cell: String = chars[curr_char_idx..pipe_idx].iter().collect();
            if !cell.is_empty() {
                let color = CSV_PALETTE[col_idx % CSV_PALETTE.len()];
                out.push_str(&format!(r##"<tspan fill="{color}">"##));
                highlight_md_source_inline(out, &cell, Some(color));
                out.push_str("</tspan>");
            }
            col_idx += 1;
            out.push_str(r##"<tspan fill="#71717a">|</tspan>"##);
            curr_char_idx = pipe_idx + 1;
        }
    }

    let end_idx = trailing_pipe.unwrap_or(chars.len());
    let cell: String = chars[curr_char_idx..end_idx].iter().collect();
    if !cell.is_empty() {
        let color = CSV_PALETTE[col_idx % CSV_PALETTE.len()];
        out.push_str(&format!(r##"<tspan fill="{color}">"##));
        highlight_md_source_inline(out, &cell, Some(color));
        out.push_str("</tspan>");
    }

    if let Some(t_pipe) = trailing_pipe {
        out.push_str(r##"<tspan fill="#71717a">|</tspan>"##);
        let trailing_ws: String = chars[t_pipe + 1..].iter().collect();
        out.push_str(&trailing_ws);
    }
}

fn highlight_md_source_inline(out: &mut String, text: &str, base_color: Option<&str>) {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let mut plain_buf = String::new();

    let flush = |out: &mut String, buf: &mut String| {
        if !buf.is_empty() {
            out.push_str(&escape_xml(buf));
            buf.clear();
        }
    };

    while i < chars.len() {
        let c = chars[i];

        // 1. Inline code: `code`
        if c == '`' {
            if let Some(close_idx) = chars[i + 1..].iter().position(|&ch| ch == '`') {
                let end = i + 1 + close_idx;
                flush(out, &mut plain_buf);
                let code_content: String = chars[i + 1..end].iter().collect();
                out.push_str(r##"<tspan fill="#71717a">`</tspan>"##);
                out.push_str(&format!(
                    r##"<tspan fill="#86efac">{}</tspan>"##,
                    escape_xml(&code_content)
                ));
                out.push_str(r##"<tspan fill="#71717a">`</tspan>"##);
                i = end + 1;
                continue;
            }
        }

        // 2. Images & Links: ![alt](url) or [text](url)
        if let Some(link) = parse_markdown_link(&chars, i) {
            flush(out, &mut plain_buf);
            let text_content: String = chars[link.text_start..link.text_end].iter().collect();
            let url_content: String = chars[link.url_start..link.url_end].iter().collect();
            if link.is_image {
                out.push_str(r##"<tspan fill="#71717a">![</tspan>"##);
                out.push_str(&format!(
                    r##"<tspan fill="#c084fc">{}</tspan>"##,
                    escape_xml(&text_content)
                ));
                out.push_str(r##"<tspan fill="#71717a">](</tspan>"##);
                out.push_str(&format!(
                    r##"<tspan fill="#a855f7">{}</tspan>"##,
                    escape_xml(&url_content)
                ));
                out.push_str(r##"<tspan fill="#71717a">)</tspan>"##);
            } else {
                out.push_str(r##"<tspan fill="#71717a">[</tspan>"##);
                let link_text_color = base_color.unwrap_or("#38bdf8");
                out.push_str(&format!(
                    r##"<tspan fill="{link_text_color}" text-decoration="underline">{}</tspan>"##,
                    escape_xml(&text_content)
                ));
                out.push_str(r##"<tspan fill="#71717a">](</tspan>"##);
                out.push_str(&format!(
                    r##"<tspan fill="#60a5fa">{}</tspan>"##,
                    escape_xml(&url_content)
                ));
                out.push_str(r##"<tspan fill="#71717a">)</tspan>"##);
            }
            i = link.next_index;
            continue;
        }

        // 4. Bold + Italic: ***text*** or ___text___
        if (i + 2 < chars.len() && chars[i] == '*' && chars[i + 1] == '*' && chars[i + 2] == '*')
            || (i + 2 < chars.len()
                && chars[i] == '_'
                && chars[i + 1] == '_'
                && chars[i + 2] == '_')
        {
            let delim = chars[i];
            let mut close_idx = None;
            for j in (i + 3)..chars.len().saturating_sub(2) {
                if chars[j] == delim && chars[j + 1] == delim && chars[j + 2] == delim {
                    close_idx = Some(j);
                    break;
                }
            }
            if let Some(end) = close_idx {
                flush(out, &mut plain_buf);
                let content: String = chars[i + 3..end].iter().collect();
                let delim_str: String = std::iter::repeat_n(delim, 3).collect();
                out.push_str(&format!(
                    r##"<tspan fill="#71717a">{}</tspan>"##,
                    escape_xml(&delim_str)
                ));
                let bold_color = base_color.unwrap_or("#ffffff");
                out.push_str(&format!(
                    r##"<tspan fill="{bold_color}" font-weight="bold" font-style="italic">{}</tspan>"##,
                    escape_xml(&content)
                ));
                out.push_str(&format!(
                    r##"<tspan fill="#71717a">{}</tspan>"##,
                    escape_xml(&delim_str)
                ));
                i = end + 3;
                continue;
            }
        }

        // 5. Bold: **text** or __text__
        if (i + 1 < chars.len() && chars[i] == '*' && chars[i + 1] == '*')
            || (i + 1 < chars.len() && chars[i] == '_' && chars[i + 1] == '_')
        {
            let delim = chars[i];
            let mut close_idx = None;
            for j in (i + 2)..chars.len().saturating_sub(1) {
                if chars[j] == delim && chars[j + 1] == delim {
                    close_idx = Some(j);
                    break;
                }
            }
            if let Some(end) = close_idx {
                flush(out, &mut plain_buf);
                let content: String = chars[i + 2..end].iter().collect();
                let delim_str: String = std::iter::repeat_n(delim, 2).collect();
                out.push_str(&format!(
                    r##"<tspan fill="#71717a">{}</tspan>"##,
                    escape_xml(&delim_str)
                ));
                let bold_color = base_color.unwrap_or("#ffffff");
                out.push_str(&format!(
                    r##"<tspan fill="{bold_color}" font-weight="bold">{}</tspan>"##,
                    escape_xml(&content)
                ));
                out.push_str(&format!(
                    r##"<tspan fill="#71717a">{}</tspan>"##,
                    escape_xml(&delim_str)
                ));
                i = end + 2;
                continue;
            }
        }

        // 6. Strikethrough: ~~text~~
        if i + 1 < chars.len() && chars[i] == '~' && chars[i + 1] == '~' {
            let mut close_idx = None;
            for j in (i + 2)..chars.len().saturating_sub(1) {
                if chars[j] == '~' && chars[j + 1] == '~' {
                    close_idx = Some(j);
                    break;
                }
            }
            if let Some(end) = close_idx {
                flush(out, &mut plain_buf);
                let content: String = chars[i + 2..end].iter().collect();
                out.push_str(r##"<tspan fill="#71717a">~~</tspan>"##);
                let strike_color = base_color.unwrap_or("#94a3b8");
                out.push_str(&format!(
                    r##"<tspan fill="{strike_color}" text-decoration="line-through">{}</tspan>"##,
                    escape_xml(&content)
                ));
                out.push_str(r##"<tspan fill="#71717a">~~</tspan>"##);
                i = end + 2;
                continue;
            }
        }

        // 7. Italic: *text* (when not followed by space and closing not preceded by space)
        if c == '*' && i + 1 < chars.len() && chars[i + 1] != ' ' {
            let mut close_idx = None;
            for j in (i + 1)..chars.len() {
                if chars[j] == '*' && chars[j - 1] != ' ' {
                    close_idx = Some(j);
                    break;
                }
            }
            if let Some(end) = close_idx {
                flush(out, &mut plain_buf);
                let content: String = chars[i + 1..end].iter().collect();
                out.push_str(r##"<tspan fill="#71717a">*</tspan>"##);
                let italic_color = base_color.unwrap_or("#cbd5e1");
                out.push_str(&format!(
                    r##"<tspan fill="{italic_color}" font-style="italic">{}</tspan>"##,
                    escape_xml(&content)
                ));
                out.push_str(r##"<tspan fill="#71717a">*</tspan>"##);
                i = end + 1;
                continue;
            }
        }

        // 8. HTML tag: <tag ...>
        if c == '<'
            && i + 1 < chars.len()
            && (chars[i + 1].is_ascii_alphabetic() || chars[i + 1] == '/')
        {
            if let Some(close_idx) = chars[i + 1..].iter().position(|&ch| ch == '>') {
                let end = i + 1 + close_idx;
                flush(out, &mut plain_buf);
                let tag_str: String = chars[i..=end].iter().collect();
                out.push_str(&format!(
                    r##"<tspan fill="#7dd3fc">{}</tspan>"##,
                    escape_xml(&tag_str)
                ));
                i = end + 1;
                continue;
            }
        }

        plain_buf.push(c);
        i += 1;
    }

    flush(out, &mut plain_buf);
}

fn highlight_md_reader_inline(out: &mut String, text: &str, base_color: Option<&str>) {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let mut plain_buf = String::new();

    let flush = |out: &mut String, buf: &mut String| {
        if !buf.is_empty() {
            out.push_str(&escape_xml(buf));
            buf.clear();
        }
    };

    while i < chars.len() {
        let c = chars[i];

        // 1. Inline code: `code`
        if c == '`' {
            if let Some(close_idx) = chars[i + 1..].iter().position(|&ch| ch == '`') {
                let end = i + 1 + close_idx;
                flush(out, &mut plain_buf);
                let code_content: String = chars[i + 1..end].iter().collect();
                out.push_str(&format!(
                    r##"<tspan fill="#86efac" font-family="ui-monospace, Menlo, Monaco, Consolas, monospace">{}</tspan>"##,
                    escape_xml(&code_content)
                ));
                i = end + 1;
                continue;
            }
        }

        // 2. Images & Links: ![alt](url) or [text](url)
        if let Some(link) = parse_markdown_link(&chars, i) {
            flush(out, &mut plain_buf);
            let text_content: String = chars[link.text_start..link.text_end].iter().collect();
            if link.is_image {
                out.push_str(&format!(
                    r##"<tspan fill="#c084fc">🖼 {}</tspan>"##,
                    escape_xml(&text_content)
                ));
            } else {
                let link_text_color = base_color.unwrap_or("#38bdf8");
                out.push_str(&format!(
                    r##"<tspan fill="{link_text_color}" text-decoration="underline">{}</tspan>"##,
                    escape_xml(&text_content)
                ));
            }
            i = link.next_index;
            continue;
        }

        // 4. Bold + Italic: ***text*** or ___text___
        if (i + 2 < chars.len() && chars[i] == '*' && chars[i + 1] == '*' && chars[i + 2] == '*')
            || (i + 2 < chars.len()
                && chars[i] == '_'
                && chars[i + 1] == '_'
                && chars[i + 2] == '_')
        {
            let delim = chars[i];
            let mut close_idx = None;
            for j in (i + 3)..chars.len().saturating_sub(2) {
                if chars[j] == delim && chars[j + 1] == delim && chars[j + 2] == delim {
                    close_idx = Some(j);
                    break;
                }
            }
            if let Some(end) = close_idx {
                flush(out, &mut plain_buf);
                let content: String = chars[i + 3..end].iter().collect();
                let bold_color = base_color.unwrap_or("#ffffff");
                out.push_str(&format!(
                    r##"<tspan fill="{bold_color}" font-weight="bold" font-style="italic">{}</tspan>"##,
                    escape_xml(&content)
                ));
                i = end + 3;
                continue;
            }
        }

        // 5. Bold: **text** or __text__
        if (i + 1 < chars.len() && chars[i] == '*' && chars[i + 1] == '*')
            || (i + 1 < chars.len() && chars[i] == '_' && chars[i + 1] == '_')
        {
            let delim = chars[i];
            let mut close_idx = None;
            for j in (i + 2)..chars.len().saturating_sub(1) {
                if chars[j] == delim && chars[j + 1] == delim {
                    close_idx = Some(j);
                    break;
                }
            }
            if let Some(end) = close_idx {
                flush(out, &mut plain_buf);
                let content: String = chars[i + 2..end].iter().collect();
                let bold_color = base_color.unwrap_or("#ffffff");
                out.push_str(&format!(
                    r##"<tspan fill="{bold_color}" font-weight="bold">{}</tspan>"##,
                    escape_xml(&content)
                ));
                i = end + 2;
                continue;
            }
        }

        // 6. Strikethrough: ~~text~~
        if i + 1 < chars.len() && chars[i] == '~' && chars[i + 1] == '~' {
            let mut close_idx = None;
            for j in (i + 2)..chars.len().saturating_sub(1) {
                if chars[j] == '~' && chars[j + 1] == '~' {
                    close_idx = Some(j);
                    break;
                }
            }
            if let Some(end) = close_idx {
                flush(out, &mut plain_buf);
                let content: String = chars[i + 2..end].iter().collect();
                let strike_color = base_color.unwrap_or("#94a3b8");
                out.push_str(&format!(
                    r##"<tspan fill="{strike_color}" text-decoration="line-through">{}</tspan>"##,
                    escape_xml(&content)
                ));
                i = end + 2;
                continue;
            }
        }

        // 7. Italic: *text* (when not followed by space and closing not preceded by space)
        if c == '*' && i + 1 < chars.len() && chars[i + 1] != ' ' {
            let mut close_idx = None;
            for j in (i + 1)..chars.len() {
                if chars[j] == '*' && chars[j - 1] != ' ' {
                    close_idx = Some(j);
                    break;
                }
            }
            if let Some(end) = close_idx {
                flush(out, &mut plain_buf);
                let content: String = chars[i + 1..end].iter().collect();
                let italic_color = base_color.unwrap_or("#e4e4e7");
                out.push_str(&format!(
                    r##"<tspan fill="{italic_color}" font-style="italic">{}</tspan>"##,
                    escape_xml(&content)
                ));
                i = end + 1;
                continue;
            }
        }

        // 8. HTML tag: <tag ...>
        if c == '<'
            && i + 1 < chars.len()
            && (chars[i + 1].is_ascii_alphabetic() || chars[i + 1] == '/')
        {
            if let Some(close_idx) = chars[i + 1..].iter().position(|&ch| ch == '>') {
                let end = i + 1 + close_idx;
                flush(out, &mut plain_buf);
                let tag_str: String = chars[i..=end].iter().collect();
                out.push_str(&format!(
                    r##"<tspan fill="#7dd3fc">{}</tspan>"##,
                    escape_xml(&tag_str)
                ));
                i = end + 1;
                continue;
            }
        }

        plain_buf.push(c);
        i += 1;
    }

    flush(out, &mut plain_buf);
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

const CSV_PALETTE: &[&str] = &[
    "#38bdf8", // Sky blue
    "#34d399", // Emerald
    "#fbbf24", // Amber
    "#f472b6", // Pink
    "#a78bfa", // Purple
    "#fb923c", // Orange
    "#2dd4bf", // Teal
    "#818cf8", // Indigo
    "#f87171", // Coral red
    "#4ade80", // Light green
];

/// Detect whether content uses comma, semicolon, or tab as delimiter.
pub fn detect_csv_delimiter(content: &str) -> char {
    let mut comma_count = 0usize;
    let mut semi_count = 0usize;
    let mut tab_count = 0usize;
    for line in content.lines().take(20) {
        comma_count += line.matches(',').count();
        semi_count += line.matches(';').count();
        tab_count += line.matches('\t').count();
    }
    if tab_count > comma_count && tab_count > semi_count {
        '\t'
    } else if semi_count > comma_count {
        ';'
    } else {
        ','
    }
}

fn parse_csv_rows(content: &str, delimiter: char) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut current_row: Vec<String> = Vec::new();
    let mut current_field = String::new();
    let mut in_quotes = false;
    let mut field_has_content = false;
    let mut chars = content.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '"' if !field_has_content && !in_quotes => {
                in_quotes = true;
                field_has_content = true;
            }
            '"' if in_quotes => {
                if chars.peek() == Some(&'"') {
                    current_field.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            }
            c if c == delimiter && !in_quotes => {
                current_row.push(current_field);
                current_field = String::new();
                field_has_content = false;
            }
            '\n' if !in_quotes => {
                if current_field.ends_with('\r') {
                    current_field.pop();
                }
                current_row.push(current_field);
                current_field = String::new();
                field_has_content = false;
                rows.push(current_row);
                current_row = Vec::new();
            }
            '\r' => {
                if in_quotes {
                    current_field.push('\r');
                }
            }
            c => {
                current_field.push(c);
                field_has_content = true;
            }
        }
    }
    if !current_field.is_empty() || !current_row.is_empty() {
        if current_field.ends_with('\r') {
            current_field.pop();
        }
        current_row.push(current_field);
        rows.push(current_row);
    }
    while rows.len() > 1 && rows.last().is_some_and(|r| r.len() == 1 && r[0].is_empty()) {
        rows.pop();
    }
    rows
}

fn is_numeric_field(s: &str) -> bool {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return false;
    }
    let stripped = trimmed
        .trim_start_matches(|c| {
            c == '$' || c == '€' || c == '£' || c == '¥' || c == '+' || c == '-'
        })
        .trim_end_matches('%')
        .trim();
    if stripped.is_empty() {
        return false;
    }
    if stripped.parse::<f64>().is_ok() {
        return true;
    }
    let replaced_comma = stripped.replace(',', ".");
    if replaced_comma.parse::<f64>().is_ok() {
        return true;
    }
    let no_comma = stripped.replace(',', "");
    no_comma.parse::<f64>().is_ok()
}

fn highlight_csv_raw_row(text: &str, delimiter: char, indent_cols: usize) -> String {
    let mut out = String::with_capacity(text.len() * 3);
    for _ in 0..indent_cols {
        out.push(' ');
    }
    let mut col_idx = 0usize;
    let mut in_quotes = false;
    let mut field_has_content = false;
    let mut current_token = String::new();

    let flush_token = |out: &mut String, token: &mut String, col: usize| {
        if !token.is_empty() {
            let color = CSV_PALETTE[col % CSV_PALETTE.len()];
            out.push_str(&format!(
                r##"<tspan fill="{color}">{}</tspan>"##,
                escape_xml(token)
            ));
            token.clear();
        }
    };

    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if !field_has_content && !in_quotes => {
                flush_token(&mut out, &mut current_token, col_idx);
                in_quotes = true;
                field_has_content = true;
                out.push_str(r##"<tspan fill="#71717a">&quot;</tspan>"##);
            }
            '"' if in_quotes => {
                if chars.peek() == Some(&'"') {
                    current_token.push('"');
                    chars.next();
                } else {
                    flush_token(&mut out, &mut current_token, col_idx);
                    in_quotes = false;
                    out.push_str(r##"<tspan fill="#71717a">&quot;</tspan>"##);
                }
            }
            c if c == delimiter && !in_quotes => {
                flush_token(&mut out, &mut current_token, col_idx);
                let delim_str = if c == '\t' { "    " } else { &c.to_string() };
                out.push_str(&format!(
                    r##"<tspan fill="#52525b" font-weight="bold">{}</tspan>"##,
                    escape_xml(delim_str)
                ));
                col_idx += 1;
                field_has_content = false;
            }
            c => {
                current_token.push(c);
                field_has_content = true;
            }
        }
    }
    flush_token(&mut out, &mut current_token, col_idx);
    out
}

fn highlight_csv_table_row(text: &str, is_header: bool, table_cells: Option<&[String]>) -> String {
    if text.contains('┼') || text.starts_with('─') {
        return format!(r##"<tspan fill="#3f3f46">{}</tspan>"##, escape_xml(text));
    }

    let mut out = String::with_capacity(text.len() * 3);
    if let Some(cells) = table_cells {
        for (col_idx, part) in cells.iter().enumerate() {
            if col_idx > 0 {
                out.push_str(r##"<tspan fill="#3f3f46"> | </tspan>"##);
            }
            let color = CSV_PALETTE[col_idx % CSV_PALETTE.len()];
            let escaped = escape_xml(part);
            if is_header {
                out.push_str(&format!(
                    r##"<tspan fill="{color}" font-weight="bold">{escaped}</tspan>"##
                ));
            } else {
                out.push_str(&format!(r##"<tspan fill="{color}">{escaped}</tspan>"##));
            }
        }
        return out;
    }

    let delim = if text.contains(" | ") {
        " | "
    } else if text.contains(" │ ") {
        " │ "
    } else {
        ""
    };

    if !delim.is_empty() {
        let parts: Vec<&str> = text.split(delim).collect();
        for (col_idx, part) in parts.iter().enumerate() {
            if col_idx > 0 {
                out.push_str(r##"<tspan fill="#3f3f46"> | </tspan>"##);
            }
            let color = CSV_PALETTE[col_idx % CSV_PALETTE.len()];
            let escaped = escape_xml(part);
            if is_header {
                out.push_str(&format!(
                    r##"<tspan fill="{color}" font-weight="bold">{escaped}</tspan>"##
                ));
            } else {
                out.push_str(&format!(r##"<tspan fill="{color}">{escaped}</tspan>"##));
            }
        }
        return out;
    }
    escape_xml(text)
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

    #[test]
    fn yaml_highlighting() {
        let yaml = r##"%YAML 1.2
---
# Application config
app:
  name: "glanvu"
  version: 1.0.0
  active: true
  score: 98.5
  empty: null
  endpoint: https://api.example.com:443/v1
  tags: [fast, image, "viewer"]
  flow_map: {x: 10, y: 20}
  defaults: &base_config
    timeout: 30
  staging:
    <<: *base_config
    debug: false
  features:
    - keyboard-first
    - name: fast-nav
      enabled: yes
  raw_multiline: |
    line one
    line two
...
"##;
        let doc = TextDocument::from_string_with_format(yaml.to_string(), SourceFormat::Yaml);
        let svg = doc.page_svg(0).unwrap();

        // Directive %YAML
        assert!(svg.contains(r##"fill="#c084fc">%YAML"##));

        // Document markers --- and ...
        assert!(svg.contains(r##"fill="#a78bfa" font-weight="bold">---</tspan>"##));
        assert!(svg.contains(r##"fill="#a78bfa" font-weight="bold">...</tspan>"##));

        // Comment
        assert!(svg.contains(r##"fill="#71717a"># Application config</tspan>"##));

        // Keys in sky blue
        assert!(svg.contains(r##"fill="#93c5fd">app</tspan>"##));
        assert!(svg.contains(r##"fill="#93c5fd">version</tspan>"##));

        // Strings in green
        assert!(svg.contains(r##"fill="#86efac">&quot;glanvu&quot;</tspan>"##));

        // Booleans in pink
        assert!(svg.contains(r##"fill="#f472b6">true</tspan>"##));
        assert!(svg.contains(r##"fill="#f472b6">null</tspan>"##));
        assert!(svg.contains(r##"fill="#f472b6">yes</tspan>"##));
        assert!(svg.contains(r##"fill="#f472b6">false</tspan>"##));

        // Numbers in orange
        assert!(svg.contains(r##"fill="#fdba74">98.5</tspan>"##));
        assert!(svg.contains(r##"fill="#fdba74">30</tspan>"##));

        // Block scalar | in bold orange
        assert!(svg.contains(r##"fill="#fdba74" font-weight="bold">|</tspan>"##));

        // Anchor &base_config and alias *base_config
        assert!(svg.contains(r##"fill="#a78bfa" font-weight="bold">&amp;base_config</tspan>"##));
        assert!(svg.contains(r##"fill="#a78bfa" font-weight="bold">*base_config</tspan>"##));

        // Bullet item '-' in bold orange
        assert!(svg.contains(r##"fill="#fdba74" font-weight="bold">- </tspan>"##));

        // Flow collections brackets
        assert!(svg.contains(r##"fill="#cbd5e1">[</tspan>"##));
        assert!(svg.contains(r##"fill="#cbd5e1">{</tspan>"##));

        // Rainbow indent guides
        assert!(svg.contains(r##"fill-opacity="0.45">│ </tspan>"##));

        // URL port ':443' not mistaken for a YAML key delimiter
        assert!(svg.contains("https://api.example.com:443/v1"));
        assert!(!svg.contains(r##"fill="#93c5fd">https://api.example.com</tspan>"##));
    }

    #[test]
    fn text_document_loads_yaml_and_bak() {
        let dir = std::env::temp_dir().join("glanvu-text-yaml-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // .yaml file
        let yaml_path = dir.join("compose.yaml");
        let yaml_content = "services:\n  app:\n    image: ubuntu:latest\n    restart: always";
        std::fs::write(&yaml_path, yaml_content).unwrap();
        let doc_yaml = TextDocument::load(&yaml_path).unwrap();
        assert_eq!(doc_yaml.content(), yaml_content);
        assert_eq!(doc_yaml.format, SourceFormat::Yaml);
        let img_yaml = doc_yaml.render_fit(0).unwrap();
        assert_eq!(img_yaml.width, TEXT_PAGE_WIDTH);

        // .yml file
        let yml_path = dir.join("ci.yml");
        let yml_content = "name: CI\non: [push]\njobs:\n  test:\n    runs-on: ubuntu-latest";
        std::fs::write(&yml_path, yml_content).unwrap();
        let doc_yml = TextDocument::load(&yml_path).unwrap();
        assert_eq!(doc_yml.content(), yml_content);
        assert_eq!(doc_yml.format, SourceFormat::Yaml);
        let img_yml = doc_yml.render_fit(0).unwrap();
        assert_eq!(img_yml.width, TEXT_PAGE_WIDTH);

        // .bak file
        let bak_path = dir.join("compose.yaml.bak");
        std::fs::write(&bak_path, yaml_content).unwrap();
        let doc_bak = TextDocument::load(&bak_path).unwrap();
        assert_eq!(doc_bak.content(), yaml_content);
        assert_eq!(doc_bak.format, SourceFormat::Yaml);
        let img_bak = doc_bak.render_fit(0).unwrap();
        assert_eq!(img_bak.width, TEXT_PAGE_WIDTH);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn yaml_numbers_and_multiline_quotes() {
        // Valid and invalid YAML numbers
        assert!(is_yaml_number("1_000_000"));
        assert!(is_yaml_number("3.141_592"));
        assert!(is_yaml_number("0xdead_beef"));
        assert!(is_yaml_number("0o755"));
        assert!(is_yaml_number("0b1010_0101"));
        assert!(is_yaml_number(".inf"));
        assert!(is_yaml_number(".nan"));

        // Malformed underscore forms rejected
        assert!(!is_yaml_number("1__2"));
        assert!(!is_yaml_number("1_"));
        assert!(!is_yaml_number("_1"));
        assert!(!is_yaml_number("3._14"));
        assert!(!is_yaml_number("3_.14"));
        assert!(!is_yaml_number("0x__beef"));
        assert!(!is_yaml_number("0xbeef_"));

        // Wrapped double-quoted string with an apostrophe (single quote)
        let long_double = "title: \"This is a very long string that wraps across lines and contains an apostrophe ' that should not close it\"";
        let doc_d =
            TextDocument::from_string_with_format(long_double.to_string(), SourceFormat::Yaml);
        assert_eq!(doc_d.pages[0].len(), 2);
        assert_eq!(doc_d.pages[0][1].string_quote, Some('"'));
        let svg_d = doc_d.page_svg(0).unwrap();
        // The wrapped part must be rendered as string green
        assert!(svg_d.contains(r##"fill="#86efac">that should not close it&quot;</tspan>"##));

        // Wrapped single-quoted string with double quotes inside
        let long_single = "quote: 'This is a very long single-quoted string that wraps across lines and contains double-quotes \"hello\" inside'";
        let doc_s =
            TextDocument::from_string_with_format(long_single.to_string(), SourceFormat::Yaml);
        assert_eq!(doc_s.pages[0].len(), 2);
        assert_eq!(doc_s.pages[0][1].string_quote, Some('\''));
        let svg_s = doc_s.page_svg(0).unwrap();
        assert!(svg_s.contains(
            r##"<tspan fill="#86efac">double-quotes &quot;hello&quot; inside&apos;</tspan>"##
        ));
    }

    #[test]
    fn md_highlighting() {
        let md = r##"# Heading 1
## Heading 2
### Heading 3
#### Heading 4
##### Heading 5
###### Heading 6

> This is a quote
> with multiple lines

- Bullet item
* Another bullet
+ Plus bullet
1. Numbered item
2. Second item
- [ ] Incomplete task
- [x] Completed task

---

Inline `code`, **bold**, *italic*, ***bold italic***, ~~strike~~.
And [Link](https://example.com) or ![Img](photo.jpg).

```rust
fn main() {
    println!("Hello, Markdown!");
}
```

| Header 1 | Header 2 |
|----------|----------|
| Val 1    | Val 2    |

    indented code or list
"##;
        // Mode 1: Reader mode
        let doc = TextDocument::from_string_with_options(
            md.to_string(),
            SourceFormat::Md,
            PageLayout::Portrait,
            1,
        );
        let svg = doc.page_svg(0).unwrap();

        // Headings H1..H6
        assert!(
            svg.contains(r##"fill="#fbbf24" font-weight="bold" font-size="24">Heading 1</tspan>"##)
        );
        assert!(
            svg.contains(r##"fill="#38bdf8" font-weight="bold" font-size="21">Heading 2</tspan>"##)
        );
        assert!(
            svg.contains(r##"fill="#34d399" font-weight="bold" font-size="19">Heading 3</tspan>"##)
        );
        assert!(
            svg.contains(r##"fill="#a78bfa" font-weight="bold" font-size="18">Heading 4</tspan>"##)
        );
        assert!(
            svg.contains(r##"fill="#f472b6" font-weight="bold" font-size="17">Heading 5</tspan>"##)
        );
        assert!(
            svg.contains(r##"fill="#94a3b8" font-weight="bold" font-size="16">Heading 6</tspan>"##)
        );

        // Blockquotes
        assert!(svg.contains(r##"fill="#38bdf8" font-weight="bold">▎ </tspan>"##));
        assert!(svg.contains(r##"fill="#cbd5e1" font-style="italic">"##));
        assert!(svg.contains("This is a quote"));

        // Bullets and numbers
        assert!(svg.contains(r##"fill="#fdba74" font-weight="bold">• </tspan>"##));
        assert!(svg.contains(r##"fill="#93c5fd" font-weight="bold">1. </tspan>"##));
        assert!(svg.contains(r##"fill="#93c5fd" font-weight="bold">2. </tspan>"##));

        // Task checkboxes
        assert!(svg.contains(r##"fill="#71717a">☐ </tspan>"##));
        assert!(svg.contains(r##"fill="#34d399" font-weight="bold">☑ </tspan>"##));

        // Horizontal rule
        assert!(svg.contains(r##"fill="#3f3f46">────────────────────────────────────────────────────────────────────────────</tspan>"##));

        // Inline formatting
        assert!(svg.contains(r##"fill="#86efac" font-family="ui-monospace, Menlo, Monaco, Consolas, monospace">code</tspan>"##));
        assert!(svg.contains(r##"fill="#ffffff" font-weight="bold">bold</tspan>"##));
        assert!(svg.contains(r##"fill="#e4e4e7" font-style="italic">italic</tspan>"##));
        assert!(svg.contains(
            r##"fill="#ffffff" font-weight="bold" font-style="italic">bold italic</tspan>"##
        ));
        assert!(svg.contains(r##"fill="#94a3b8" text-decoration="line-through">strike</tspan>"##));
        assert!(svg.contains(r##"fill="#38bdf8" text-decoration="underline">Link</tspan>"##));
        assert!(svg.contains(r##"fill="#c084fc">🖼 Img</tspan>"##));

        // Code block
        assert!(svg.contains(r##"╭─ </tspan><tspan fill="#7dd3fc""##));
        assert!(svg.contains("rust</tspan>"));
        assert!(svg.contains(r##"fill="#86efac" font-family="ui-monospace, Menlo, Monaco, Consolas, monospace" font-size="16">fn main() {</tspan>"##));
        assert!(svg.contains(r##"println!(&quot;Hello, Markdown!&quot;);</tspan>"##));

        // Table
        assert!(svg.contains(r##"fill="#3f3f46"> | </tspan>"##));
        assert!(svg.contains(r##"fill="#38bdf8" font-weight="bold">Header 1"##));
        assert!(svg.contains("─┼─"));

        // Rainbow indent guides
        assert!(svg.contains(r##"fill-opacity="0.45">│ </tspan>"##));

        // Mode 0: Default Source mode
        let doc_src = TextDocument::from_string_with_format(md.to_string(), SourceFormat::Md);
        assert_eq!(doc_src.render_mode(), 0);
        let svg_src = doc_src.page_svg(0).unwrap();
        assert!(svg_src.contains(r##"fill="#71717a" font-weight="bold">#</tspan>"##));
        assert!(svg_src.contains(r##"fill="#71717a">|</tspan>"##));
        assert!(svg_src.contains(r##"fill="#38bdf8"> Val 1"##));
        assert!(svg_src.contains(r##">1</text>"##));
    }

    #[test]
    fn md_frontmatter() {
        let md = r#"---
title: Test Note
tags: [test, doc]
---

# Title
Body text.
"#;
        let doc = TextDocument::from_string_with_options(
            md.to_string(),
            SourceFormat::Md,
            PageLayout::Portrait,
            1,
        );
        let svg = doc.page_svg(0).unwrap();
        assert!(svg.contains(r##"fill="#52525b">─── metadata ───</tspan>"##));
        assert!(svg.contains(r##"fill="#71717a" font-style="italic">title: Test Note</tspan>"##));
        assert!(svg.contains(r##"fill="#fbbf24" font-weight="bold" font-size="24">Title</tspan>"##));
    }

    #[test]
    fn text_document_loads_md_and_bak() {
        let dir = std::env::temp_dir().join("glanvu-text-md-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // .md file
        let md_path = dir.join("README.md");
        let md_content = "# Glanvu\n\nFast image viewer.\n- Feature 1\n- Feature 2";
        std::fs::write(&md_path, md_content).unwrap();
        let doc_md = TextDocument::load(&md_path).unwrap();
        assert_eq!(doc_md.content(), md_content);
        assert_eq!(doc_md.format, SourceFormat::Md);
        let img_md = doc_md.render_fit(0).unwrap();
        assert_eq!(img_md.width, TEXT_PAGE_WIDTH);

        // .markdown file
        let markdown_path = dir.join("guide.markdown");
        let markdown_content = "## Guide\nSome text with `code`.";
        std::fs::write(&markdown_path, markdown_content).unwrap();
        let doc_markdown = TextDocument::load(&markdown_path).unwrap();
        assert_eq!(doc_markdown.content(), markdown_content);
        assert_eq!(doc_markdown.format, SourceFormat::Md);
        let img_markdown = doc_markdown.render_fit(0).unwrap();
        assert_eq!(img_markdown.width, TEXT_PAGE_WIDTH);

        // .bak file
        let bak_path = dir.join("README.md.bak");
        std::fs::write(&bak_path, md_content).unwrap();
        let doc_bak = TextDocument::load(&bak_path).unwrap();
        assert_eq!(doc_bak.content(), md_content);
        assert_eq!(doc_bak.format, SourceFormat::Md);
        let img_bak = doc_bak.render_fit(0).unwrap();
        assert_eq!(img_bak.width, TEXT_PAGE_WIDTH);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn md_code_fence_matching_and_nesting() {
        let md = r##"```markdown
In this code block, ~~~ should not close it!
And # Not A Heading should not be styled as heading.
```

After code block, this is # Real Heading

~~~~
Here ``` is not a closing fence
~~~~
"##;
        let doc = TextDocument::from_string_with_options(
            md.to_string(),
            SourceFormat::Md,
            PageLayout::Portrait,
            1,
        );
        let svg = doc.page_svg(0).unwrap();

        // ~~~ inside ``` block rendered as code green
        assert!(svg.contains(r##"In this code block, ~~~ should not close it!</tspan>"##));
        // Heading inside ``` block rendered as code green, not heading
        assert!(svg.contains(r##"And # Not A Heading should not be styled as heading.</tspan>"##));
        assert!(svg.contains(r##"fill="#86efac""##));

        // Outside code block
        assert!(svg.contains(r##"After code block"##));

        // ``` inside ~~~~ block rendered as code green
        assert!(svg.contains(r##"Here ``` is not a closing fence</tspan>"##));
    }

    #[test]
    fn md_table_and_emoji_handling() {
        let md = r#"
| Col 1 | Col 2 |
|-------|-------|
| Val 1 | ✅ draft with [Link](https://example.com) and ✏️ |
"#;
        let doc = TextDocument::from_string_with_options(
            md.to_string(),
            SourceFormat::Md,
            PageLayout::Portrait,
            1,
        );
        let svg = doc.page_svg(0).unwrap();
        assert!(svg.contains(r##"fill="#38bdf8" font-weight="bold">Col 1"##));
        assert!(svg.contains(r##"fill="#34d399" font-weight="bold">Col 2"##));
        assert!(svg.contains("─┼─"));
        assert!(svg.contains("✓ draft with"));
        assert!(svg.contains(r##"fill="#34d399" text-decoration="underline">Link</tspan>"##));
        assert!(svg.contains('✎'));
    }

    #[test]
    fn md_source_mode_and_landscape() {
        let md = r#"# Title
> Quote
- [ ] Task
| A | B |
|---|---|
| 1 | [Link](https://example.com) |
"#;
        // Mode 0: Source mode in Landscape
        let doc_source = TextDocument::from_string_with_options(
            md.to_string(),
            SourceFormat::Md,
            PageLayout::Landscape,
            0,
        );
        assert_eq!(doc_source.layout(), PageLayout::Landscape);
        assert_eq!(doc_source.render_mode(), 0);
        let (w, h) = doc_source.page_size(0).unwrap();
        assert_eq!(w, 1600.0);
        assert_eq!(h, 1000.0);

        let svg = doc_source.page_svg(0).unwrap();
        assert!(svg.contains(r##"width="1600" height="1000""##));
        // Source mode retains raw syntax: '#' in heading, '>' in quote, '[ ]' in task
        assert!(svg.contains(r##"fill="#71717a" font-weight="bold">#</tspan>"##));
        assert!(svg.contains(r##"fill="#38bdf8" font-weight="bold">&gt;</tspan>"##));
        assert!(svg.contains(r##"fill="#71717a" font-weight="bold">[ ] </tspan>"##));
        assert!(svg.contains(r##"fill="#71717a">|</tspan>"##));
        assert!(svg.contains("https://example.com"));
        assert!(svg.contains(r##"fill="#38bdf8"> 1 </tspan>"##));
    }

    #[test]
    fn md_complex_links_nested_and_escaped() {
        let md = r#"
Here is [Wikipedia (Rust)](https://en.wikipedia.org/wiki/Rust_(programming_language)) and \[not a link\].
Also image: ![An [inner] image](https://site.org/pic_(2).png).
"#;
        // Mode 1: Reader mode: hidden URLs, parsed nested parens/brackets
        let doc_reader = TextDocument::from_string_with_options(
            md.to_string(),
            SourceFormat::Md,
            PageLayout::Portrait,
            1,
        );
        let svg_reader = doc_reader.page_svg(0).unwrap();
        assert!(svg_reader.contains("Wikipedia (Rust)"));
        assert!(!svg_reader.contains("programming_language"));
        assert!(svg_reader.contains("An [inner] image"));
        assert!(!svg_reader.contains("pic_(2).png"));
        assert!(svg_reader.contains(r##"\[not a link\]"##));

        // Mode 0: Source mode: raw URLs visible
        let doc_source = TextDocument::from_string_with_format(md.to_string(), SourceFormat::Md);
        assert_eq!(doc_source.render_mode(), 0);
        let svg_source = doc_source.page_svg(0).unwrap();
        assert!(svg_source.contains("https://en.wikipedia.org/wiki/Rust_(programming_language)"));
        assert!(svg_source.contains("https://site.org/pic_(2).png"));
    }

    #[test]
    fn md_long_table_row_not_split() {
        let long_cell = "A".repeat(180);
        let md = format!("| Short | Long |\n|---|---|\n| Cell 1 | {long_cell} |\n");
        let doc_reader = TextDocument::from_string_with_options(
            md.clone(),
            SourceFormat::Md,
            PageLayout::Portrait,
            1,
        );
        assert_eq!(doc_reader.page_count(), 1);
        let svg = doc_reader.page_svg(0).unwrap();
        assert!(svg.contains("Cell 1"));
        assert!(svg.contains('…'));
        assert!(svg.contains("AAAA"));

        // In Source (raw) mode, table rows wrap to avoid extending past the canvas margin
        let doc_source = TextDocument::from_string_with_format(md, SourceFormat::Md);
        let svg_source = doc_source.page_svg(0).unwrap();
        assert!(svg_source.contains("Cell 1"));
    }

    #[test]
    fn md_table_column_alignment_and_rainbow() {
        let md = r#"| Product | Price | Qty | Description |
|:---|---:|---:|:---|
| Widget | $19.99 | 100 | Standard `tool` |
| Gadget | $129.50 | 5 | Deluxe [link](https://example.com) |
"#;
        // Mode 1: Reader mode
        let doc_reader = TextDocument::from_string_with_options(
            md.to_string(),
            SourceFormat::Md,
            PageLayout::Portrait,
            1,
        );
        let svg_reader = doc_reader.page_svg(0).unwrap();

        // Check header columns rainbow colors: col 0 (#38bdf8), col 1 (#34d399), col 2 (#fbbf24), col 3 (#f472b6)
        assert!(svg_reader.contains(r##"fill="#38bdf8" font-weight="bold">Product"##));
        assert!(svg_reader.contains(r##"fill="#34d399" font-weight="bold">"##));
        assert!(svg_reader.contains(r##"fill="#fbbf24" font-weight="bold">"##));
        assert!(svg_reader.contains(r##"fill="#f472b6" font-weight="bold">Description"##));

        // Check dividers and separator
        assert!(svg_reader.contains(r##"fill="#3f3f46"> | </tspan>"##));
        assert!(svg_reader.contains("─┼─"));

        // Check numeric right alignment (prices and quantities right aligned)
        assert!(svg_reader.contains("$19.99"));
        assert!(svg_reader.contains("$129.50"));

        // Inline markdown preserved
        assert!(svg_reader.contains(r##"fill="#86efac""##));
        assert!(svg_reader.contains("tool</tspan>"));
        assert!(svg_reader.contains(r##"fill="#f472b6" text-decoration="underline">link</tspan>"##));

        // Mode 0: Source mode
        let doc_source = TextDocument::from_string_with_options(
            md.to_string(),
            SourceFormat::Md,
            PageLayout::Portrait,
            0,
        );
        let svg_source = doc_source.page_svg(0).unwrap();

        // Raw pipes preserved in #71717a
        assert!(svg_source.contains(r##"fill="#71717a">|</tspan>"##));

        // Raw cells colored in rainbow order
        assert!(svg_source.contains(r##"fill="#38bdf8"> Product </tspan>"##));
        assert!(svg_source.contains(r##"fill="#34d399"> Price </tspan>"##));
        assert!(svg_source.contains(r##"fill="#fbbf24"> Qty </tspan>"##));
        assert!(svg_source.contains(r##"fill="#f472b6"> Description </tspan>"##));

        // Test escaped pipe in source mode stays within single cell rainbow span
        let md_escaped = "| Key \\| Extra | Value |\n|---|---|\n| a\\|b | 42 |\n";
        let doc_escaped = TextDocument::from_string_with_options(
            md_escaped.to_string(),
            SourceFormat::Md,
            PageLayout::Portrait,
            0,
        );
        let svg_escaped = doc_escaped.page_svg(0).unwrap();
        assert!(svg_escaped.contains(r##"fill="#38bdf8"> a\|b </tspan>"##));
        assert!(svg_escaped.contains(r##"fill="#34d399"> 42 </tspan>"##));

        // Test wide CJK unicode and truncated styled link
        let long_link = format!("[{}](https://example.com)", "A".repeat(120));
        let md_cjk = format!("| 言語 | Description |\n|:---|:---|\n| 日本語 | {long_link} |\n");
        let doc_cjk = TextDocument::from_string_with_options(
            md_cjk,
            SourceFormat::Md,
            PageLayout::Portrait,
            1,
        );
        let svg_cjk = doc_cjk.page_svg(0).unwrap();
        assert!(svg_cjk.contains("日本語"));
        // Truncated link retains link markup and column-matching underline styling
        assert!(svg_cjk.contains(r##"fill="#34d399" text-decoration="underline">"##));
        assert!(svg_cjk.contains('…'));
    }

    #[test]
    fn markdown_source_wrapped_table_row_preserves_column_palette() {
        let md = r#"| Area | Subarea | Topic | Key Sources | Status | Notes |
|:---|:---|:---|:---|:---|:---|
| 08-knowledge | 01-projects | 1.1-glanvu | [Projects](https://example.com/p) [Apple Pay CA](https://example.com/a) [TAA/TANK](https://example.com/t) [Card->COU->CQ](https://example.com/c) | ✓ draft | Long notes that continue across the row |
"#;
        let doc = TextDocument::from_string_with_options(
            md.to_string(),
            SourceFormat::Md,
            PageLayout::Portrait,
            0,
        );
        let svg = doc.page_svg(0).unwrap();
        // Col 3 (Key Sources) is pink #f472b6; its link texts must retain #f472b6
        assert!(svg.contains(r##"fill="#f472b6" text-decoration="underline">Projects</tspan>"##));
        assert!(
            svg.contains(r##"fill="#f472b6" text-decoration="underline">Apple Pay CA</tspan>"##)
        );
        assert!(svg.contains(r##"fill="#f472b6" text-decoration="underline">TAA/TANK</tspan>"##));
        assert!(svg.contains(
            r##"fill="#f472b6" text-decoration="underline">Card-&gt;COU-&gt;CQ</tspan>"##
        ));
        // Col 4 (Status) is purple #a78bfa
        assert!(svg.contains(r##"fill="#a78bfa"> ✓ draft </tspan>"##));
    }

    #[test]
    fn csv_raw_mode_and_rainbow_palette() {
        let csv = "id,name,age,city\n1,Alice,30,London\n2,Bob,25,Paris\n";
        // Mode 0: Default Raw mode
        let doc_raw = TextDocument::from_string_with_format(csv.to_string(), SourceFormat::Csv);
        assert_eq!(doc_raw.render_mode(), 0);
        assert_eq!(doc_raw.layout(), PageLayout::Portrait);
        let svg = doc_raw.page_svg(0).unwrap();

        // Line numbers and vertical rule present in raw mode
        assert!(svg.contains(r##"<line x1="92""##));
        assert!(svg.contains(r##">1</text>"##));
        assert!(svg.contains(r##">2</text>"##));

        // Rainbow columns: Col 0 (#38bdf8), Col 1 (#34d399), Col 2 (#fbbf24), Col 3 (#f472b6)
        assert!(svg.contains(r##"fill="#38bdf8">id</tspan>"##));
        assert!(svg.contains(r##"fill="#34d399">name</tspan>"##));
        assert!(svg.contains(r##"fill="#fbbf24">age</tspan>"##));
        assert!(svg.contains(r##"fill="#f472b6">city</tspan>"##));

        // Delimiters styled in muted zinc-600
        assert!(svg.contains(r##"fill="#52525b" font-weight="bold">,</tspan>"##));

        // Data rows have column colors
        assert!(svg.contains(r##"fill="#38bdf8">1</tspan>"##));
        assert!(svg.contains(r##"fill="#34d399">Alice</tspan>"##));
        assert!(svg.contains(r##"fill="#fbbf24">30</tspan>"##));
    }

    #[test]
    fn csv_table_mode_and_landscape() {
        let csv =
            "ID,Product,Price,Quantity\n101,Mechanical Keyboard,$129.99,15\n102,Mouse,$49.50,42\n";
        // Mode 1: Table mode in Landscape
        let doc_table = TextDocument::from_string_with_options(
            csv.to_string(),
            SourceFormat::Csv,
            PageLayout::Landscape,
            1,
        );
        assert_eq!(doc_table.render_mode(), 1);
        assert_eq!(doc_table.layout(), PageLayout::Landscape);
        let svg = doc_table.page_svg(0).unwrap();

        // No raw line numbers gutter in Table mode
        assert!(!svg.contains(r##"<line x1="92""##));

        // Table headers bold in column colors
        assert!(svg.contains(r##"fill="#38bdf8" font-weight="bold">"##));
        assert!(svg.contains("Mechanical Keyboard"));

        // Column dividers and separator line
        assert!(svg.contains(" | "));
        assert!(svg.contains("─┼─"));

        // Numeric alignment: price and quantity columns right-aligned with spaces
        assert!(svg.contains("$129.99"));
        assert!(svg.contains("42"));
    }

    #[test]
    fn csv_semicolon_delimiter_detection() {
        let csv = "Nombre;Edad;Saldo\nJuan;30;1250,50\nMaria;25;800,00\n";
        assert_eq!(detect_csv_delimiter(csv), ';');

        let doc_raw = TextDocument::from_string_with_format(csv.to_string(), SourceFormat::Csv);
        let svg_raw = doc_raw.page_svg(0).unwrap();
        // Semicolon delimiter styled in #52525b
        assert!(svg_raw.contains(r##"fill="#52525b" font-weight="bold">;</tspan>"##));
        assert!(svg_raw.contains("Juan"));
        assert!(svg_raw.contains("Maria"));

        let doc_table = TextDocument::from_string_with_options(
            csv.to_string(),
            SourceFormat::Csv,
            PageLayout::Portrait,
            1,
        );
        let svg_table = doc_table.page_svg(0).unwrap();
        assert!(svg_table.contains("Nombre"));
        assert!(svg_table.contains("Juan"));
        assert!(svg_table.contains("─┼─"));
    }

    #[test]
    fn csv_quotes_and_escapes() {
        let csv = "id,name,notes\n1,\"Doe, John\",\"Senior dev, leads team\"\n2,\"Smith, \"\"Bob\"\"\",Staff\n3,Screen 15\" OLED,\n";
        let doc_table = TextDocument::from_string_with_options(
            csv.to_string(),
            SourceFormat::Csv,
            PageLayout::Portrait,
            1,
        );
        let svg = doc_table.page_svg(0).unwrap();
        // Comma inside quote did not split field
        assert!(svg.contains("Doe, John"));
        assert!(svg.contains("Senior dev, leads team"));
        // Escaped quote "" -> " (rendered as &quot; in SVG)
        assert!(svg.contains("Smith, &quot;Bob&quot;"));
        // Literal quote inside unquoted field preserved
        assert!(svg.contains("Screen 15&quot; OLED"));

        // Verify parse_csv_rows directly for trailing empty fields
        let rows = parse_csv_rows(csv, ',');
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[3], vec!["3", "Screen 15\" OLED", ""]);
    }

    #[test]
    fn tsv_raw_and_table_modes() {
        let tsv = "ID\tItem\tScore\n1\tAlpha\t98.5\n2\tBeta\t87.0\n";
        // Raw mode
        let doc_raw = TextDocument::from_string_with_format(tsv.to_string(), SourceFormat::Tsv);
        assert_eq!(doc_raw.render_mode(), 0);
        let svg_raw = doc_raw.page_svg(0).unwrap();
        assert!(svg_raw.contains(r##"<line x1="92""##));
        assert!(svg_raw.contains("Alpha"));
        assert!(svg_raw.contains("Beta"));

        // Table mode
        let doc_table = TextDocument::from_string_with_options(
            tsv.to_string(),
            SourceFormat::Tsv,
            PageLayout::Portrait,
            1,
        );
        assert_eq!(doc_table.render_mode(), 1);
        let svg_table = doc_table.page_svg(0).unwrap();
        assert!(svg_table.contains(" | "));
        assert!(svg_table.contains("─┼─"));
        assert!(svg_table.contains("Alpha"));
    }

    #[test]
    fn text_document_loads_csv_and_tsv_and_bak() {
        let dir = std::env::temp_dir().join("glanvu-text-csv-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // .csv file
        let csv_path = dir.join("data.csv");
        let csv_content = "A,B\n1,2\n";
        std::fs::write(&csv_path, csv_content).unwrap();
        let doc_csv = TextDocument::load(&csv_path).unwrap();
        assert_eq!(doc_csv.content(), csv_content);
        assert_eq!(doc_csv.format, SourceFormat::Csv);
        let img_csv = doc_csv.render_fit(0).unwrap();
        assert_eq!(img_csv.width, TEXT_PAGE_WIDTH);

        // .tsv file
        let tsv_path = dir.join("data.tsv");
        let tsv_content = "X\tY\n10\t20\n";
        std::fs::write(&tsv_path, tsv_content).unwrap();
        let doc_tsv = TextDocument::load(&tsv_path).unwrap();
        assert_eq!(doc_tsv.content(), tsv_content);
        assert_eq!(doc_tsv.format, SourceFormat::Tsv);
        let img_tsv = doc_tsv.render_fit(0).unwrap();
        assert_eq!(img_tsv.width, TEXT_PAGE_WIDTH);

        // .bak file
        let bak_path = dir.join("data.csv.bak");
        std::fs::write(&bak_path, csv_content).unwrap();
        let doc_bak = TextDocument::load(&bak_path).unwrap();
        assert_eq!(doc_bak.content(), csv_content);
        assert_eq!(doc_bak.format, SourceFormat::Csv);
        let img_bak = doc_bak.render_fit(0).unwrap();
        assert_eq!(img_bak.width, TEXT_PAGE_WIDTH);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wide_csv_table_mode_does_not_panic() {
        // 50 columns
        let header = (0..50)
            .map(|i| format!("Col{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let row1 = (0..50)
            .map(|i| format!("val{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let csv = format!("{header}\n{row1}\n");

        let doc_portrait = TextDocument::from_string_with_options(
            csv.clone(),
            SourceFormat::Csv,
            PageLayout::Portrait,
            1,
        );
        let svg_p = doc_portrait.page_svg(0).unwrap();
        assert!(svg_p.contains(" | "));

        let doc_landscape = TextDocument::from_string_with_options(
            csv,
            SourceFormat::Csv,
            PageLayout::Landscape,
            1,
        );
        let svg_l = doc_landscape.page_svg(0).unwrap();
        assert!(svg_l.contains(" | "));
    }

    #[test]
    fn markdown_table_with_bullet_and_numeric_first_col() {
        let md = "\
| Rank | Item | Note |
|---|---|---|
| 1 | - bullet-like text | simple |
| 100 | another | value |
";
        let doc = TextDocument::from_string_with_options(
            md.to_string(),
            SourceFormat::Md,
            PageLayout::Portrait,
            1,
        );
        let svg = doc.page_svg(0).unwrap();
        assert!(svg.contains(" | "));
        // Verify it was NOT rendered as an unordered list bullet symbol
        assert!(!svg.contains("• "));
        assert!(!svg.contains(r##"fill="#fdba74" font-weight="bold">• </tspan>"##));
        // Verify rainbow column palette is used
        assert!(svg.contains(r##"fill="#38bdf8""##));
        assert!(svg.contains(r##"fill="#34d399""##));
    }

    #[test]
    fn markdown_visual_text_preserves_literals() {
        assert_eq!(markdown_visual_text("file_name_test"), "file_name_test");
        assert_eq!(markdown_visual_text("`a_b_c`"), "a_b_c");
        assert_eq!(markdown_visual_text("**bold**"), "bold");
        assert_eq!(markdown_visual_text("*italic*"), "italic");
        assert_eq!(markdown_visual_text("[link](url)"), "link");
        assert_eq!(markdown_visual_text("![img](url)"), "🖼 img");
    }

    #[test]
    fn code_fence_closing_requires_empty_info() {
        let md = "\
```rust
| A | B |
|---|---|
| 1 | 2 |
```info
| X | Y |
|---|---|
| 3 | 4 |
```
";
        let doc = TextDocument::from_string_with_options(
            md.to_string(),
            SourceFormat::Md,
            PageLayout::Portrait,
            1,
        );
        let svg = doc.page_svg(0).unwrap();
        // The table inside the code block should not be transformed to aligned " │ " table
        assert!(!svg.contains("─┼─"));
    }

    #[test]
    fn markdown_table_cell_with_backticks_not_code_fence() {
        let md = "\
| Code | Description |
|---|---|
| ```example``` | Value |
";
        let doc = TextDocument::from_string_with_options(
            md.to_string(),
            SourceFormat::Md,
            PageLayout::Portrait,
            1,
        );
        let svg = doc.page_svg(0).unwrap();
        // Should be formatted as a table row, not a code fence
        assert!(svg.contains(" | "));
        assert!(!svg.contains("╭─"));
    }

    #[test]
    fn markdown_table_cell_and_paragraph_with_cross_character() {
        let md = "\
| Symbol | Name |
|---|---|
| `┼` | cross |

The symbol ┼ is a box-drawing cross character.
";
        let doc = TextDocument::from_string_with_options(
            md.to_string(),
            SourceFormat::Md,
            PageLayout::Portrait,
            1,
        );
        let svg = doc.page_svg(0).unwrap();
        // The table row should render with column rainbow and inline code
        assert!(svg.contains(" | "));
        assert!(svg.contains("─┼─"));
        assert!(svg.contains(r##"<tspan fill="#86efac" font-family="ui-monospace, Menlo, Monaco, Consolas, monospace">┼</tspan>"##));
        // The paragraph containing ┼ should NOT be rendered as a separator rule
        assert!(svg.contains("The symbol ┼ is a box-drawing cross character."));
    }

    #[test]
    fn markdown_table_cell_with_literal_divider_string() {
        let md = "\
| Pattern | Meaning |
|---|---|
| `a │ b` | options |
";
        let doc = TextDocument::from_string_with_options(
            md.to_string(),
            SourceFormat::Md,
            PageLayout::Portrait,
            1,
        );
        let svg = doc.page_svg(0).unwrap();
        // The inline code should contain the full unbroken string "a │ b"
        assert!(svg.contains(r##"<tspan fill="#86efac" font-family="ui-monospace, Menlo, Monaco, Consolas, monospace">a │ b</tspan>"##));
        // And the second column should still be formatted with the second color
        assert!(svg.contains(r##"<tspan fill="#34d399">"##));
        assert!(svg.contains("options"));
    }

    #[test]
    fn truncate_markdown_image_cell_respects_width() {
        let truncated = truncate_markdown_cell("![abcdef](url)", 5);
        let visual = markdown_visual_text(&truncated);
        let vlen = str_display_width(&visual);
        assert!(vlen <= 5, "visual length {vlen} exceeded width 5");
        assert_eq!(visual, "🖼 a…");

        let truncated_narrow = truncate_markdown_cell("![abcdef](url)", 3);
        assert_eq!(truncated_narrow, "…");
    }

    #[test]
    fn markdown_table_explicit_left_alignment_numeric_column() {
        let md = "\
| ID | Value |
|:---|:---|
| 1 | 100 |
| 20 | 200 |
";
        let doc = TextDocument::from_string_with_options(
            md.to_string(),
            SourceFormat::Md,
            PageLayout::Portrait,
            1,
        );
        let svg = doc.page_svg(0).unwrap();
        // Column 1 is explicitly left-aligned (:---), so "1" must be left-aligned ("1  │")
        assert!(svg.contains("1 "));
        assert!(!svg.contains(" 1 │"));
    }

    #[test]
    fn markdown_table_unmatched_backtick_does_not_swallow_pipes() {
        let row = "| a`b | second |";
        let cells = split_markdown_row(row);
        assert_eq!(cells, vec!["a`b", "second"]);

        let md = "\
| Name | Desc |
|---|---|
| a`b | second |
";
        // Mode 1: Reader
        let doc_reader = TextDocument::from_string_with_options(
            md.to_string(),
            SourceFormat::Md,
            PageLayout::Portrait,
            1,
        );
        let svg_reader = doc_reader.page_svg(0).unwrap();
        assert!(svg_reader.contains("a`b"));
        assert!(svg_reader.contains("second"));

        // Mode 0: Source
        let doc_source = TextDocument::from_string_with_options(
            md.to_string(),
            SourceFormat::Md,
            PageLayout::Portrait,
            0,
        );
        let svg_source = doc_source.page_svg(0).unwrap();
        assert!(svg_source.contains("a`b"));
        assert!(svg_source.contains("second"));
    }

    #[test]
    fn wide_tables_indicate_omitted_columns() {
        // 50 columns
        let header = (0..50)
            .map(|i| format!("Col{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let row = (0..50)
            .map(|i| format!("val{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let csv = format!("{header}\n{row}\n");

        let doc_csv =
            TextDocument::from_string_with_options(csv, SourceFormat::Csv, PageLayout::Portrait, 1);
        let svg_csv = doc_csv.page_svg(0).unwrap();
        // In portrait, max_cols is 19, so 18 columns shown + overflow indicator "+32 more"
        assert!(
            svg_csv.contains("+32 more"),
            "CSV overflow header not found in SVG"
        );
        assert!(
            svg_csv.contains("…"),
            "CSV overflow ellipsis not found in SVG"
        );

        // Markdown table with 50 columns
        let md_header = format!(
            "| {} |",
            (0..50)
                .map(|i| format!("Col{i}"))
                .collect::<Vec<_>>()
                .join(" | ")
        );
        let md_sep = format!(
            "| {} |",
            (0..50).map(|_| "---").collect::<Vec<_>>().join(" | ")
        );
        let md_row = format!(
            "| {} |",
            (0..50)
                .map(|i| format!("val{i}"))
                .collect::<Vec<_>>()
                .join(" | ")
        );
        let md = format!("{md_header}\n{md_sep}\n{md_row}\n");

        let doc_md =
            TextDocument::from_string_with_options(md, SourceFormat::Md, PageLayout::Portrait, 1);
        let svg_md = doc_md.page_svg(0).unwrap();
        assert!(
            svg_md.contains("+32 more"),
            "MD overflow header not found in SVG"
        );
        assert!(
            svg_md.contains("…"),
            "MD overflow ellipsis not found in SVG"
        );
    }

    #[test]
    fn markdown_table_truncation_preserves_inline_code_literals() {
        let md = format!(
            "| A | B |\n|---|---|\n| prefix `*literal*` {} | x |\n",
            "z".repeat(150)
        );
        let doc =
            TextDocument::from_string_with_options(md, SourceFormat::Md, PageLayout::Portrait, 1);
        let svg = doc.page_svg(0).unwrap();
        // The inline code must preserve literal asterisks and not convert to italics
        assert!(svg.contains(r##"<tspan fill="#86efac" font-family="ui-monospace, Menlo, Monaco, Consolas, monospace">*literal*</tspan>"##));
        assert!(!svg.contains(r##"font-style="italic">literal"##));
    }

    #[test]
    fn markdown_heading_with_pipes_renders_as_heading() {
        let md = "# Left | Right |\n";
        // Reader mode: should be H1 (font-size 24, #fbbf24)
        let doc_reader = TextDocument::from_string_with_options(
            md.to_string(),
            SourceFormat::Md,
            PageLayout::Portrait,
            1,
        );
        let svg_reader = doc_reader.page_svg(0).unwrap();
        assert!(svg_reader.contains(r##"font-size="24""##));
        assert!(svg_reader.contains("Left | Right"));

        // Source mode: should be H1 (#fbbf24)
        let doc_source = TextDocument::from_string_with_options(
            md.to_string(),
            SourceFormat::Md,
            PageLayout::Portrait,
            0,
        );
        let svg_source = doc_source.page_svg(0).unwrap();
        assert!(svg_source.contains(r##"fill="#fbbf24""##));
        assert!(svg_source.contains("#"));
    }

    #[test]
    fn markdown_table_truncation_nested_brackets_link() {
        let md = format!(
            "| A | B |\n|---|---|\n| [A [{}] tail](https://example.com) | y |\n",
            "x".repeat(150)
        );
        let doc =
            TextDocument::from_string_with_options(md, SourceFormat::Md, PageLayout::Portrait, 1);
        let svg = doc.page_svg(0).unwrap();
        // The link must be recognized as a link, underlined in #38bdf8, without stray unstyled "[A "
        assert!(svg.contains(r##"<tspan fill="#38bdf8" text-decoration="underline">"##));
        assert!(!svg.contains(r##">[A <"##));
    }

    #[test]
    fn markdown_code_block_with_literal_gt_does_not_render_as_blockquote() {
        let md = "```\n> **literal**\n```\n";
        let doc = TextDocument::from_string_with_options(
            md.to_string(),
            SourceFormat::Md,
            PageLayout::Portrait,
            1,
        );
        let svg = doc.page_svg(0).unwrap();
        // Should render inside monospace code span, NOT blockquote ▎
        assert!(svg.contains("&gt; **literal**"));
        assert!(!svg.contains("▎"));
    }

    #[test]
    fn markdown_table_cell_starting_with_gt_renders_as_table_not_blockquote() {
        let md = "| Col1 | Col2 |\n|---|---|\n| > **literal** | Val2 |\n";
        // Reader mode:
        let doc_reader = TextDocument::from_string_with_options(
            md.to_string(),
            SourceFormat::Md,
            PageLayout::Portrait,
            1,
        );
        let svg_reader = doc_reader.page_svg(0).unwrap();
        assert!(!svg_reader.contains("▎"));
        assert!(svg_reader.contains('|'));
        assert!(svg_reader.contains("&gt;"));

        // Source mode:
        let doc_source = TextDocument::from_string_with_options(
            md.to_string(),
            SourceFormat::Md,
            PageLayout::Portrait,
            0,
        );
        let svg_source = doc_source.page_svg(0).unwrap();
        assert!(svg_source.contains(r##"fill="#71717a">|"##));
        assert!(svg_source.contains("&gt;"));
    }

    #[test]
    fn truncate_markdown_cell_image_budget_le_3_preserves_prefix() {
        let truncated = truncate_markdown_cell("Hello ![alt](https://example.com/img.png)", 8);
        assert_eq!(truncated, "Hello …");
    }

    #[test]
    fn markdown_nested_blockquotes_strip_all_markers() {
        let md = ">> nested quote\n> > spaced nested quote\n";
        let doc = TextDocument::from_string_with_options(
            md.to_string(),
            SourceFormat::Md,
            PageLayout::Portrait,
            1,
        );
        let svg = doc.page_svg(0).unwrap();
        // In reader mode, should strip all leading quote markers
        assert!(svg.contains("nested quote"));
        assert!(svg.contains("spaced nested quote"));
        assert!(!svg.contains("&gt;"));
    }

    #[test]
    fn markdown_list_item_with_pipes_not_absorbed_into_table() {
        let md = "| A | B |\n|---|---|\n| x | y |\n- unrelated |\n";
        let doc = TextDocument::from_string_with_options(
            md.to_string(),
            SourceFormat::Md,
            PageLayout::Portrait,
            1,
        );
        let svg = doc.page_svg(0).unwrap();
        // The list item must be rendered as a bullet item, not aligned table row
        assert!(svg.contains("• "));
        assert!(svg.contains("unrelated |"));

        // Header with list syntax should not be treated as table header
        let md_list_hdr = "- keep | this |\n|---|---|\n| x | y |\n";
        let doc2 = TextDocument::from_string_with_options(
            md_list_hdr.to_string(),
            SourceFormat::Md,
            PageLayout::Portrait,
            1,
        );
        let svg2 = doc2.page_svg(0).unwrap();
        assert!(svg2.contains("• "));
        assert!(svg2.contains("keep | this"));
    }

    #[test]
    fn markdown_wrapped_blockquote_preserves_literal_gt_in_continuation() {
        let md = format!("> {}{}", "a".repeat(116), "> comparison\n");
        let doc =
            TextDocument::from_string_with_options(md, SourceFormat::Md, PageLayout::Portrait, 1);
        let svg = doc.page_svg(0).unwrap();
        // The literal '>' wrapped into the continuation chunk must be preserved (&gt; in XML)
        assert!(svg.contains("&gt; comparison"));
    }

    #[test]
    fn markdown_table_row_trailing_escaped_pipe_preserved() {
        let md = "| A | B |\n|---|---|\n| a | b\\|\n";
        let doc = TextDocument::from_string_with_options(
            md.to_string(),
            SourceFormat::Md,
            PageLayout::Portrait,
            1,
        );
        let svg = doc.page_svg(0).unwrap();
        // The escaped pipe at the end of the line must be preserved in reader mode cell
        assert!(svg.contains("b|"));
        assert!(!svg.contains(r"b\"));
    }

    #[test]
    fn markdown_source_table_empty_cell_advances_column_palette() {
        let md = "| A || C |\n|---|---|---|\n| a || c |\n";
        let doc = TextDocument::from_string_with_options(
            md.to_string(),
            SourceFormat::Md,
            PageLayout::Portrait,
            0,
        );
        let svg = doc.page_svg(0).unwrap();
        // Column 0 is CSV_PALETTE[0], Column 1 is empty, Column 2 is CSV_PALETTE[2]
        let col0_color = CSV_PALETTE[0];
        let col2_color = CSV_PALETTE[2];
        assert!(svg.contains(&format!(r##"<tspan fill="{col0_color}"> A </tspan>"##)));
        assert!(svg.contains(&format!(r##"<tspan fill="{col2_color}"> C </tspan>"##)));
        assert!(svg.contains(&format!(r##"<tspan fill="{col0_color}"> a </tspan>"##)));
        assert!(svg.contains(&format!(r##"<tspan fill="{col2_color}"> c </tspan>"##)));
    }

    #[test]
    fn markdown_table_row_escaped_backslashes_before_delimiter() {
        let md = "| C:\\\\| value |\n|---|---|\n| a | b |\n";
        // Reader mode: should have 2 columns ("C:\" and "value")
        let doc_reader = TextDocument::from_string_with_options(
            md.to_string(),
            SourceFormat::Md,
            PageLayout::Portrait,
            1,
        );
        let svg_reader = doc_reader.page_svg(0).unwrap();
        assert!(svg_reader.contains("C:\\"));
        assert!(svg_reader.contains("value"));

        // Source mode: delimiter pipe must be recognized
        let doc_source = TextDocument::from_string_with_options(
            md.to_string(),
            SourceFormat::Md,
            PageLayout::Portrait,
            0,
        );
        let svg_source = doc_source.page_svg(0).unwrap();
        let col0_color = CSV_PALETTE[0];
        let col1_color = CSV_PALETTE[1];
        assert!(svg_source.contains(&format!(r##"<tspan fill="{col0_color}"> C:\\</tspan>"##)));
        assert!(svg_source.contains(&format!(r##"<tspan fill="{col1_color}"> value </tspan>"##)));
    }

    #[test]
    fn markdown_table_with_bold_cell_renders_aligned() {
        let md = "\
| # | Página | Verbosidad |
|---|---|---|
| raíz | **Onboarding: Card Platform International University** | narrativa |
| 1 | Conocer Affirm y el equipo CPI | narrativa |
";
        let doc_reader = TextDocument::from_string_with_options(
            md.to_string(),
            SourceFormat::Md,
            PageLayout::Landscape,
            1,
        );
        let svg = doc_reader.page_svg(0).unwrap();

        // Bold text in Column 1 (Página) must use the column's palette color (CSV_PALETTE[1] = #34d399 / green)
        assert!(svg.contains(r##"fill="#34d399" font-weight="bold">Onboarding: Card Platform"##));
        // Must use ASCII pipe divider to prevent font fallback in bold rows
        assert!(svg.contains(r##"fill="#3f3f46"> | </tspan>"##));
        assert!(svg.contains("─┼─"));

        // Render to pixmap must succeed without errors
        let img = doc_reader
            .render_fit(0)
            .expect("Rendering page with bold table cells must succeed");
        assert!(img.width > 0 && img.height > 0);

        // Source mode must also render bold text in column palette color (CSV_PALETTE[1] = #34d399 / green)
        let doc_source = TextDocument::from_string_with_options(
            md.to_string(),
            SourceFormat::Md,
            PageLayout::Landscape,
            0,
        );
        let svg_source = doc_source.page_svg(0).unwrap();
        assert!(
            svg_source.contains(r##"fill="#34d399" font-weight="bold">Onboarding: Card Platform"##)
        );
        assert!(svg_source.contains(r##"fill="#71717a">**</tspan>"##));
    }
}
