use anyhow::{Context, Result};
use crossterm::ExecutableCommand;
use crossterm::style::{Color, Print, ResetColor, SetForegroundColor};
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
/// - Fence markers (``` or ~~~) are detected automatically
/// - Inline formatting (bold, italic, inline code, headers) is applied during streaming
///
/// When `plain_text_mode` is enabled (e.g., when output is piped to a file),
/// all markdown formatting and syntax highlighting is disabled for clean output.
pub struct StreamRenderer {
    renderer: MarkdownRenderer,
    buffer: String,
    in_code_block: bool,
    code_fence_chars: String,
    line_buffer: String,
    inline_buffer: String,
    inline_colors: InlineColors,
    plain_text_mode: bool,
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
            code_fence_chars: String::new(),
            line_buffer: String::new(),
            inline_buffer: String::new(),
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
            code_fence_chars: String::new(),
            line_buffer: String::new(),
            inline_buffer: String::new(),
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
    /// - Outside code blocks: lines are rendered with inline markdown formatting applied
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
                let line = self.line_buffer.clone();
                self.line_buffer.clear();

                // Check for code fence markers (```, ~~~)
                let trimmed = line.trim_start();
                if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
                    if !self.in_code_block {
                        // Starting a code block
                        self.in_code_block = true;
                        self.code_fence_chars = if trimmed.starts_with("```") {
                            "```".to_string()
                        } else {
                            "~~~".to_string()
                        };
                        self.buffer.push_str(&line);
                    } else if trimmed.starts_with(&self.code_fence_chars) {
                        // Ending a code block - add the closing fence and render
                        self.buffer.push_str(&line);
                        self.renderer.render(&self.buffer)?;
                        self.buffer.clear();
                        self.in_code_block = false;
                        self.code_fence_chars.clear();
                    } else {
                        // Code inside the block
                        self.buffer.push_str(&line);
                    }
                } else if self.in_code_block {
                    // Inside code block - keep buffering
                    self.buffer.push_str(&line);
                } else {
                    // Regular text - apply simple inline formatting and print
                    self.render_line_with_inline_formatting(&line)?;
                }
            }
        }

        Ok(())
    }

    /// Render a line with simple inline markdown formatting applied.
    ///
    /// Applies terminal formatting for common markdown patterns:
    /// - `**bold**` → bold terminal text
    /// - `*italic*` → colored terminal text
    /// - `` `code` `` → colored terminal text
    /// - `# heading` → colored terminal text
    ///
    /// Colors are determined by the configured inline_colors.
    /// This provides basic markdown rendering while maintaining true streaming behavior.
    fn render_line_with_inline_formatting(&mut self, line: &str) -> Result<()> {
        let mut stdout = io::stdout();

        self.inline_buffer.push_str(line);
        let mut line = std::mem::take(&mut self.inline_buffer);
        let has_trailing_newline = line.ends_with('\n');
        if has_trailing_newline {
            line.pop();
        }

        // Check if line is a header
        let trimmed = line.trim_start();
        if trimmed.starts_with("# ") {
            stdout
                .execute(SetForegroundColor(self.inline_colors.get_heading_color()))?
                .execute(Print(&line))?
                .execute(ResetColor)?;
            stdout.flush()?;
            return Ok(());
        }

        // Process line for inline formatting
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        let mut pending_start = 0;

        while i < chars.len() {
            // Check for inline code `...`
            if chars[i] == '`' && i + 1 < chars.len() {
                if let Some(end) = chars[i + 1..].iter().position(|&c| c == '`') {
                    // Found inline code
                    if i > pending_start {
                        let text: String = chars[pending_start..i].iter().collect();
                        stdout.execute(Print(&text))?;
                    }
                    let code: String = chars[i + 1..i + 1 + end].iter().collect();
                    stdout
                        .execute(SetForegroundColor(
                            self.inline_colors.get_inline_code_color(),
                        ))?
                        .execute(Print("`"))?
                        .execute(Print(&code))?
                        .execute(Print("`"))?
                        .execute(ResetColor)?;
                    i += end + 2;
                    pending_start = i;
                    continue;
                } else if !has_trailing_newline {
                    if i > pending_start {
                        let text: String = chars[pending_start..i].iter().collect();
                        stdout.execute(Print(&text))?;
                    }
                    let tail: String = chars[i..].iter().collect();
                    self.inline_buffer.push_str(&tail);
                    stdout.flush()?;
                    return Ok(());
                }
            }

            // Check for bold **...**
            if i + 1 < chars.len() && chars[i] == '*' && chars[i + 1] == '*' {
                if let Some(end) = find_closing_marker(&chars[i + 2..], "**") {
                    // Found bold text
                    if i > pending_start {
                        let text: String = chars[pending_start..i].iter().collect();
                        stdout.execute(Print(&text))?;
                    }
                    let text: String = chars[i + 2..i + 2 + end].iter().collect();
                    stdout
                        .execute(SetForegroundColor(self.inline_colors.get_emphasis_color()))?
                        .execute(crossterm::style::SetAttribute(
                            crossterm::style::Attribute::Bold,
                        ))?
                        .execute(Print(&text))?
                        .execute(crossterm::style::SetAttribute(
                            crossterm::style::Attribute::Reset,
                        ))?
                        .execute(ResetColor)?;
                    i += end + 4;
                    pending_start = i;
                    continue;
                } else if !has_trailing_newline {
                    if i > pending_start {
                        let text: String = chars[pending_start..i].iter().collect();
                        stdout.execute(Print(&text))?;
                    }
                    let tail: String = chars[i..].iter().collect();
                    self.inline_buffer.push_str(&tail);
                    stdout.flush()?;
                    return Ok(());
                }
            }

            // Check for italic *...*
            if chars[i] == '*' && !(i + 1 < chars.len() && chars[i + 1] == '*') {
                if let Some(end) = chars[i + 1..].iter().position(|&c| c == '*') {
                    // Found italic text
                    if i > pending_start {
                        let text: String = chars[pending_start..i].iter().collect();
                        stdout.execute(Print(&text))?;
                    }
                    let text: String = chars[i + 1..i + 1 + end].iter().collect();
                    stdout
                        .execute(SetForegroundColor(self.inline_colors.get_emphasis_color()))?
                        .execute(crossterm::style::SetAttribute(
                            crossterm::style::Attribute::Italic,
                        ))?
                        .execute(Print(&text))?
                        .execute(crossterm::style::SetAttribute(
                            crossterm::style::Attribute::Reset,
                        ))?
                        .execute(ResetColor)?;
                    i += end + 2;
                    pending_start = i;
                    continue;
                } else if !has_trailing_newline {
                    if i > pending_start {
                        let text: String = chars[pending_start..i].iter().collect();
                        stdout.execute(Print(&text))?;
                    }
                    let tail: String = chars[i..].iter().collect();
                    self.inline_buffer.push_str(&tail);
                    stdout.flush()?;
                    return Ok(());
                }
            }

            i += 1;
        }

        if pending_start < chars.len() {
            let text: String = chars[pending_start..].iter().collect();
            stdout.execute(Print(&text))?;
        }

        if has_trailing_newline {
            stdout.execute(Print("\n"))?;
        }
        stdout.flush()?;
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
            } else {
                // Render remaining line with inline formatting
                let line = self.line_buffer.clone();
                self.render_line_with_inline_formatting(&line)?;
            }
            self.line_buffer.clear();
        }

        // Render any remaining buffered code block
        if !self.buffer.is_empty() {
            self.renderer.render(&self.buffer)?;
            self.buffer.clear();
        }

        if !self.inline_buffer.is_empty() {
            print!("{}", self.inline_buffer);
            self.inline_buffer.clear();
        }

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

/// Helper function to find closing marker in character slice
fn find_closing_marker(chars: &[char], marker: &str) -> Option<usize> {
    let marker_chars: Vec<char> = marker.chars().collect();
    let marker_len = marker_chars.len();

    for i in 0..chars.len() {
        if i + marker_len <= chars.len() && chars[i..i + marker_len] == marker_chars[..] {
            return Some(i);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::InlineColors;

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
}
