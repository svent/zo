use anyhow::{Context, Result, bail};
use crossterm::ExecutableCommand;
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use similar::{ChangeTag, TextDiff};
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use crate::config::InlineColors;
use crate::tools::{enforce_tool_path_policy, resolve_workspace_path};

/// Handles file writing with security checks and user approval
pub struct FileWriter {
    allowed_files: Vec<String>,
    allow_all_within_workspace: bool,
    allow_hidden: bool,
    auto_approve: bool,
    inline_colors: InlineColors,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NewlineStyle {
    Lf,
    CrLf,
}

impl FileWriter {
    /// Create a new FileWriter with allowed files list
    pub fn new(
        allowed_files: Vec<String>,
        allow_all_within_workspace: bool,
        allow_hidden: bool,
        auto_approve: bool,
        inline_colors: InlineColors,
    ) -> Self {
        Self {
            allowed_files,
            allow_all_within_workspace,
            allow_hidden,
            auto_approve,
            inline_colors,
        }
    }

    /// Write full file contents with security checks and approval
    ///
    /// Returns Ok(true) if file was written, Ok(false) if user declined.
    pub fn write_file(&self, path: &str, content: &str) -> Result<bool> {
        let resolved = self.authorize_path(path, true)?;

        if resolved.exists() {
            self.write_with_approval(&resolved, content)
        } else {
            fs::write(&resolved, content)
                .with_context(|| format!("Failed to write new file: {}", path))?;

            let mut stdout = io::stdout();
            stdout
                .execute(SetForegroundColor(self.inline_colors.get_emphasis_color()))?
                .execute(SetAttribute(Attribute::Bold))?
                .execute(Print("✓ Created file: "))?
                .execute(ResetColor)?
                .execute(Print(path))?
                .execute(Print("\n"))?;
            stdout.flush()?;

            Ok(true)
        }
    }

    /// Replace exactly one matching text occurrence in a file.
    ///
    /// Fails unless `old_string` matches exactly once.
    pub fn edit_file(&self, path: &str, old_string: &str, new_string: &str) -> Result<bool> {
        if old_string.is_empty() {
            bail!("old_string must not be empty");
        }

        let resolved = self.authorize_path(path, false)?;
        let content = fs::read_to_string(&resolved)
            .with_context(|| format!("Failed to read existing file: {}", path))?;

        let matches = content.match_indices(old_string).count();
        if matches == 0 {
            bail!("old_string not found in file: {}", path);
        }
        if matches > 1 {
            bail!(
                "old_string matched {} times in file '{}'; expected exactly one match",
                matches,
                path
            );
        }

        let updated = content.replacen(old_string, new_string, 1);
        self.write_file(path, &updated)
    }

    /// Replace an inclusive line range in a file.
    pub fn replace_lines(
        &self,
        path: &str,
        start_line: usize,
        end_line: usize,
        new_content: &str,
    ) -> Result<bool> {
        if start_line == 0 || end_line == 0 {
            bail!("start_line and end_line must be >= 1");
        }
        if end_line < start_line {
            bail!("end_line must be >= start_line");
        }

        let resolved = self.authorize_path(path, false)?;
        let content = fs::read_to_string(&resolved)
            .with_context(|| format!("Failed to read existing file: {}", path))?;

        let (lines, had_trailing_newline, newline_style) = split_lines_preserve_trailing(&content);
        if end_line > lines.len() {
            bail!(
                "Line range {}-{} is out of bounds for '{}' ({} lines)",
                start_line,
                end_line,
                path,
                lines.len()
            );
        }

        let replacement_lines = split_replacement_lines(new_content);

        let mut updated_lines: Vec<String> = Vec::new();
        updated_lines.extend(lines[..start_line - 1].iter().cloned());
        updated_lines.extend(replacement_lines);
        updated_lines.extend(lines[end_line..].iter().cloned());

        let newline = match newline_style {
            NewlineStyle::Lf => "\n",
            NewlineStyle::CrLf => "\r\n",
        };

        let mut updated = updated_lines.join(newline);
        if had_trailing_newline {
            updated.push_str(newline);
        }

        self.write_file(path, &updated)
    }

    fn authorize_path(&self, path: &str, allow_missing: bool) -> Result<PathBuf> {
        let resolved = resolve_workspace_path(path, allow_missing)?;
        enforce_tool_path_policy(&resolved, path, self.allow_hidden)?;
        let normalized = resolved.to_string_lossy().to_string();

        if self.allow_all_within_workspace || self.allowed_files.iter().any(|p| p == &normalized) {
            Ok(resolved)
        } else {
            bail!(
                "Security: File '{}' is not in allowed list for this tool call.",
                path
            );
        }
    }

    /// Write file with approval (for overwrites)
    fn write_with_approval(&self, path: &Path, new_content: &str) -> Result<bool> {
        let old_content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read existing file: {}", path.display()))?;

        println!("\n📝 Changes to {}:", path.display());
        self.show_diff(&old_content, new_content)?;

        let approved = if self.auto_approve {
            println!("✓ Auto-approved (--yes flag)\n");
            true
        } else {
            self.ask_approval()?
        };

        if approved {
            fs::write(path, new_content)
                .with_context(|| format!("Failed to write file: {}", path.display()))?;

            let mut stdout = io::stdout();
            stdout
                .execute(SetForegroundColor(self.inline_colors.get_emphasis_color()))?
                .execute(SetAttribute(Attribute::Bold))?
                .execute(Print("✓ Updated file: "))?
                .execute(ResetColor)?
                .execute(Print(path.display().to_string()))?
                .execute(Print("\n\n"))?;
            stdout.flush()?;

            Ok(true)
        } else {
            println!("✗ Skipped: {}\n", path.display());
            Ok(false)
        }
    }

    /// Display colored diff between old and new content
    fn show_diff(&self, old: &str, new: &str) -> Result<()> {
        let mut stdout = io::stdout();

        let diff = TextDiff::from_lines(old, new);

        for change in diff.iter_all_changes() {
            let (sign, color) = match change.tag() {
                ChangeTag::Delete => ("-", Color::Red),
                ChangeTag::Insert => ("+", Color::Green),
                ChangeTag::Equal => (" ", Color::Reset),
            };

            stdout.execute(SetForegroundColor(color))?;
            stdout.execute(Print(format!("{}{}", sign, change)))?;
            stdout.execute(ResetColor)?;
        }

        stdout.flush()?;
        Ok(())
    }

    /// Ask user for approval
    fn ask_approval(&self) -> Result<bool> {
        let mut stdout = io::stdout();
        stdout
            .execute(SetForegroundColor(self.inline_colors.get_prompt_color()))?
            .execute(SetAttribute(Attribute::Bold))?
            .execute(Print("Apply changes? [y/N]: "))?
            .execute(ResetColor)?;
        stdout.flush()?;

        #[cfg(unix)]
        {
            use std::fs::File;
            if let Ok(tty) = File::open("/dev/tty") {
                let mut reader = io::BufReader::new(tty);
                let mut response = String::new();
                reader.read_line(&mut response)?;
                return Ok(response.trim().eq_ignore_ascii_case("y"));
            }
        }

        let mut response = String::new();
        io::stdin().read_line(&mut response)?;
        Ok(response.trim().eq_ignore_ascii_case("y"))
    }
}

fn split_lines_preserve_trailing(content: &str) -> (Vec<String>, bool, NewlineStyle) {
    if content.is_empty() {
        return (Vec::new(), false, NewlineStyle::Lf);
    }

    let newline_style = if content.contains("\r\n") {
        NewlineStyle::CrLf
    } else {
        NewlineStyle::Lf
    };
    let had_trailing_newline = content.ends_with('\n');
    let mut lines: Vec<String> = content
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
        .collect();
    if had_trailing_newline {
        lines.pop();
    }
    (lines, had_trailing_newline, newline_style)
}

fn split_replacement_lines(new_content: &str) -> Vec<String> {
    if new_content.is_empty() {
        return Vec::new();
    }

    let mut lines: Vec<String> = new_content
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
        .collect();
    if new_content.ends_with('\n') {
        lines.pop();
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_writer_creates_new_file() {
        let temp_file = "test_file_writer_new.txt";
        let _ = fs::remove_file(temp_file);

        let normalized = resolve_workspace_path(temp_file, true)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let inline_colors = InlineColors::default();
        let writer = FileWriter::new(vec![normalized], false, false, true, inline_colors);

        let result = writer.write_file(temp_file, "test content");
        assert!(result.is_ok());
        assert!(result.unwrap());

        assert!(Path::new(temp_file).exists());
        let content = fs::read_to_string(temp_file).unwrap();
        assert_eq!(content, "test content");

        fs::remove_file(temp_file).ok();
    }

    #[test]
    fn test_file_writer_rejects_unallowed() {
        let inline_colors = InlineColors::default();
        let writer = FileWriter::new(vec![], false, false, true, inline_colors);
        let result = writer.write_file("forbidden.txt", "content");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("allowed list"));
    }

    #[test]
    fn test_file_writer_auto_approve() {
        let temp_file = "test_file_writer_auto.txt";

        fs::write(temp_file, "old content\n").unwrap();

        let normalized = resolve_workspace_path(temp_file, false)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let inline_colors = InlineColors::default();
        let writer = FileWriter::new(vec![normalized], false, false, true, inline_colors);

        let result = writer.write_file(temp_file, "new content");
        assert!(result.is_ok());
        assert!(result.unwrap());

        let content = fs::read_to_string(temp_file).unwrap();
        assert_eq!(content, "new content");

        fs::remove_file(temp_file).ok();
    }

    #[test]
    fn test_edit_file_exactly_one_match() {
        let temp_file = "test_edit_file.txt";
        fs::write(temp_file, "hello world\n").unwrap();

        let normalized = resolve_workspace_path(temp_file, false)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let writer = FileWriter::new(
            vec![normalized],
            false,
            false,
            true,
            InlineColors::default(),
        );

        let result = writer.edit_file(temp_file, "world", "rust");
        assert!(result.unwrap());

        let content = fs::read_to_string(temp_file).unwrap();
        assert_eq!(content, "hello rust\n");

        fs::remove_file(temp_file).ok();
    }

    #[test]
    fn test_edit_file_multiple_matches_fails() {
        let temp_file = "test_edit_file_multi.txt";
        fs::write(temp_file, "x\nx\n").unwrap();

        let normalized = resolve_workspace_path(temp_file, false)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let writer = FileWriter::new(
            vec![normalized],
            false,
            false,
            true,
            InlineColors::default(),
        );

        let result = writer.edit_file(temp_file, "x", "y");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("exactly one"));

        fs::remove_file(temp_file).ok();
    }

    #[test]
    fn test_replace_lines_success() {
        let temp_file = "test_replace_lines.txt";
        fs::write(temp_file, "a\nb\nc\nd\n").unwrap();

        let normalized = resolve_workspace_path(temp_file, false)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let writer = FileWriter::new(
            vec![normalized],
            false,
            false,
            true,
            InlineColors::default(),
        );

        let result = writer.replace_lines(temp_file, 2, 3, "x\ny");
        assert!(result.unwrap());

        let content = fs::read_to_string(temp_file).unwrap();
        assert_eq!(content, "a\nx\ny\nd\n");

        fs::remove_file(temp_file).ok();
    }

    #[test]
    fn test_replace_lines_out_of_bounds_fails() {
        let temp_file = "test_replace_lines_oob.txt";
        fs::write(temp_file, "a\nb\n").unwrap();

        let normalized = resolve_workspace_path(temp_file, false)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let writer = FileWriter::new(
            vec![normalized],
            false,
            false,
            true,
            InlineColors::default(),
        );

        let result = writer.replace_lines(temp_file, 2, 4, "x");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("out of bounds"));

        fs::remove_file(temp_file).ok();
    }

    #[test]
    fn test_file_writer_allow_all_within_workspace() {
        let temp_file = "test_allow_all_workspace.txt";
        let _ = fs::remove_file(temp_file);

        let writer = FileWriter::new(vec![], true, false, true, InlineColors::default());
        let result = writer.write_file(temp_file, "workspace write");

        assert!(result.unwrap());
        let content = fs::read_to_string(temp_file).unwrap();
        assert_eq!(content, "workspace write");

        fs::remove_file(temp_file).ok();
    }

    #[test]
    fn test_file_writer_blocks_hidden_path_by_default() {
        let hidden_file = ".test_file_writer_hidden.txt";
        let _ = fs::remove_file(hidden_file);

        let blocked_writer = FileWriter::new(vec![], true, false, true, InlineColors::default());
        let blocked = blocked_writer.write_file(hidden_file, "hidden content");
        assert!(blocked.is_err());
        assert!(blocked.unwrap_err().to_string().contains("--hidden"));

        let allowed_writer = FileWriter::new(vec![], true, true, true, InlineColors::default());
        let allowed = allowed_writer.write_file(hidden_file, "hidden content");
        assert!(allowed.unwrap());

        fs::remove_file(hidden_file).ok();
    }

    #[test]
    fn test_split_lines_preserve_trailing_crlf() {
        let (lines, had_trailing_newline, newline_style) =
            split_lines_preserve_trailing("a\r\nb\r\n");
        assert_eq!(lines, vec!["a".to_string(), "b".to_string()]);
        assert!(had_trailing_newline);
        assert_eq!(newline_style, NewlineStyle::CrLf);
    }

    #[test]
    fn test_replace_lines_preserves_crlf_line_endings() {
        let temp_file = "test_replace_lines_crlf.txt";
        fs::write(temp_file, "a\r\nb\r\nc\r\n").unwrap();

        let normalized = resolve_workspace_path(temp_file, false)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let writer = FileWriter::new(
            vec![normalized],
            false,
            false,
            true,
            InlineColors::default(),
        );

        let result = writer.replace_lines(temp_file, 2, 2, "x\ny");
        assert!(result.unwrap());

        let content = fs::read_to_string(temp_file).unwrap();
        assert_eq!(content, "a\r\nx\r\ny\r\nc\r\n");

        fs::remove_file(temp_file).ok();
    }

    #[test]
    fn test_split_replacement_lines_normalizes_crlf_input() {
        let lines = split_replacement_lines("x\r\ny\r\n");
        assert_eq!(lines, vec!["x".to_string(), "y".to_string()]);
    }
}
