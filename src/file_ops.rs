use anyhow::{Context, Result, bail};
use crossterm::ExecutableCommand;
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use similar::{ChangeTag, TextDiff};
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::Path;

use crate::config::InlineColors;
use crate::tools::normalize_path;

/// Handles file writing with security checks and user approval
pub struct FileWriter {
    allowed_files: Vec<String>,
    auto_approve: bool,
    inline_colors: InlineColors,
}

impl FileWriter {
    /// Create a new FileWriter with allowed files list
    pub fn new(
        allowed_files: Vec<String>,
        auto_approve: bool,
        inline_colors: InlineColors,
    ) -> Self {
        Self {
            allowed_files,
            auto_approve,
            inline_colors,
        }
    }

    /// Write file with security checks and approval
    ///
    /// Returns Ok(true) if file was written, Ok(false) if user declined
    pub fn write_file(&self, path: &str, content: &str) -> Result<bool> {
        // Security: Validate path is allowed
        if !self.is_allowed(path)? {
            eprintln!("aborting because path is not allowed!!!");
            bail!(
                "Security: File '{}' is not in allowed list. \
                 Use !{} in your prompt to allow writing.",
                path,
                path
            );
        }

        let path_buf = Path::new(path);

        // Check if file exists
        if path_buf.exists() {
            // Overwrite - need approval
            self.write_with_approval(path_buf, content)
        } else {
            // New file - write directly
            fs::write(path_buf, content)
                .with_context(|| format!("Failed to write new file: {}", path))?;

            // Print colorized success message
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

    /// Check if path is in allowed list
    fn is_allowed(&self, path: &str) -> Result<bool> {
        let normalized = normalize_path(path)?;
        Ok(self.allowed_files.iter().any(|p| p == &normalized))
    }

    /// Write file with approval (for overwrites)
    fn write_with_approval(&self, path: &Path, new_content: &str) -> Result<bool> {
        let old_content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read existing file: {}", path.display()))?;

        // Show diff
        println!("\n📝 Changes to {}:", path.display());
        self.show_diff(&old_content, new_content)?;

        // Auto-approve or ask user
        let approved = if self.auto_approve {
            println!("✓ Auto-approved (--yes flag)\n");
            true
        } else {
            self.ask_approval()?
        };

        if approved {
            fs::write(path, new_content)
                .with_context(|| format!("Failed to write file: {}", path.display()))?;

            // Print colorized success message
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
        // Print colorized prompt
        let mut stdout = io::stdout();
        stdout
            .execute(SetForegroundColor(self.inline_colors.get_prompt_color()))?
            .execute(SetAttribute(Attribute::Bold))?
            .execute(Print("Apply changes? [y/N]: "))?
            .execute(ResetColor)?;
        stdout.flush()?;

        // Read from /dev/tty on Unix (same pattern as chat.rs)
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

        // Fallback to stdin
        let mut response = String::new();
        io::stdin().read_line(&mut response)?;
        Ok(response.trim().eq_ignore_ascii_case("y"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_file_writer_creates_new_file() {
        let temp_file = "test_file_writer_new.txt";

        // Ensure file doesn't exist
        let _ = fs::remove_file(temp_file);

        let normalized = normalize_path(temp_file).unwrap();
        let inline_colors = InlineColors::default();
        let writer = FileWriter::new(vec![normalized], true, inline_colors);

        let result = writer.write_file(temp_file, "test content");
        assert!(result.is_ok());
        assert!(result.unwrap());

        // Verify file was created
        assert!(Path::new(temp_file).exists());
        let content = fs::read_to_string(temp_file).unwrap();
        assert_eq!(content, "test content");

        // Cleanup
        fs::remove_file(temp_file).ok();
    }

    #[test]
    fn test_file_writer_rejects_unallowed() {
        let inline_colors = InlineColors::default();
        let writer = FileWriter::new(vec![], true, inline_colors);
        let result = writer.write_file("forbidden.txt", "content");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not in allowed list")
        );
    }

    #[test]
    fn test_file_writer_auto_approve() {
        let temp_file = "test_file_writer_auto.txt";

        // Create initial file
        let mut file = fs::File::create(temp_file).unwrap();
        writeln!(file, "old content").unwrap();
        drop(file);

        let normalized = normalize_path(temp_file).unwrap();
        let inline_colors = InlineColors::default();
        let writer = FileWriter::new(vec![normalized], true, inline_colors); // auto_approve = true

        let result = writer.write_file(temp_file, "new content");
        assert!(result.is_ok());
        assert!(result.unwrap());

        // Verify file was updated
        let content = fs::read_to_string(temp_file).unwrap();
        assert_eq!(content, "new content");

        // Cleanup
        fs::remove_file(temp_file).ok();
    }
}
