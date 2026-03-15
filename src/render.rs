use anyhow::{Context, Result};
use crossterm::ExecutableCommand;
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use pulldown_cmark::{Event, Options, Parser, Tag};
use std::io::{self, Write};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::{LinesWithEndings, as_24_bit_terminal_escaped};

use crate::config::InlineColors;

/// Parse a color name or hex string into a crossterm Color.
///
/// Supports:
/// - Named colors: "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white", "grey"/"gray"
/// - Hex colors: "#RRGGBB" format
///
/// Returns None if the color cannot be parsed.
fn parse_color(color_name: &str) -> Option<Color> {
    match color_name.to_lowercase().as_str() {
        "black" => Some(Color::Black),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "yellow" => Some(Color::Yellow),
        "blue" => Some(Color::Blue),
        "magenta" => Some(Color::Magenta),
        "cyan" => Some(Color::Cyan),
        "white" => Some(Color::White),
        "grey" | "gray" => Some(Color::Grey),
        // RGB hex format: #RRGGBB
        hex if hex.starts_with('#') && hex.len() == 7 => {
            let r = u8::from_str_radix(&hex[1..3], 16).ok()?;
            let g = u8::from_str_radix(&hex[3..5], 16).ok()?;
            let b = u8::from_str_radix(&hex[5..7], 16).ok()?;
            Some(Color::Rgb { r, g, b })
        }
        _ => None,
    }
}

impl InlineColors {
    /// Get the color for headings.
    pub fn get_heading_color(&self) -> Color {
        self.heading
            .as_ref()
            .and_then(|c| parse_color(c))
            .unwrap_or(Color::Cyan)
    }

    /// Get the color for inline code.
    pub fn get_inline_code_color(&self) -> Color {
        self.inline_code
            .as_ref()
            .and_then(|c| parse_color(c))
            .unwrap_or(Color::Yellow)
    }

    /// Get the color for emphasis (bold/italic).
    pub fn get_emphasis_color(&self) -> Color {
        self.emphasis
            .as_ref()
            .and_then(|c| parse_color(c))
            .unwrap_or(Color::White)
    }

    /// Get the color for chat prompt.
    pub fn get_prompt_color(&self) -> Color {
        self.prompt
            .as_ref()
            .and_then(|c| parse_color(c))
            .unwrap_or(Color::Cyan)
    }
}

/// Markdown renderer with syntax highlighting support.
///
/// Uses `pulldown-cmark` for markdown parsing and `syntect` for syntax highlighting
/// of code blocks. Supports headings, paragraphs, lists, emphasis, inline code,
/// and fenced code blocks with language-specific highlighting.
pub struct MarkdownRenderer {
    syntax_set: SyntaxSet,
    theme_set: ThemeSet,
    theme_name: String,
    inline_colors: InlineColors,
}

impl MarkdownRenderer {
    /// Create a new markdown renderer with default settings.
    ///
    /// Initializes the syntax set and theme set from syntect's default bundles.
    /// Uses the "base16-ocean.dark" theme for code highlighting.
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self::with_theme("base16-ocean.dark", InlineColors::default())
    }

    /// Create a markdown renderer with a custom theme and inline colors.
    ///
    /// # Arguments
    /// * `theme_name` - Name of the syntect theme (e.g., "base16-ocean.dark", "InspiredGitHub")
    /// * `inline_colors` - Custom colors for inline markdown elements
    ///
    /// If the theme doesn't exist, falls back to "base16-ocean.dark" with a warning.
    pub fn with_theme(theme_name: &str, inline_colors: InlineColors) -> Self {
        let theme_set = ThemeSet::load_defaults();

        // Validate theme exists, fallback to default if not
        let validated_theme = if theme_set.themes.contains_key(theme_name) {
            theme_name.to_string()
        } else {
            eprintln!(
                "Warning: Theme '{}' not found, using 'base16-ocean.dark'",
                theme_name
            );
            "base16-ocean.dark".to_string()
        };

        Self {
            syntax_set: SyntaxSet::load_defaults_newlines(),
            theme_set,
            theme_name: validated_theme,
            inline_colors,
        }
    }

    /// Render a complete markdown text to stdout.
    ///
    /// Parses the markdown and renders it with:
    /// - Cyan colored headings
    /// - Yellow inline code
    /// - Syntax highlighted code blocks
    /// - Bullet points for lists
    /// - Proper spacing between elements
    ///
    /// # Errors
    /// Returns an error if terminal output fails or syntax highlighting fails.
    pub fn render(&self, markdown: &str) -> Result<()> {
        let mut stdout = io::stdout();
        let mut options = Options::empty();
        options.insert(Options::ENABLE_STRIKETHROUGH);
        options.insert(Options::ENABLE_TABLES);

        let parser = Parser::new_ext(markdown, options);

        let mut in_code_block = false;
        let mut code_block_lang = String::new();
        let mut code_block_content = String::new();
        let mut in_heading = false;

        for event in parser {
            match event {
                Event::Start(Tag::CodeBlock(kind)) => {
                    in_code_block = true;
                    code_block_content.clear();

                    // Extract language from code block
                    code_block_lang = match kind {
                        pulldown_cmark::CodeBlockKind::Fenced(lang) => lang.to_string(),
                        pulldown_cmark::CodeBlockKind::Indented => String::new(),
                    };
                }
                Event::End(Tag::CodeBlock(_)) => {
                    if in_code_block {
                        self.render_code_block(&code_block_content, &code_block_lang)?;
                        in_code_block = false;
                        code_block_content.clear();
                    }
                }
                Event::Text(text) => {
                    if in_code_block {
                        code_block_content.push_str(&text);
                    } else if in_heading {
                        // Render headings in configured color
                        stdout
                            .execute(SetForegroundColor(self.inline_colors.get_heading_color()))?
                            .execute(Print(&*text))?
                            .execute(ResetColor)?;
                    } else {
                        print!("{}", text);
                    }
                }
                Event::Code(code) => {
                    // Inline code in configured color
                    stdout
                        .execute(SetForegroundColor(
                            self.inline_colors.get_inline_code_color(),
                        ))?
                        .execute(Print("`"))?
                        .execute(Print(&*code))?
                        .execute(Print("`"))?
                        .execute(ResetColor)?;
                }
                Event::Start(Tag::Heading(..)) => {
                    in_heading = true;
                }
                Event::End(Tag::Heading(..)) => {
                    in_heading = false;
                    println!(); // Newline after heading
                }
                Event::Start(Tag::Paragraph) => {}
                Event::End(Tag::Paragraph) => {
                    println!(); // Blank line after paragraph
                }
                Event::Start(Tag::List(_)) => {}
                Event::End(Tag::List(_)) => {
                    println!(); // Blank line after list
                }
                Event::Start(Tag::Item) => {
                    print!("  • "); // Bullet point
                }
                Event::End(Tag::Item) => {
                    println!();
                }
                Event::Start(Tag::Emphasis) => {
                    stdout.execute(SetForegroundColor(self.inline_colors.get_emphasis_color()))?;
                }
                Event::End(Tag::Emphasis) => {
                    stdout.execute(ResetColor)?;
                }
                Event::Start(Tag::Strong) => {
                    stdout.execute(SetForegroundColor(self.inline_colors.get_emphasis_color()))?;
                }
                Event::End(Tag::Strong) => {
                    stdout.execute(ResetColor)?;
                }
                Event::SoftBreak => {
                    print!(" ");
                }
                Event::HardBreak => {
                    println!();
                }
                Event::Rule => {
                    println!("────────────────────────────────────────");
                }
                _ => {}
            }
        }

        stdout.flush()?;
        Ok(())
    }

    /// Render a code block with syntax highlighting.
    ///
    /// Attempts to find a syntax definition for the specified language.
    /// If found, applies syntax highlighting using the configured theme.
    /// Falls back to plain text rendering if the language is unknown.
    ///
    /// # Arguments
    /// * `code` - The code content to render
    /// * `lang` - The language identifier (e.g., "rust", "python", "javascript")
    fn render_code_block(&self, code: &str, lang: &str) -> Result<()> {
        let theme = &self.theme_set.themes[&self.theme_name];

        // Try to find syntax for the language
        let syntax = if !lang.is_empty() {
            self.syntax_set
                .find_syntax_by_token(lang)
                .or_else(|| self.syntax_set.find_syntax_by_extension(lang))
        } else {
            None
        };

        if let Some(syntax) = syntax {
            // Render with syntax highlighting
            let mut highlighter = HighlightLines::new(syntax, theme);

            for line in LinesWithEndings::from(code) {
                let ranges: Vec<(Style, &str)> = highlighter
                    .highlight_line(line, &self.syntax_set)
                    .context("Failed to highlight line")?;
                let escaped = as_24_bit_terminal_escaped(&ranges[..], false);
                print!("{}", escaped);
            }
            println!("\x1b[0m"); // Reset color
        } else {
            // No syntax highlighting available, print as-is
            print!("{}", code);
            if !code.ends_with('\n') {
                println!();
            }
        }

        Ok(())
    }
}

/// Render state for streaming markdown with progressive output.
///
/// Enables true streaming behavior where:
/// - Plain text is rendered with inline markdown formatting applied in real-time
/// - Code blocks are buffered and rendered with syntax highlighting when complete
/// - Markdown tables are buffered as blocks and rendered with aligned columns
/// - Fence markers (``` or ~~~) are detected automatically
/// - Inline formatting (bold, italic, inline code, headers) is applied during streaming
///
/// When `plain_text_mode` is enabled (e.g., when output is piped to a file),
/// all markdown formatting and syntax highlighting is disabled for clean output.
pub struct StreamRenderer {
    renderer: MarkdownRenderer,
    buffer: String,
    in_code_block: bool,
    active_fence: Option<FenceMarker>,
    line_buffer: String,
    pending_table_header: Option<PendingTableHeader>,
    active_table_block: Option<ActiveTableBlock>,
    inline_colors: InlineColors,
    plain_text_mode: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FenceMarker {
    ch: char,
    len: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TableAlignment {
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone)]
struct PendingTableHeader {
    raw_line: String,
    indent: String,
    cells: Vec<String>,
}

#[derive(Debug, Clone)]
struct ActiveTableBlock {
    indent: String,
    headers: Vec<String>,
    alignments: Vec<TableAlignment>,
    rows: Vec<Vec<String>>,
}

impl StreamRenderer {
    /// Create a new stream renderer with default theme
    pub fn new() -> Self {
        Self::with_theme("base16-ocean.dark", InlineColors::default())
    }

    /// Create a stream renderer in plain text mode (no formatting or highlighting)
    ///
    /// Use this when output is piped to a file or non-terminal destination.
    /// All markdown formatting and ANSI color codes are disabled.
    pub fn with_plain_text() -> Self {
        Self {
            renderer: MarkdownRenderer::new(),
            buffer: String::new(),
            in_code_block: false,
            active_fence: None,
            line_buffer: String::new(),
            pending_table_header: None,
            active_table_block: None,
            inline_colors: InlineColors::default(),
            plain_text_mode: true,
        }
    }

    /// Create a stream renderer with a custom theme and inline colors.
    ///
    /// # Arguments
    /// * `theme_name` - Name of the syntect theme for code blocks
    /// * `inline_colors` - Custom colors for inline markdown elements
    pub fn with_theme(theme_name: &str, inline_colors: InlineColors) -> Self {
        Self {
            renderer: MarkdownRenderer::with_theme(theme_name, inline_colors.clone()),
            buffer: String::new(),
            in_code_block: false,
            active_fence: None,
            line_buffer: String::new(),
            pending_table_header: None,
            active_table_block: None,
            inline_colors,
            plain_text_mode: false,
        }
    }

    /// Add a chunk of text and render progressively.
    ///
    /// Processes text character-by-character, accumulating lines. When a complete
    /// line is received:
    /// - Code fence markers (``` or ~~~) are detected to track code block boundaries
    /// - Inside code blocks: text is buffered for later syntax highlighting
    /// - Markdown tables are buffered until complete so columns can be aligned
    /// - Outside code/table blocks: lines are rendered with markdown formatting applied
    ///
    /// In plain text mode, all text is printed as-is without any processing.
    ///
    /// # Arguments
    /// * `chunk` - The text chunk to process (typically from a streaming API response)
    ///
    /// # Errors
    /// Returns an error if terminal output fails or markdown rendering fails.
    pub fn add_chunk(&mut self, chunk: &str) -> Result<()> {
        // In plain text mode, just print everything as-is
        if self.plain_text_mode {
            print!("{}", chunk);
            io::stdout().flush()?;
            return Ok(());
        }

        for ch in chunk.chars() {
            self.line_buffer.push(ch);

            // Check if we have a complete line
            if ch == '\n' {
                let line = std::mem::take(&mut self.line_buffer);

                if self.in_code_block {
                    if is_matching_fence_closer(&line, self.active_fence) {
                        // Ending a code block - add the closing fence and render
                        self.buffer.push_str(&line);
                        self.renderer.render(&self.buffer)?;
                        self.buffer.clear();
                        self.in_code_block = false;
                        self.active_fence = None;
                    } else {
                        // Code inside the block - keep buffering
                        self.buffer.push_str(&line);
                    }
                } else if let Some(fence) = parse_fence_opener(&line) {
                    self.flush_pending_tables()?;
                    // Starting a code block
                    self.in_code_block = true;
                    self.active_fence = Some(fence);
                    self.buffer.push_str(&line);
                } else {
                    // Regular text - render markdown inline/block elements for this line
                    self.process_regular_line(line)?;
                }
            }
        }

        Ok(())
    }

    fn process_regular_line(&mut self, line: String) -> Result<()> {
        let mut stdout = io::stdout();
        self.process_regular_line_to(line, &mut stdout)?;
        stdout.flush()?;
        Ok(())
    }

    fn process_regular_line_to<W: Write>(&mut self, line: String, out: &mut W) -> Result<()> {
        loop {
            if let Some(active_table) = self.active_table_block.as_mut() {
                if let Some((indent, cells)) = parse_table_row(&line) {
                    if indent == active_table.indent && cells.len() == active_table.headers.len() {
                        active_table.rows.push(cells);
                        return Ok(());
                    }
                }

                if let Some(table) = self.active_table_block.take() {
                    self.render_table_block_to(&table, out)?;
                }
            }

            if let Some(pending_header) = self.pending_table_header.take() {
                if let Some((indent, alignments)) =
                    parse_table_separator(&line, pending_header.cells.len())
                {
                    if indent == pending_header.indent {
                        self.active_table_block = Some(ActiveTableBlock {
                            indent: pending_header.indent,
                            headers: pending_header.cells,
                            alignments,
                            rows: Vec::new(),
                        });
                        return Ok(());
                    }
                }

                self.render_line_with_inline_formatting_to(&pending_header.raw_line, out)?;
                continue;
            }

            if let Some(header) = parse_table_header_candidate(&line) {
                self.pending_table_header = Some(header);
                return Ok(());
            }

            self.render_line_with_inline_formatting_to(&line, out)?;
            return Ok(());
        }
    }

    fn render_line_with_inline_formatting_to<W: Write>(
        &self,
        line: &str,
        out: &mut W,
    ) -> Result<()> {
        let (content, has_trailing_newline) = strip_single_trailing_newline(line);

        let mut options = Options::empty();
        options.insert(Options::ENABLE_STRIKETHROUGH);
        options.insert(Options::ENABLE_TABLES);
        let parser = Parser::new_ext(content, options);

        let mut list_stack: Vec<Option<u64>> = Vec::new();
        let mut link_dest_stack: Vec<String> = Vec::new();

        for event in parser {
            match event {
                Event::Start(Tag::Heading(..)) => {
                    out.execute(SetForegroundColor(self.inline_colors.get_heading_color()))?
                        .execute(SetAttribute(Attribute::Bold))?;
                }
                Event::End(Tag::Heading(..)) => {
                    out.execute(SetAttribute(Attribute::Reset))?
                        .execute(ResetColor)?;
                }
                Event::Start(Tag::Strong) => {
                    out.execute(SetForegroundColor(self.inline_colors.get_emphasis_color()))?
                        .execute(SetAttribute(Attribute::Bold))?;
                }
                Event::End(Tag::Strong) => {
                    out.execute(SetAttribute(Attribute::Reset))?
                        .execute(ResetColor)?;
                }
                Event::Start(Tag::Emphasis) => {
                    out.execute(SetForegroundColor(self.inline_colors.get_emphasis_color()))?
                        .execute(SetAttribute(Attribute::Italic))?;
                }
                Event::End(Tag::Emphasis) => {
                    out.execute(SetAttribute(Attribute::Reset))?
                        .execute(ResetColor)?;
                }
                Event::Start(Tag::Strikethrough) => {
                    out.execute(SetForegroundColor(self.inline_colors.get_emphasis_color()))?
                        .execute(SetAttribute(Attribute::CrossedOut))?;
                }
                Event::End(Tag::Strikethrough) => {
                    out.execute(SetAttribute(Attribute::Reset))?
                        .execute(ResetColor)?;
                }
                Event::Start(Tag::Link(_, destination, _)) => {
                    link_dest_stack.push(destination.to_string());
                    out.execute(SetForegroundColor(self.inline_colors.get_heading_color()))?
                        .execute(SetAttribute(Attribute::Underlined))?;
                }
                Event::End(Tag::Link(..)) => {
                    out.execute(SetAttribute(Attribute::Reset))?
                        .execute(ResetColor)?;

                    if let Some(destination) = link_dest_stack.pop() {
                        if !destination.is_empty() {
                            out.execute(Print(format!(" ({})", destination)))?;
                        }
                    }
                }
                Event::Start(Tag::BlockQuote) => {
                    out.execute(SetForegroundColor(self.inline_colors.get_emphasis_color()))?
                        .execute(Print("│ "))?;
                }
                Event::End(Tag::BlockQuote) => {
                    out.execute(ResetColor)?;
                }
                Event::Start(Tag::List(start)) => {
                    list_stack.push(start);
                }
                Event::End(Tag::List(_)) => {
                    list_stack.pop();
                }
                Event::Start(Tag::Item) => {
                    let depth = list_stack.len().saturating_sub(1);
                    if depth > 0 {
                        out.execute(Print("  ".repeat(depth)))?;
                    }

                    if let Some(last) = list_stack.last_mut() {
                        match last {
                            Some(next_index) => {
                                out.execute(Print(format!("{}. ", *next_index)))?;
                                *next_index += 1;
                            }
                            None => {
                                out.execute(Print("• "))?;
                            }
                        }
                    } else {
                        out.execute(Print("• "))?;
                    }
                }
                Event::TaskListMarker(checked) => {
                    out.execute(Print(if checked { "[x] " } else { "[ ] " }))?;
                }
                Event::Text(text) | Event::Html(text) => {
                    out.execute(Print(&*text))?;
                }
                Event::Code(code) => {
                    out.execute(SetForegroundColor(
                        self.inline_colors.get_inline_code_color(),
                    ))?
                    .execute(Print("`"))?
                    .execute(Print(&*code))?
                    .execute(Print("`"))?
                    .execute(ResetColor)?;
                }
                Event::SoftBreak => {
                    out.execute(Print(" "))?;
                }
                Event::HardBreak => {
                    out.execute(Print("\n"))?;
                }
                Event::Rule => {
                    out.execute(Print("────────────────────────────────────────"))?;
                }
                _ => {}
            }
        }

        if has_trailing_newline {
            out.execute(Print("\n"))?;
        }

        Ok(())
    }

    fn flush_pending_tables(&mut self) -> Result<()> {
        let mut stdout = io::stdout();
        self.flush_pending_tables_to(&mut stdout)?;
        stdout.flush()?;
        Ok(())
    }

    fn flush_pending_tables_to<W: Write>(&mut self, out: &mut W) -> Result<()> {
        if let Some(table) = self.active_table_block.take() {
            self.render_table_block_to(&table, out)?;
        }

        if let Some(pending_header) = self.pending_table_header.take() {
            self.render_line_with_inline_formatting_to(&pending_header.raw_line, out)?;
        }

        Ok(())
    }

    fn render_table_block_to<W: Write>(&self, table: &ActiveTableBlock, out: &mut W) -> Result<()> {
        let column_count = table
            .headers
            .len()
            .max(table.alignments.len())
            .max(table.rows.iter().map(|row| row.len()).max().unwrap_or(0));

        if column_count == 0 {
            return Ok(());
        }

        let mut widths = vec![0usize; column_count];
        for (idx, cell) in table.headers.iter().enumerate() {
            widths[idx] = widths[idx].max(cell.chars().count());
        }
        for row in &table.rows {
            for (idx, cell) in row.iter().enumerate() {
                widths[idx] = widths[idx].max(cell.chars().count());
            }
        }

        out.execute(Print(table_border_line(
            &table.indent,
            &widths,
            '┌',
            '┬',
            '┐',
        )))?;
        out.execute(Print("\n"))?;
        out.execute(Print(table_row_line(
            &table.indent,
            &table.headers,
            &widths,
            &table.alignments,
        )))?;
        out.execute(Print("\n"))?;
        out.execute(Print(table_border_line(
            &table.indent,
            &widths,
            '├',
            '┼',
            '┤',
        )))?;
        out.execute(Print("\n"))?;

        for row in &table.rows {
            out.execute(Print(table_row_line(
                &table.indent,
                row,
                &widths,
                &table.alignments,
            )))?;
            out.execute(Print("\n"))?;
        }

        out.execute(Print(table_border_line(
            &table.indent,
            &widths,
            '└',
            '┴',
            '┘',
        )))?;
        out.execute(Print("\n"))?;
        Ok(())
    }

    /// Flush any remaining buffered content.
    ///
    /// Called at the end of streaming to ensure all content is rendered.
    /// Handles incomplete lines and code blocks that may still be in the buffer.
    ///
    /// # Errors
    /// Returns an error if terminal output fails.
    pub fn flush(&mut self) -> Result<()> {
        // In plain text mode, just print any remaining buffer
        if self.plain_text_mode {
            if !self.line_buffer.is_empty() {
                print!("{}", self.line_buffer);
                self.line_buffer.clear();
            }
            io::stdout().flush()?;
            return Ok(());
        }

        // Flush any remaining line buffer
        if !self.line_buffer.is_empty() {
            if self.in_code_block {
                self.buffer.push_str(&self.line_buffer);
                self.line_buffer.clear();
            } else {
                // Process remaining regular line (may finalize a pending table)
                let line = std::mem::take(&mut self.line_buffer);
                self.process_regular_line(line)?;
            }
        }

        // Render any remaining buffered code block
        if !self.buffer.is_empty() {
            self.renderer.render(&self.buffer)?;
            self.buffer.clear();
            self.in_code_block = false;
            self.active_fence = None;
        }

        self.flush_pending_tables()?;

        Ok(())
    }

    /// Get the accumulated buffer (for testing)
    #[allow(dead_code)]
    pub fn get_buffer(&self) -> &str {
        &self.buffer
    }
}

impl Default for StreamRenderer {
    fn default() -> Self {
        Self::new()
    }
}

fn strip_single_trailing_newline(line: &str) -> (&str, bool) {
    if let Some(stripped) = line.strip_suffix("\r\n") {
        (stripped, true)
    } else if let Some(stripped) = line.strip_suffix('\n') {
        (stripped, true)
    } else {
        (line, false)
    }
}

fn leading_markdown_indent(line: &str) -> Option<usize> {
    let mut indent = 0usize;
    for (idx, ch) in line.char_indices() {
        match ch {
            ' ' => {
                indent += 1;
                if indent > 3 {
                    return None;
                }
            }
            '\t' => {
                return None;
            }
            _ => return Some(idx),
        }
    }
    Some(line.len())
}

fn parse_fence_sequence(line: &str) -> Option<(char, usize, &str)> {
    let (without_newline, _) = strip_single_trailing_newline(line);
    let content = without_newline
        .strip_suffix('\r')
        .unwrap_or(without_newline);

    let start = leading_markdown_indent(content)?;
    let mut chars = content[start..].chars();
    let marker = chars.next()?;
    if marker != '`' && marker != '~' {
        return None;
    }

    let mut len = 1usize;
    let mut bytes_consumed = marker.len_utf8();
    for ch in chars {
        if ch == marker {
            len += 1;
            bytes_consumed += ch.len_utf8();
        } else {
            break;
        }
    }

    if len < 3 {
        return None;
    }

    let remainder = &content[start + bytes_consumed..];
    Some((marker, len, remainder))
}

fn parse_fence_opener(line: &str) -> Option<FenceMarker> {
    let (ch, len, _) = parse_fence_sequence(line)?;
    Some(FenceMarker { ch, len })
}

fn parse_table_header_candidate(line: &str) -> Option<PendingTableHeader> {
    let (indent, cells) = parse_table_row(line)?;
    if cells.len() < 2 {
        return None;
    }

    Some(PendingTableHeader {
        raw_line: line.to_string(),
        indent,
        cells,
    })
}

fn parse_table_separator(
    line: &str,
    expected_cols: usize,
) -> Option<(String, Vec<TableAlignment>)> {
    let (indent, cells) = parse_table_row(line)?;
    if cells.len() != expected_cols {
        return None;
    }

    let alignments = cells
        .iter()
        .map(|cell| parse_table_alignment(cell))
        .collect::<Option<Vec<_>>>()?;

    Some((indent, alignments))
}

fn parse_table_row(line: &str) -> Option<(String, Vec<String>)> {
    let (without_newline, _) = strip_single_trailing_newline(line);
    let content = without_newline
        .strip_suffix('\r')
        .unwrap_or(without_newline);
    let prefix_len = content.len() - content.trim_start().len();
    let indent = content[..prefix_len].to_string();
    let trimmed = content.trim();

    if !trimmed.starts_with('|') || !trimmed.ends_with('|') {
        return None;
    }

    let inner = &trimmed[1..trimmed.len() - 1];
    let cells = split_table_cells(inner);
    if cells.is_empty() {
        return None;
    }

    Some((indent, cells))
}

fn split_table_cells(inner: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut current = String::new();
    let mut chars = inner.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\\' && matches!(chars.peek(), Some('|')) {
            current.push('|');
            chars.next();
            continue;
        }

        if ch == '|' {
            cells.push(current.trim().to_string());
            current.clear();
        } else {
            current.push(ch);
        }
    }

    cells.push(current.trim().to_string());
    cells
}

fn parse_table_alignment(cell: &str) -> Option<TableAlignment> {
    let compact: String = cell.chars().filter(|ch| !ch.is_whitespace()).collect();
    if compact.is_empty() {
        return None;
    }

    let starts_colon = compact.starts_with(':');
    let ends_colon = compact.ends_with(':');
    let dashes = compact.trim_matches(':');
    if dashes.len() < 3 || !dashes.chars().all(|ch| ch == '-') {
        return None;
    }

    let alignment = match (starts_colon, ends_colon) {
        (true, true) => TableAlignment::Center,
        (false, true) => TableAlignment::Right,
        _ => TableAlignment::Left,
    };
    Some(alignment)
}

fn table_border_line(
    indent: &str,
    widths: &[usize],
    left: char,
    middle: char,
    right: char,
) -> String {
    let mut out = String::new();
    out.push_str(indent);
    out.push(left);

    for (idx, width) in widths.iter().enumerate() {
        out.push_str(&"─".repeat(*width + 2));
        if idx + 1 < widths.len() {
            out.push(middle);
        }
    }

    out.push(right);
    out
}

fn table_row_line(
    indent: &str,
    cells: &[String],
    widths: &[usize],
    alignments: &[TableAlignment],
) -> String {
    let mut out = String::new();
    out.push_str(indent);
    out.push('│');

    for (idx, width) in widths.iter().enumerate() {
        let cell = cells.get(idx).map(|s| s.as_str()).unwrap_or("");
        let alignment = alignments.get(idx).copied().unwrap_or(TableAlignment::Left);
        out.push(' ');
        out.push_str(&padded_table_cell(cell, *width, alignment));
        out.push(' ');
        out.push('│');
    }

    out
}

fn padded_table_cell(content: &str, width: usize, alignment: TableAlignment) -> String {
    let content_width = content.chars().count();
    if content_width >= width {
        return content.to_string();
    }

    let pad = width - content_width;
    match alignment {
        TableAlignment::Right => format!("{}{}", " ".repeat(pad), content),
        TableAlignment::Center => {
            let left = pad / 2;
            let right = pad - left;
            format!("{}{}{}", " ".repeat(left), content, " ".repeat(right))
        }
        TableAlignment::Left => format!("{}{}", content, " ".repeat(pad)),
    }
}

fn is_matching_fence_closer(line: &str, active_fence: Option<FenceMarker>) -> bool {
    let Some(active) = active_fence else {
        return false;
    };

    let Some((ch, len, remainder)) = parse_fence_sequence(line) else {
        return false;
    };

    if ch != active.ch || len < active.len {
        return false;
    }

    remainder.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::InlineColors;

    fn strip_ansi(input: &str) -> String {
        let mut out = String::new();
        let mut chars = input.chars().peekable();

        while let Some(ch) = chars.next() {
            if ch == '\x1b' && matches!(chars.peek(), Some('[')) {
                chars.next(); // Skip '['
                for c in chars.by_ref() {
                    // End of CSI sequence
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            } else {
                out.push(ch);
            }
        }

        out
    }

    #[test]
    fn test_stream_renderer_accumulates_chunks() {
        let mut renderer = StreamRenderer::new();
        // For plain text without newlines, content goes to line_buffer
        renderer.add_chunk("Hello ").unwrap();
        renderer.add_chunk("World\n").unwrap();
        // After a newline, non-code content is printed immediately
        // So buffer should be empty
        assert_eq!(renderer.get_buffer(), "");
    }

    #[test]
    fn test_markdown_renderer_creation() {
        let renderer = MarkdownRenderer::new();
        // Just ensure it doesn't panic
        assert!(!renderer.syntax_set.syntaxes().is_empty());
    }

    #[test]
    fn test_markdown_renderer_with_theme() {
        // Test creating renderer with valid theme
        let renderer = MarkdownRenderer::with_theme("InspiredGitHub", InlineColors::default());
        assert_eq!(renderer.theme_name, "InspiredGitHub");

        // Test with invalid theme - should fallback to default
        let renderer2 = MarkdownRenderer::with_theme("nonexistent", InlineColors::default());
        assert_eq!(renderer2.theme_name, "base16-ocean.dark");
    }

    #[test]
    fn test_parse_color_named() {
        assert!(matches!(parse_color("red"), Some(Color::Red)));
        assert!(matches!(parse_color("blue"), Some(Color::Blue)));
        assert!(matches!(parse_color("cyan"), Some(Color::Cyan)));
        assert!(matches!(parse_color("YELLOW"), Some(Color::Yellow))); // Case insensitive
        assert!(matches!(parse_color("invalid"), None));
    }

    #[test]
    fn test_parse_color_hex() {
        // Valid hex color
        if let Some(Color::Rgb { r, g, b }) = parse_color("#FF8800") {
            assert_eq!(r, 255);
            assert_eq!(g, 136);
            assert_eq!(b, 0);
        } else {
            panic!("Expected RGB color");
        }

        // Invalid hex
        assert!(matches!(parse_color("#GGGGGG"), None));
        assert!(matches!(parse_color("#FF"), None)); // Too short
    }

    #[test]
    fn test_inline_colors_defaults() {
        let colors = InlineColors::default();
        assert_eq!(colors.get_heading_color(), Color::Cyan);
        assert_eq!(colors.get_inline_code_color(), Color::Yellow);
        assert_eq!(colors.get_emphasis_color(), Color::White);
    }

    #[test]
    fn test_inline_colors_custom() {
        let colors = InlineColors {
            heading: Some("blue".to_string()),
            inline_code: Some("#FF8800".to_string()),
            emphasis: Some("magenta".to_string()),
            prompt: Some("green".to_string()),
        };

        assert_eq!(colors.get_heading_color(), Color::Blue);
        assert!(matches!(colors.get_inline_code_color(), Color::Rgb { .. }));
        assert_eq!(colors.get_emphasis_color(), Color::Magenta);
        assert_eq!(colors.get_prompt_color(), Color::Green);
    }

    #[test]
    fn test_stream_renderer_with_theme() {
        let colors = InlineColors::for_theme("InspiredGitHub");
        let renderer = StreamRenderer::with_theme("Solarized (light)", colors);
        assert_eq!(renderer.renderer.theme_name, "Solarized (light)");
    }

    #[test]
    fn test_plain_text_renderer() {
        let renderer = StreamRenderer::with_plain_text();
        assert!(renderer.plain_text_mode);
    }

    #[test]
    fn test_plain_text_mode_no_formatting() {
        let mut renderer = StreamRenderer::with_plain_text();
        // In plain text mode, markdown formatting should be ignored
        // This test just ensures the renderer can be created and chunks can be added
        let result = renderer.add_chunk("**bold** and `code`\n");
        assert!(result.is_ok());
    }

    #[test]
    fn test_render_heading_preserves_trailing_newline() {
        let renderer = StreamRenderer::new();
        let mut out = Vec::new();

        renderer
            .render_line_with_inline_formatting_to("## Heading\n", &mut out)
            .unwrap();

        let rendered = String::from_utf8(out).unwrap();
        assert_eq!(strip_ansi(&rendered), "Heading\n");
    }

    #[test]
    fn test_code_fence_matching_requires_same_marker_and_length() {
        let fence = parse_fence_opener("````rust\n").unwrap();
        assert_eq!(fence, FenceMarker { ch: '`', len: 4 });
        assert!(!is_matching_fence_closer("```\n", Some(fence)));
        assert!(is_matching_fence_closer("````\n", Some(fence)));
        assert!(!is_matching_fence_closer("```` trailing\n", Some(fence)));
        assert!(!is_matching_fence_closer("~~~~\n", Some(fence)));
    }

    #[test]
    fn test_code_fence_opener_requires_indentation_of_three_spaces_or_less() {
        assert!(parse_fence_opener("   ```rust\n").is_some());
        assert!(parse_fence_opener("    ```rust\n").is_none());
        assert!(parse_fence_opener("\t```rust\n").is_none());
    }

    #[test]
    fn test_render_line_supports_lists_quotes_and_links() {
        let renderer = StreamRenderer::new();

        let mut list_out = Vec::new();
        renderer
            .render_line_with_inline_formatting_to("- item\n", &mut list_out)
            .unwrap();
        let list_rendered = String::from_utf8(list_out).unwrap();
        assert_eq!(strip_ansi(&list_rendered), "• item\n");

        let mut quote_out = Vec::new();
        renderer
            .render_line_with_inline_formatting_to("> quoted\n", &mut quote_out)
            .unwrap();
        let quote_rendered = String::from_utf8(quote_out).unwrap();
        assert_eq!(strip_ansi(&quote_rendered), "│ quoted\n");

        let mut link_out = Vec::new();
        renderer
            .render_line_with_inline_formatting_to(
                "[Rust](https://www.rust-lang.org)\n",
                &mut link_out,
            )
            .unwrap();
        let link_rendered = String::from_utf8(link_out).unwrap();
        assert_eq!(
            strip_ansi(&link_rendered),
            "Rust (https://www.rust-lang.org)\n"
        );
    }

    #[test]
    fn test_table_block_is_buffered_and_rendered_with_aligned_columns() {
        let mut renderer = StreamRenderer::new();
        let mut out = Vec::new();

        renderer
            .process_regular_line_to("| Name | Value |\n".to_string(), &mut out)
            .unwrap();
        assert!(out.is_empty());

        renderer
            .process_regular_line_to("| :--- | ---: |\n".to_string(), &mut out)
            .unwrap();
        assert!(out.is_empty());

        renderer
            .process_regular_line_to("| long-name | 7 |\n".to_string(), &mut out)
            .unwrap();
        assert!(out.is_empty());

        renderer
            .process_regular_line_to("after table\n".to_string(), &mut out)
            .unwrap();

        let rendered = strip_ansi(&String::from_utf8(out).unwrap());
        assert!(rendered.contains("┌───────────┬───────┐"));
        assert!(rendered.contains("│ Name      │ Value │"));
        assert!(rendered.contains("│ long-name │     7 │"));
        assert!(rendered.contains("└───────────┴───────┘"));
        assert!(rendered.ends_with("after table\n"));
    }

    #[test]
    fn test_table_candidate_falls_back_to_normal_line_when_separator_missing() {
        let mut renderer = StreamRenderer::new();
        let mut out = Vec::new();

        renderer
            .process_regular_line_to("| maybe | header |\n".to_string(), &mut out)
            .unwrap();
        assert!(out.is_empty());

        renderer
            .process_regular_line_to("plain text\n".to_string(), &mut out)
            .unwrap();

        let rendered = strip_ansi(&String::from_utf8(out).unwrap());
        assert_eq!(rendered, "| maybe | header |\nplain text\n");
    }
}
