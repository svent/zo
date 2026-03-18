//! Readline integration for chat mode with context-aware file path autocompletion
//!
//! This module provides rustyline integration for zo's chat mode, offering:
//! - Full Emacs-style line editing (cursor movement, word deletion, etc.)
//! - Command history with optional persistence to file
//! - Context-aware file path autocompletion triggered by `@`, `!`, or `@!`

use crossterm::style::Color;
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::Validator;
use rustyline::{
    Cmd, CompletionType, Config, EditMode, Editor, EventHandler, Helper, KeyCode, KeyEvent,
    Modifiers,
};
use std::borrow::Cow;
use std::path::{Path, PathBuf};

/// Result of searching for a file marker in the input
struct MarkerMatch {
    /// Byte position where the path starts (after marker)
    path_start: usize,
}

/// Find the file marker (@, !, @!) that the cursor is currently completing
///
/// Returns None if cursor is not in a file pattern context.
/// Only matches markers at word boundaries (start of line or after whitespace).
fn find_file_marker(line: &str, pos: usize) -> Option<MarkerMatch> {
    // Get the portion of line up to cursor
    let prefix = &line[..pos];

    // Find the start of the current "word" (after last whitespace)
    let word_start = prefix
        .rfind(char::is_whitespace)
        .map(|i| i + 1)
        .unwrap_or(0);

    let word = &prefix[word_start..];

    // Check for markers at the start of the word
    if word.starts_with("@!") {
        Some(MarkerMatch {
            path_start: word_start + 2,
        })
    } else if word.starts_with('@') {
        Some(MarkerMatch {
            path_start: word_start + 1,
        })
    } else if word.starts_with('!') {
        Some(MarkerMatch {
            path_start: word_start + 1,
        })
    } else {
        None
    }
}

/// Complete file paths based on partial input
///
/// - Case-insensitive matching
/// - Hidden files only shown with explicit '.' prefix
/// - Directories get trailing '/' and trigger immediate sub-completion
fn complete_path(partial: &str) -> Vec<Pair> {
    // Handle glob patterns - return as single completion
    if partial.contains('*') || partial.contains('?') || partial.contains('[') {
        return vec![Pair {
            display: partial.to_string(),
            replacement: partial.to_string(),
        }];
    }

    // Determine directory to scan and prefix to match
    let (dir_path, prefix) = if partial.is_empty() {
        (Path::new("."), "")
    } else if partial.ends_with('/') || partial.ends_with(std::path::MAIN_SEPARATOR) {
        (Path::new(partial), "")
    } else {
        let path = Path::new(partial);
        match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) => {
                let parent = if parent.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    parent
                };
                (parent, name.to_str().unwrap_or(""))
            }
            _ => (Path::new("."), partial),
        }
    };

    // Read directory entries
    let entries = match std::fs::read_dir(dir_path) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut candidates: Vec<Pair> = Vec::new();
    let prefix_lower = prefix.to_lowercase();
    let show_hidden = prefix.starts_with('.');

    for entry in entries.filter_map(|e| e.ok()) {
        let file_name = entry.file_name();
        let name_str = file_name.to_string_lossy();

        // Skip hidden files unless prefix starts with '.'
        if name_str.starts_with('.') && !show_hidden {
            continue;
        }

        // Case-insensitive prefix match
        if !name_str.to_lowercase().starts_with(&prefix_lower) {
            continue;
        }

        // Build the full path for replacement
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);

        let replacement = if partial.is_empty() || partial.ends_with('/') {
            if is_dir {
                format!("{}{}/", partial, name_str)
            } else {
                format!("{}{}", partial, name_str)
            }
        } else {
            // Replace the prefix portion with the actual filename
            let base = if let Some(parent) = Path::new(partial).parent() {
                if parent.as_os_str().is_empty() {
                    String::new()
                } else {
                    format!("{}/", parent.display())
                }
            } else {
                String::new()
            };

            if is_dir {
                format!("{}{}/", base, name_str)
            } else {
                format!("{}{}", base, name_str)
            }
        };

        let display = if is_dir {
            format!("{}/", name_str)
        } else {
            name_str.to_string()
        };

        candidates.push(Pair {
            display,
            replacement,
        });
    }

    // Sort: directories first, then alphabetically (case-insensitive)
    candidates.sort_by(|a, b| {
        let a_is_dir = a.display.ends_with('/');
        let b_is_dir = b.display.ends_with('/');
        match (a_is_dir, b_is_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.display.to_lowercase().cmp(&b.display.to_lowercase()),
        }
    });

    candidates
}

/// Completer for file patterns (@file, !file, @!file)
pub struct FilePatternCompleter;

impl Completer for FilePatternCompleter {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        // Find if we're in a file pattern context
        let marker = match find_file_marker(line, pos) {
            Some(m) => m,
            None => return Ok((pos, Vec::new())),
        };

        // Extract the partial path (everything after the marker)
        let partial = &line[marker.path_start..pos];

        // Get completions
        let candidates = complete_path(partial);

        // Return start position (where path begins) and candidates
        Ok((marker.path_start, candidates))
    }
}

/// Helper for rustyline that provides file pattern completion
pub struct ZoHelper {
    completer: FilePatternCompleter,
    input_color_ansi: String,
}

impl ZoHelper {
    fn set_input_color(&mut self, color: Color) {
        self.input_color_ansi = color_to_ansi(color);
    }
}

impl Completer for ZoHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        self.completer.complete(line, pos, ctx)
    }
}

impl Hinter for ZoHelper {
    type Hint = String;

    fn hint(&self, _line: &str, _pos: usize, _ctx: &rustyline::Context<'_>) -> Option<String> {
        None // No hints for now
    }
}

impl Highlighter for ZoHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        if line.is_empty() {
            return Cow::Borrowed(line);
        }

        Cow::Owned(format!("\x1b[{}m{}\x1b[0m", self.input_color_ansi, line))
    }

    fn highlight_char(&self, line: &str, _pos: usize, forced: bool) -> bool {
        // Re-highlight on normal typing/cursor movement so color appears immediately.
        !forced && !line.is_empty()
    }
}
impl Validator for ZoHelper {}
impl Helper for ZoHelper {}

/// Main interface for chat input, wrapping the rustyline editor
pub struct ChatReadline {
    editor: Editor<ZoHelper, DefaultHistory>,
    history_path: Option<PathBuf>,
}

impl ChatReadline {
    /// Create a new readline instance
    ///
    /// # Arguments
    /// * `history_file` - Optional path to history file (enables persistence)
    pub fn new(history_file: Option<&str>) -> rustyline::Result<Self> {
        let config = Config::builder()
            .completion_type(CompletionType::List)
            .edit_mode(EditMode::Emacs)
            .auto_add_history(true)
            .build();

        let helper = ZoHelper {
            completer: FilePatternCompleter,
            input_color_ansi: color_to_ansi(Color::Cyan),
        };

        let mut editor = Editor::with_config(config)?;
        editor.set_helper(Some(helper));

        // Bind keys for multiline input (insert newline without submitting)
        // Alt-Enter: Works in most modern terminals
        editor.bind_sequence(
            KeyEvent(KeyCode::Enter, Modifiers::ALT),
            EventHandler::Simple(Cmd::Newline),
        );
        // Ctrl-O: Fallback for terminals where Alt-Enter doesn't work
        editor.bind_sequence(KeyEvent::ctrl('O'), EventHandler::Simple(Cmd::Newline));
        // Ctrl-J: Alternative newline binding
        editor.bind_sequence(KeyEvent::ctrl('J'), EventHandler::Simple(Cmd::Newline));

        // Expand ~ in history path and load if exists
        let history_path = history_file.map(expand_tilde);

        if let Some(ref path) = history_path {
            // Ignore errors - history file might not exist yet
            let _ = editor.load_history(path);
        }

        Ok(Self {
            editor,
            history_path,
        })
    }

    /// Read a line of input with colored prompt
    ///
    /// Returns:
    /// - Ok(Some(line)) - User entered input
    /// - Ok(None) - User wants to exit (Ctrl-D, "exit", "quit", "q")
    /// - Err(e) - Error occurred
    pub fn read_input(&mut self, prompt_color: Color) -> anyhow::Result<Option<String>> {
        if let Some(helper) = self.editor.helper_mut() {
            helper.set_input_color(prompt_color);
        }

        // Build colored prompt
        let prompt = format!("\x1b[{}m> \x1b[0m", color_to_ansi(prompt_color));

        match self.editor.readline(&prompt) {
            Ok(line) => {
                let trimmed = line.trim();

                // Check for exit commands
                if trimmed.eq_ignore_ascii_case("exit")
                    || trimmed.eq_ignore_ascii_case("quit")
                    || trimmed.eq_ignore_ascii_case("q")
                {
                    return Ok(None);
                }

                Ok(Some(trimmed.to_string()))
            }
            Err(ReadlineError::Interrupted) => {
                // Ctrl-C - treat as wanting to exit
                Ok(None)
            }
            Err(ReadlineError::Eof) => {
                // Ctrl-D - exit
                Ok(None)
            }
            Err(e) => Err(anyhow::anyhow!("Readline error: {}", e)),
        }
    }

    /// Save history if persistence is enabled
    pub fn save_history(&mut self) {
        if let Some(ref path) = self.history_path {
            // Create parent directory if needed
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }

            if let Err(e) = self.editor.save_history(path) {
                eprintln!("Warning: Failed to save history: {}", e);
            }
        }
    }
}

impl Drop for ChatReadline {
    fn drop(&mut self) {
        self.save_history();
    }
}

/// Expand ~ to home directory
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    }
    PathBuf::from(path)
}

/// Convert crossterm Color to ANSI code
fn color_to_ansi(color: Color) -> String {
    match color {
        Color::Black => "30".to_string(),
        Color::DarkRed => "31".to_string(),
        Color::DarkGreen => "32".to_string(),
        Color::DarkYellow => "33".to_string(),
        Color::DarkBlue => "34".to_string(),
        Color::DarkMagenta => "35".to_string(),
        Color::DarkCyan => "36".to_string(),
        Color::Grey => "37".to_string(),
        Color::DarkGrey => "90".to_string(),
        Color::Red => "91".to_string(),
        Color::Green => "92".to_string(),
        Color::Yellow => "93".to_string(),
        Color::Blue => "94".to_string(),
        Color::Magenta => "95".to_string(),
        Color::Cyan => "96".to_string(),
        Color::White => "97".to_string(),
        Color::Rgb { r, g, b } => format!("38;2;{};{};{}", r, g, b),
        _ => "37".to_string(), // Default to grey
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_file_marker_at_start() {
        let m = find_file_marker("@file.txt", 5).unwrap();
        assert_eq!(m.path_start, 1);
    }

    #[test]
    fn test_find_file_marker_mid_line() {
        let m = find_file_marker("analyze @src/main.rs", 15).unwrap();
        assert_eq!(m.path_start, 9);
    }

    #[test]
    fn test_find_file_marker_output() {
        let m = find_file_marker("!output.txt", 6).unwrap();
        assert_eq!(m.path_start, 1);
    }

    #[test]
    fn test_find_file_marker_input_output() {
        let m = find_file_marker("@!config.json", 8).unwrap();
        assert_eq!(m.path_start, 2);
    }

    #[test]
    fn test_find_file_marker_no_marker() {
        assert!(find_file_marker("just text", 5).is_none());
    }

    #[test]
    fn test_find_file_marker_not_at_boundary() {
        // @ in middle of word should not match
        assert!(find_file_marker("email@example.com", 10).is_none());
    }

    #[test]
    fn test_complete_path_glob_passthrough() {
        let results = complete_path("*.rs");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].replacement, "*.rs");
    }

    #[test]
    fn test_expand_tilde() {
        let result = expand_tilde("~/.zo/history.txt");
        if let Some(home) = dirs::home_dir() {
            assert_eq!(result, home.join(".zo/history.txt"));
        }
    }

    #[test]
    fn test_expand_tilde_no_tilde() {
        let result = expand_tilde("/absolute/path.txt");
        assert_eq!(result, PathBuf::from("/absolute/path.txt"));
    }

    #[test]
    fn test_highlighter_colors_typed_input() {
        let mut helper = ZoHelper {
            completer: FilePatternCompleter,
            input_color_ansi: color_to_ansi(Color::Cyan),
        };

        helper.set_input_color(Color::Magenta);
        let colored = helper.highlight("hello", 0);
        assert_eq!(colored, "\x1b[95mhello\x1b[0m");
    }

    #[test]
    fn test_highlighter_requests_refresh_on_typed_chars() {
        let helper = ZoHelper {
            completer: FilePatternCompleter,
            input_color_ansi: color_to_ansi(Color::Cyan),
        };

        assert!(helper.highlight_char("h", 1, false));
        assert!(!helper.highlight_char("", 0, false));
        assert!(!helper.highlight_char("hello", 5, true));
    }
}
