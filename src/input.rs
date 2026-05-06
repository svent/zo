use anyhow::{Context, Result, bail};
use glob::glob;
use std::collections::HashMap;
use std::fs;
use std::io::{self, IsTerminal, Read};
use std::path::Path;

use crate::tools::normalize_path;

/// Type of file reference syntax marker
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileSyntaxType {
    /// @ syntax - input only (must exist)
    Input,
    /// ! syntax - output only (can be created)
    Output,
    /// @! syntax - read and write (read if exists, can be created)
    InputOutput,
}

impl FileSyntaxType {
    /// Returns true if this syntax type requires file to exist
    fn requires_existing_file(&self) -> bool {
        matches!(self, FileSyntaxType::Input | FileSyntaxType::InputOutput)
    }

    /// Returns true if this syntax should be included in input file references
    fn is_input(&self) -> bool {
        matches!(self, FileSyntaxType::Input | FileSyntaxType::InputOutput)
    }

    /// Returns true if this syntax should be included in output file specs
    fn is_output(&self) -> bool {
        matches!(self, FileSyntaxType::Output | FileSyntaxType::InputOutput)
    }

    /// Returns the more permissive of two syntax types (for deduplication)
    fn merge(self, other: FileSyntaxType) -> FileSyntaxType {
        match (self, other) {
            // InputOutput is most permissive
            (FileSyntaxType::InputOutput, _) | (_, FileSyntaxType::InputOutput) => {
                FileSyntaxType::InputOutput
            }
            // Otherwise, return either (they're the same or one is Input/Output)
            _ => self,
        }
    }
}

/// A parsed file pattern before type-specific conversion
#[derive(Debug, Clone)]
struct ParsedFilePattern {
    /// Resolved file paths after glob expansion (sorted, deduplicated)
    resolved_files: Vec<String>,

    /// Which syntax was used (merged if same file appears multiple times)
    syntax_type: FileSyntaxType,
}

/// Result of unified parsing
#[derive(Debug)]
struct UnifiedParseResult {
    /// Modified prompt with globs expanded
    modified_prompt: String,

    /// All parsed files with their syntax types (key = normalized path)
    files: HashMap<String, ParsedFilePattern>,
}

/// Check if position is @! with proper whitespace boundary
fn is_at_bang(chars: &[char], i: usize) -> bool {
    i + 1 < chars.len()
        && chars[i] == '@'
        && chars[i + 1] == '!'
        && (i == 0 || chars[i - 1].is_whitespace())
}

/// Check if position is @ (not @!) with proper whitespace boundary
fn is_at(chars: &[char], i: usize) -> bool {
    chars[i] == '@'
        && (i == 0 || chars[i - 1].is_whitespace())
        && !(i + 1 < chars.len() && chars[i + 1] == '!') // Not @!
}

/// Check if position is ! (not part of @!) with proper whitespace boundary
fn is_bang(chars: &[char], i: usize) -> bool {
    chars[i] == '!' && (i == 0 || chars[i - 1].is_whitespace())
    // Note: we check whitespace before, so if previous char is @, it would fail whitespace check
}

/// Build replacement text for prompt rewriting
fn build_replacement_text(files: &[String], syntax_type: FileSyntaxType) -> String {
    let prefix = match syntax_type {
        FileSyntaxType::Input => "@",
        FileSyntaxType::Output => "!",
        FileSyntaxType::InputOutput => "@!",
    };

    files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            if i > 0 {
                format!(" {}{}", prefix, f)
            } else {
                format!("{}{}", prefix, f)
            }
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Apply replacements to input string in reverse order
fn apply_replacements(input: &str, mut replacements: Vec<(usize, usize, String)>) -> String {
    if replacements.is_empty() {
        return input.to_string();
    }

    let mut result = input.to_string();

    // Sort by start position descending (apply from end to start to preserve positions)
    replacements.sort_by(|a, b| b.0.cmp(&a.0));

    for (start, end, replacement) in replacements {
        let byte_start = input.char_indices().nth(start).map(|(i, _)| i).unwrap_or(0);
        let byte_end = input
            .char_indices()
            .nth(end)
            .map(|(i, _)| i)
            .unwrap_or(input.len());
        result.replace_range(byte_start..byte_end, &replacement);
    }

    result
}

/// Check if a string contains glob metacharacters
fn contains_glob_chars(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[') || s.contains('{')
}

/// Resolve a file pattern to concrete paths
///
/// Strategy:
/// 1. If path exists as-is, return it (literal file takes precedence)
/// 2. If no glob chars and doesn't exist, return error (missing file)
/// 3. If has glob chars, expand pattern
/// 4. If pattern matches nothing, return error
///
/// # Arguments
/// * `pattern` - File pattern (literal path or glob)
///
/// # Returns
/// Vector of resolved file paths (sorted alphabetically)
///
/// # Errors
/// - Pattern matches no files
/// - Glob pattern syntax error
#[cfg(test)]
fn resolve_file_pattern(pattern: &str) -> Result<Vec<String>> {
    resolve_file_pattern_impl(pattern, false)
}

/// Resolve a file pattern for output files (allows non-existing files)
///
/// For output files, non-existing literal files are allowed (they will be created).
/// Glob patterns still require at least one match.
#[cfg(test)]
fn resolve_output_file_pattern(pattern: &str) -> Result<Vec<String>> {
    resolve_file_pattern_impl(pattern, true)
}

/// Internal implementation of file pattern resolution
///
/// # Arguments
/// * `pattern` - File pattern (literal path or glob)
/// * `allow_missing` - If true, non-existing literal files are allowed
fn resolve_file_pattern_impl(pattern: &str, allow_missing: bool) -> Result<Vec<String>> {
    let path = Path::new(pattern);

    // Check if it's a literal file/directory that exists
    if path.exists() {
        return Ok(vec![pattern.to_string()]);
    }

    // Try trimming trailing punctuation if original doesn't exist.
    // This handles cases like "@file.txt," or "!output.txt," where user typed punctuation after filename
    // (e.g., "look at @file1.txt, @file2.txt and @file3.txt" or "create !output.txt, please")
    if !contains_glob_chars(pattern) {
        let trimmed = pattern.trim_end_matches(&[',', '.', ';', ')', ':'][..]);
        if trimmed != pattern {
            // If trimmed version exists, use it
            if Path::new(trimmed).exists() {
                return Ok(vec![trimmed.to_string()]);
            }
            // For output files (allow_missing), use trimmed version even if it doesn't exist yet
            // This prevents LLMs from creating files with trailing punctuation
            if allow_missing {
                return Ok(vec![trimmed.to_string()]);
            }
        }
    }

    // If no glob characters
    if !contains_glob_chars(pattern) {
        if allow_missing {
            // For output files, allow non-existing literal files (already handled trimming above)
            return Ok(vec![pattern.to_string()]);
        } else {
            // For input files, this is an error
            bail!(
                "File '{}' not found. If you meant to use a glob pattern, \
                 ensure it contains wildcards like * or ?",
                pattern
            );
        }
    }

    // Expand glob pattern
    let mut matches: Vec<String> = glob(pattern)
        .with_context(|| format!("Invalid glob pattern: {}", pattern))?
        .filter_map(|entry| entry.ok().map(|p| p.to_string_lossy().to_string()))
        .collect();

    if matches.is_empty() {
        bail!(
            "Glob pattern '{}' matched no files. \
             Check that the pattern is correct and files exist.",
            pattern
        );
    }

    // Warn if large expansion
    if matches.len() > 100 {
        eprintln!(
            "Warning: Pattern '{}' matched {} files. \
             This may exceed model context limits.",
            pattern,
            matches.len()
        );
    }

    // Sort for consistent ordering
    matches.sort();

    Ok(matches)
}

/// A file referenced with @-syntax
#[derive(Debug, Clone, PartialEq)]
pub struct FileReference {
    /// The filename as specified by the user
    pub filename: String,

    /// The contents of the file
    pub content: String,
}

/// An output file specified with !file or @!file syntax
#[derive(Debug, Clone, PartialEq)]
pub struct OutputFileSpec {
    /// Original relative path (as specified by user)
    pub filename: String,

    /// Normalized absolute path (for validation)
    pub normalized_path: String,

    /// true for @!file (read and write), false for !file (write only)
    pub include_as_input: bool,
}

/// Parsed input from command line and STDIN
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedInput {
    /// Model override from slash command (e.g., "sonnet" from "/sonnet")
    pub model_override: Option<String>,

    /// Main prompt text
    pub prompt: String,

    /// Content from STDIN if available
    pub stdin_content: Option<String>,

    /// Files referenced with @-syntax
    pub file_references: Vec<FileReference>,

    /// Output files specified with !file or @!file syntax
    pub output_files: Vec<OutputFileSpec>,
}

/// Parse all file patterns (@, !, @!) in a single pass
///
/// This unified parser handles:
/// - @ syntax (input files - must exist)
/// - ! syntax (output files - can be created)
/// - @! syntax (input+output files)
/// - Glob pattern expansion
/// - Deduplication by normalized path
/// - Prompt rewriting with expanded file lists
///
/// When the same file appears with different syntaxes, the most permissive wins:
/// - @!file (InputOutput) > @file (Input) or !file (Output)
///
/// # Returns
/// Modified prompt and mapping of normalized paths to parsed patterns
fn parse_all_file_patterns(input: &str) -> Result<UnifiedParseResult> {
    let mut files: HashMap<String, ParsedFilePattern> = HashMap::new();
    let chars: Vec<char> = input.chars().collect();
    let mut replacements: Vec<(usize, usize, String)> = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        let start_pos = i;

        // Detect syntax marker (@!, @, or !)
        let (syntax_type, prefix_len) = if is_at_bang(&chars, i) {
            (Some(FileSyntaxType::InputOutput), 2)
        } else if is_at(&chars, i) {
            (Some(FileSyntaxType::Input), 1)
        } else if is_bang(&chars, i) {
            (Some(FileSyntaxType::Output), 1)
        } else {
            (None, 0)
        };

        if let Some(stype) = syntax_type {
            // Extract pattern (e.g., "*.rs" or "file.txt")
            let (pattern, consumed) = extract_filename(&chars[i + prefix_len..])?;

            if !pattern.is_empty() {
                // Resolve glob pattern
                let allow_missing = !stype.requires_existing_file();
                // Try trimming punctuation for input files (@ and @!), but not pure output files (!)
                let resolved =
                    resolve_file_pattern_impl(&pattern, allow_missing).with_context(|| {
                        let prefix = match stype {
                            FileSyntaxType::Input => "@",
                            FileSyntaxType::Output => "!",
                            FileSyntaxType::InputOutput => "@!",
                        };
                        format!("Failed to resolve pattern '{}{}'", prefix, pattern)
                    })?;

                // Track all resolved files for replacement
                let mut resolved_for_replacement = Vec::new();

                // Process each resolved file
                for filename in &resolved {
                    // let normalized = normalize_path(filename).unwrap_or_else(|_| filename.clone());
                    let normalized = normalize_path(filename)?;

                    resolved_for_replacement.push(filename.clone());

                    // Merge with existing entry if present (most permissive wins)
                    files
                        .entry(normalized.clone())
                        .and_modify(|existing| {
                            existing.syntax_type = existing.syntax_type.merge(stype);
                            if !existing.resolved_files.contains(filename) {
                                existing.resolved_files.push(filename.clone());
                            }
                        })
                        .or_insert_with(|| ParsedFilePattern {
                            resolved_files: vec![filename.clone()],
                            syntax_type: stype,
                        });
                }

                // Track replacement for prompt rewriting - only if glob expansion occurred
                // (i.e., pattern had glob chars). For single literal files (even with
                // trimmed punctuation), keep the prompt as-is to preserve user's original text.
                if contains_glob_chars(&pattern) && !resolved_for_replacement.is_empty() {
                    let replacement = build_replacement_text(&resolved_for_replacement, stype);
                    replacements.push((start_pos, i + prefix_len + consumed, replacement));
                }
            }

            i += prefix_len + consumed;
        } else {
            i += 1;
        }
    }

    // Apply replacements in reverse order
    let modified_prompt = apply_replacements(input, replacements);

    Ok(UnifiedParseResult {
        modified_prompt,
        files,
    })
}

/// Parse all file patterns (@, !, @!) in a single pass
///
/// This unified function handles:
/// - @ syntax (input files - must exist)
/// - ! syntax (output files - can be created)
/// - @! syntax (input+output files)
/// - Glob pattern expansion
/// - Deduplication by normalized path
/// - Prompt rewriting with expanded file lists
///
/// When the same file appears with different syntaxes, the most permissive wins:
/// - @!file (InputOutput) > @file (Input) or !file (Output)
///
/// # Arguments
/// * `input` - The input string potentially containing file patterns
///
/// # Returns
/// A tuple of (modified_prompt, file_references, output_files)
///
/// # Errors
/// Returns error if:
/// - Input file (@) doesn't exist or can't be read
/// - Glob pattern syntax error
/// - Pattern matches no files
///
/// # Examples
///
/// - "@data.csv analyze" → reads data.csv as input
/// - "Create !output.txt" → marks output.txt for writing
/// - "Update @!config.json" → reads config.json as input AND marks for output
/// - "@*.rs review" → reads all .rs files, expands prompt
pub fn parse_file_patterns(
    input: &str,
) -> Result<(String, Vec<FileReference>, Vec<OutputFileSpec>)> {
    let result = parse_all_file_patterns(input)?;

    let mut parsed_files: Vec<(String, ParsedFilePattern)> = result.files.into_iter().collect();
    parsed_files.sort_by(|(left, _), (right, _)| left.cmp(right));

    let mut file_references = Vec::new();
    let mut output_files = Vec::new();

    // Process all parsed files
    for (normalized_path, mut pattern) in parsed_files {
        pattern.resolved_files.sort();
        // Build input file references (@ and @!)
        if pattern.syntax_type.is_input() {
            for filename in &pattern.resolved_files {
                let content = if pattern.syntax_type == FileSyntaxType::InputOutput {
                    // @! syntax - file might not exist yet
                    read_file_if_exists(filename)?
                } else {
                    // @ syntax - file must exist
                    fs::read_to_string(filename).with_context(|| {
                        format!(
                            "Could not read file '{}'. Make sure the file exists and is readable.",
                            filename
                        )
                    })?
                };

                file_references.push(FileReference {
                    filename: filename.clone(),
                    content,
                });
            }
        }

        // Build output file specs (! and @!)
        if pattern.syntax_type.is_output() {
            for filename in &pattern.resolved_files {
                output_files.push(OutputFileSpec {
                    filename: filename.clone(),
                    normalized_path: normalized_path.clone(),
                    include_as_input: pattern.syntax_type == FileSyntaxType::InputOutput,
                });
            }
        }
    }

    Ok((result.modified_prompt, file_references, output_files))
}

/// Extract filename from character slice
///
/// Returns (filename, number of characters consumed)
/// Filenames end at whitespace or end of string
fn extract_filename(chars: &[char]) -> Result<(String, usize)> {
    let mut filename = String::new();
    let mut consumed = 0;

    for &ch in chars {
        if ch.is_whitespace() {
            break;
        }
        filename.push(ch);
        consumed += 1;
    }

    Ok((filename, consumed))
}

/// Read file if it exists, otherwise return empty string
///
/// Used for @!file syntax where file might not exist yet
fn read_file_if_exists(filename: &str) -> Result<String> {
    let path = Path::new(filename);
    if path.exists() {
        fs::read_to_string(path).with_context(|| {
            format!(
                "Could not read file '{}'. Make sure the file is readable.",
                filename
            )
        })
    } else {
        // File doesn't exist yet - return empty content
        Ok(String::new())
    }
}

/// Parse input from command-line arguments and STDIN
///
/// This function handles:
/// - Slash commands for model selection (e.g., "/sonnet explain this")
/// - File references with @-syntax (e.g., "@data.csv analyze this")
/// - Regular prompts
/// - STDIN input (when piped)
///
/// # Examples
///
/// ```no_run
/// // Regular prompt: ["hello", "world"]
/// // Result: ParsedInput { model_override: None, prompt: "hello world", stdin_content: None, file_references: [] }
///
/// // Slash command: ["/sonnet", "explain", "this"]
/// // Result: ParsedInput { model_override: Some("sonnet"), prompt: "explain this", stdin_content: None, file_references: [] }
///
/// // With file reference: ["@data.csv", "analyze", "this"]
/// // Result: ParsedInput { model_override: None, prompt: "@data.csv analyze this", file_references: [FileReference {...}] }
///
/// // With STDIN (when piped): ["analyze"]
/// // Result: ParsedInput { model_override: None, prompt: "analyze", stdin_content: Some("..."), file_references: [] }
/// ```
pub fn parse_input(args: Vec<String>) -> Result<ParsedInput> {
    let stdin_content = read_stdin_if_available().context("Failed to read from STDIN")?;
    parse_input_with_stdin(args, stdin_content)
}

fn parse_input_with_stdin(args: Vec<String>, stdin_content: Option<String>) -> Result<ParsedInput> {
    // Join args into single string
    let joined = args.join(" ");

    // Parse slash command if present
    let (model_override, prompt) = parse_slash_command(&joined);

    // Parse all file patterns in a single pass (@ for input, ! for output, @! for both)
    let (final_prompt, file_references, output_files) =
        parse_file_patterns(&prompt).context("Failed to parse file patterns")?;

    Ok(ParsedInput {
        model_override,
        prompt: final_prompt, // Use expanded prompt
        stdin_content,
        file_references,
        output_files,
    })
}

/// Parse input for image mode.
///
/// Image prompts keep slash-model overrides and piped STDIN behavior, but skip all
/// `@file`, `!file`, and `@!file` parsing so prompt text stays literal.
pub fn parse_image_input(args: Vec<String>) -> Result<ParsedInput> {
    let stdin_content = read_stdin_if_available().context("Failed to read from STDIN")?;
    parse_image_input_with_stdin(args, stdin_content)
}

fn parse_image_input_with_stdin(
    args: Vec<String>,
    stdin_content: Option<String>,
) -> Result<ParsedInput> {
    let joined = args.join(" ");
    let (model_override, prompt) = parse_slash_command(&joined);

    Ok(ParsedInput {
        model_override,
        prompt,
        stdin_content,
        file_references: Vec::new(),
        output_files: Vec::new(),
    })
}

/// Parse slash command from input string
///
/// Returns (model_override, remaining_prompt)
///
/// # Examples
///
/// - "/sonnet hello world" → (Some("sonnet"), "hello world")
/// - "hello world" → (None, "hello world")
/// - "/gpt4o" → (Some("gpt4o"), "")
fn parse_slash_command(input: &str) -> (Option<String>, String) {
    let trimmed = input.trim();

    // Check if input starts with "/"
    if !trimmed.starts_with('/') {
        return (None, trimmed.to_string());
    }

    // Remove leading "/"
    let without_slash = &trimmed[1..];

    // Find the first whitespace to separate model from prompt
    if let Some(space_idx) = without_slash.find(char::is_whitespace) {
        let model = without_slash[..space_idx].to_string();
        let prompt = without_slash[space_idx..].trim().to_string();
        (Some(model), prompt)
    } else {
        // No space found, entire string is the model name
        (Some(without_slash.to_string()), String::new())
    }
}

/// Read from STDIN if it's available (not a terminal)
///
/// Returns None if STDIN is a terminal (interactive mode)
/// Returns Some(content) if STDIN is piped
fn read_stdin_if_available() -> Result<Option<String>> {
    let stdin = io::stdin();

    // Check if STDIN is a terminal (interactive) or a pipe
    if stdin.is_terminal() {
        // STDIN is a terminal, no piped input
        return Ok(None);
    }

    // STDIN is piped, read all content
    let mut buffer = String::new();
    stdin
        .lock()
        .read_to_string(&mut buffer)
        .context("Failed to read STDIN content")?;

    // Return None if buffer is empty, otherwise return the content
    if buffer.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(buffer))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_slash_command_with_prompt() {
        let (model, prompt) = parse_slash_command("/sonnet explain Rust lifetimes");
        assert_eq!(model, Some("sonnet".to_string()));
        assert_eq!(prompt, "explain Rust lifetimes");
    }

    #[test]
    fn test_parse_slash_command_no_prompt() {
        let (model, prompt) = parse_slash_command("/gpt4o");
        assert_eq!(model, Some("gpt4o".to_string()));
        assert_eq!(prompt, "");
    }

    #[test]
    fn test_parse_slash_command_no_slash() {
        let (model, prompt) = parse_slash_command("hello world");
        assert_eq!(model, None);
        assert_eq!(prompt, "hello world");
    }

    #[test]
    fn test_parse_slash_command_with_extra_spaces() {
        let (model, prompt) = parse_slash_command("/sonnet    explain   this");
        assert_eq!(model, Some("sonnet".to_string()));
        assert_eq!(prompt, "explain   this");
    }

    #[test]
    fn test_parse_slash_command_with_leading_trailing_spaces() {
        let (model, prompt) = parse_slash_command("  /flash hello  ");
        assert_eq!(model, Some("flash".to_string()));
        assert_eq!(prompt, "hello");
    }

    #[test]
    fn test_parse_input_simple() {
        let args = vec!["hello".to_string(), "world".to_string()];
        // Note: This test assumes no STDIN is piped (terminal mode)
        // In real terminal environment, this should work
        let result = parse_input(args);
        assert!(result.is_ok());
        let parsed = result.unwrap();
        assert_eq!(parsed.model_override, None);
        assert_eq!(parsed.prompt, "hello world");
        assert_eq!(parsed.file_references.len(), 0);
        // stdin_content depends on whether STDIN is actually piped
    }

    #[test]
    fn test_parse_input_with_slash_command() {
        let args = vec!["/sonnet".to_string(), "test".to_string()];
        let result = parse_input(args);
        assert!(result.is_ok());
        let parsed = result.unwrap();
        assert_eq!(parsed.model_override, Some("sonnet".to_string()));
        assert_eq!(parsed.prompt, "test");
        assert_eq!(parsed.file_references.len(), 0);
    }

    #[test]
    fn test_parse_input_slash_command_no_prompt() {
        let args = vec!["/gpt4".to_string()];
        let result = parse_input(args);
        assert!(result.is_ok());
        let parsed = result.unwrap();
        assert_eq!(parsed.model_override, Some("gpt4".to_string()));
        assert_eq!(parsed.prompt, "");
        assert_eq!(parsed.file_references.len(), 0);
    }

    #[test]
    fn test_parse_image_input_with_slash_command() {
        let parsed =
            parse_image_input_with_stdin(vec!["/flash".to_string(), "draw".to_string()], None)
                .unwrap();

        assert_eq!(parsed.model_override, Some("flash".to_string()));
        assert_eq!(parsed.prompt, "draw");
        assert!(parsed.file_references.is_empty());
        assert!(parsed.output_files.is_empty());
    }

    #[test]
    fn test_parse_image_input_keeps_file_syntax_literal() {
        let parsed = parse_image_input_with_stdin(
            vec![
                "@cat.png".to_string(),
                "literal".to_string(),
                "!output.png".to_string(),
            ],
            None,
        )
        .unwrap();

        assert_eq!(parsed.prompt, "@cat.png literal !output.png");
        assert!(parsed.file_references.is_empty());
        assert!(parsed.output_files.is_empty());
    }

    #[test]
    fn test_parse_image_input_preserves_stdin() {
        let parsed = parse_image_input_with_stdin(
            vec![
                "/flash".to_string(),
                "render".to_string(),
                "@skyline.png".to_string(),
            ],
            Some("from stdin".to_string()),
        )
        .unwrap();

        assert_eq!(parsed.model_override, Some("flash".to_string()));
        assert_eq!(parsed.prompt, "render @skyline.png");
        assert_eq!(parsed.stdin_content.as_deref(), Some("from stdin"));
        assert!(parsed.file_references.is_empty());
        assert!(parsed.output_files.is_empty());
    }

    #[test]
    fn test_parse_file_patterns_input_single_file() {
        // Create a temporary file for testing
        use std::io::Write;
        let temp_file = "test_file_single.txt";
        let mut file = std::fs::File::create(temp_file).unwrap();
        writeln!(file, "test content").unwrap();

        let result = parse_file_patterns("@test_file_single.txt analyze this");
        assert!(result.is_ok());
        let (_prompt, refs, _outputs) = result.unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].filename, "test_file_single.txt");
        assert_eq!(refs[0].content.trim(), "test content");

        // Cleanup
        std::fs::remove_file(temp_file).ok();
    }

    #[test]
    fn test_parse_file_patterns_input_multiple_files() {
        // Create temporary files for testing
        use std::io::Write;
        let temp_file1 = "test_file_multi1.txt";
        let temp_file2 = "test_file_multi2.txt";

        let mut file1 = std::fs::File::create(temp_file1).unwrap();
        writeln!(file1, "content 1").unwrap();

        let mut file2 = std::fs::File::create(temp_file2).unwrap();
        writeln!(file2, "content 2").unwrap();

        let result =
            parse_file_patterns("@test_file_multi1.txt @test_file_multi2.txt compare these");
        assert!(result.is_ok());
        let (_prompt, refs, _outputs) = result.unwrap();
        assert_eq!(refs.len(), 2);

        // Find files by name (order may vary due to HashMap iteration)
        let file1_ref = refs
            .iter()
            .find(|r| r.filename == "test_file_multi1.txt")
            .unwrap();
        let file2_ref = refs
            .iter()
            .find(|r| r.filename == "test_file_multi2.txt")
            .unwrap();

        assert_eq!(file1_ref.content.trim(), "content 1");
        assert_eq!(file2_ref.content.trim(), "content 2");

        // Cleanup
        std::fs::remove_file(temp_file1).ok();
        std::fs::remove_file(temp_file2).ok();
    }

    #[test]
    fn test_parse_file_patterns_no_files() {
        let result = parse_file_patterns("just a regular prompt");
        assert!(result.is_ok());
        let (_prompt, refs, outputs) = result.unwrap();
        assert_eq!(refs.len(), 0);
        assert_eq!(outputs.len(), 0);
    }

    #[test]
    fn test_parse_file_patterns_missing_file() {
        let result = parse_file_patterns("@nonexistent_file.txt analyze");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("nonexistent_file.txt")
        );
    }

    #[test]
    fn test_parse_file_patterns_input_at_end() {
        use std::io::Write;
        let temp_file = "test_file_end.txt";
        let mut file = std::fs::File::create(temp_file).unwrap();
        writeln!(file, "ending content").unwrap();

        let result = parse_file_patterns("analyze @test_file_end.txt");
        assert!(result.is_ok());
        let (_prompt, refs, _outputs) = result.unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].filename, "test_file_end.txt");
        assert_eq!(refs[0].content.trim(), "ending content");

        // Cleanup
        std::fs::remove_file(temp_file).ok();
    }

    #[test]
    fn test_parse_file_patterns_with_slash_command() {
        use std::io::Write;
        let temp_file = "test_file_slash.txt";
        let mut file = std::fs::File::create(temp_file).unwrap();
        writeln!(file, "slash test").unwrap();

        let result = parse_file_patterns("/sonnet @test_file_slash.txt review this");
        assert!(result.is_ok());
        let (_prompt, refs, _outputs) = result.unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].filename, "test_file_slash.txt");

        // Cleanup
        std::fs::remove_file(temp_file).ok();
    }

    #[test]
    fn test_parse_file_patterns_output_write_only() {
        let (prompt, _refs, files) = parse_file_patterns("Create !hello.txt").unwrap();
        assert_eq!(prompt, "Create !hello.txt");
        assert_eq!(files.len(), 1);
        assert!(files[0].filename.ends_with("hello.txt"));
        assert!(!files[0].include_as_input);
    }

    #[test]
    fn test_parse_file_patterns_output_read_write() {
        let (prompt, _refs, files) = parse_file_patterns("Update @!src/main.rs").unwrap();
        assert_eq!(prompt, "Update @!src/main.rs");
        assert_eq!(files.len(), 1);
        assert!(files[0].filename.ends_with("main.rs"));
        assert!(files[0].include_as_input);
    }

    #[test]
    fn test_parse_file_patterns_output_multiple() {
        let (_prompt, _refs, files) = parse_file_patterns("Create !a.txt and !b.txt").unwrap();
        assert_eq!(files.len(), 2);

        // Find files by name (order may vary due to HashMap iteration)
        let a_file = files
            .iter()
            .find(|f| f.filename.ends_with("a.txt"))
            .unwrap();
        let b_file = files
            .iter()
            .find(|f| f.filename.ends_with("b.txt"))
            .unwrap();

        assert!(!a_file.include_as_input);
        assert!(!b_file.include_as_input);
    }

    #[test]
    fn test_parse_file_patterns_output_mixed() {
        use std::io::Write;
        let temp_file = "test_output_old.rs";
        let mut file = std::fs::File::create(temp_file).unwrap();
        writeln!(file, "old content").unwrap();

        let (_prompt, _refs, files) =
            parse_file_patterns("Update @!test_output_old.rs and create !new.rs").unwrap();
        assert_eq!(files.len(), 2);

        // Find files by name (order may vary due to HashMap iteration)
        let old_file = files
            .iter()
            .find(|f| f.filename.ends_with("test_output_old.rs"))
            .unwrap();
        let new_file = files
            .iter()
            .find(|f| f.filename.ends_with("new.rs"))
            .unwrap();

        assert!(old_file.include_as_input); // @! means read+write
        assert!(!new_file.include_as_input); // ! means write only

        // Cleanup
        std::fs::remove_file(temp_file).ok();
    }

    #[test]
    fn test_parse_file_patterns_output_no_files() {
        let (prompt, refs, files) = parse_file_patterns("just a regular prompt").unwrap();
        assert_eq!(prompt, "just a regular prompt");
        assert_eq!(refs.len(), 0);
        assert_eq!(files.len(), 0);
    }

    #[test]
    fn test_extract_filename() {
        let chars: Vec<char> = "hello.txt more text".chars().collect();
        let (filename, consumed) = extract_filename(&chars).unwrap();
        assert_eq!(filename, "hello.txt");
        assert_eq!(consumed, 9);
    }

    #[test]
    fn test_extract_filename_at_end() {
        let chars: Vec<char> = "file.txt".chars().collect();
        let (filename, consumed) = extract_filename(&chars).unwrap();
        assert_eq!(filename, "file.txt");
        assert_eq!(consumed, 8);
    }

    #[test]
    fn test_parse_file_patterns_includes_input_output_syntax() {
        use std::io::Write;

        // Create a test file for @!file syntax
        let temp_file = "test_output_ref.txt";
        let mut file = std::fs::File::create(temp_file).unwrap();
        writeln!(file, "test content").unwrap();

        // @! syntax means read+write, so it should be included in both refs and outputs
        let result = parse_file_patterns("Update @!test_output_ref.txt with new content");
        assert!(result.is_ok());
        let (_prompt, refs, outputs) = result.unwrap();

        // Should have one file reference (because @! includes input)
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].filename, "test_output_ref.txt");
        assert_eq!(refs[0].content.trim(), "test content");

        // Should also have one output file
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].filename, "test_output_ref.txt");
        assert!(outputs[0].include_as_input);

        // Cleanup
        std::fs::remove_file(temp_file).ok();
    }

    #[test]
    fn test_parse_file_patterns_mixed_input_and_output() {
        use std::io::Write;

        // Create test files
        let input_file = "test_input_ref.txt";
        let output_file = "test_output_ref2.txt";

        let mut file1 = std::fs::File::create(input_file).unwrap();
        writeln!(file1, "input content").unwrap();

        let mut file2 = std::fs::File::create(output_file).unwrap();
        writeln!(file2, "output content").unwrap();

        // Mix @file (input) and @!file (input+output) syntax
        let result =
            parse_file_patterns("Read @test_input_ref.txt and update @!test_output_ref2.txt");
        assert!(result.is_ok());
        let (_prompt, refs, outputs) = result.unwrap();

        // Should have both references (@! is included as it means read+write)
        assert_eq!(refs.len(), 2);

        // Find files by name (order may vary due to HashMap iteration)
        let input_ref = refs
            .iter()
            .find(|r| r.filename == "test_input_ref.txt")
            .unwrap();
        let output_ref = refs
            .iter()
            .find(|r| r.filename == "test_output_ref2.txt")
            .unwrap();

        assert_eq!(input_ref.content.trim(), "input content");
        assert_eq!(output_ref.content.trim(), "output content");

        // Should have one output file (only @! file)
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].filename, "test_output_ref2.txt");
        assert!(outputs[0].include_as_input);

        // Cleanup
        std::fs::remove_file(input_file).ok();
        std::fs::remove_file(output_file).ok();
    }

    // === Glob Pattern Tests ===

    #[test]
    fn test_contains_glob_chars() {
        assert!(contains_glob_chars("*.rs"));
        assert!(contains_glob_chars("file?.txt"));
        assert!(contains_glob_chars("test[123].md"));
        assert!(contains_glob_chars("src/{a,b}.rs"));
        assert!(!contains_glob_chars("plain_file.txt"));
        assert!(!contains_glob_chars("path/to/file"));
    }

    #[test]
    fn test_resolve_file_pattern_literal() {
        use std::io::Write;
        // Create temp file
        let temp_file = "test_literal_resolve.txt";
        std::fs::File::create(temp_file)
            .unwrap()
            .write_all(b"content")
            .unwrap();

        let result = resolve_file_pattern(temp_file).unwrap();
        assert_eq!(result, vec![temp_file]);

        std::fs::remove_file(temp_file).ok();
    }

    #[test]
    fn test_resolve_file_pattern_missing() {
        let result = resolve_file_pattern("nonexistent_test_file.txt");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_resolve_file_pattern_glob() {
        use std::io::Write;
        // Create temp files
        std::fs::File::create("test_glob_1.txt")
            .unwrap()
            .write_all(b"one")
            .unwrap();
        std::fs::File::create("test_glob_2.txt")
            .unwrap()
            .write_all(b"two")
            .unwrap();

        let result = resolve_file_pattern("test_glob_*.txt").unwrap();
        assert_eq!(result.len(), 2);
        assert!(result.contains(&"test_glob_1.txt".to_string()));
        assert!(result.contains(&"test_glob_2.txt".to_string()));

        // Cleanup
        std::fs::remove_file("test_glob_1.txt").ok();
        std::fs::remove_file("test_glob_2.txt").ok();
    }

    #[test]
    fn test_resolve_file_pattern_no_matches() {
        let result = resolve_file_pattern("*.nonexistent_extension_xyz");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("matched no files"));
    }

    #[test]
    fn test_glob_pattern_precedence() {
        use std::io::Write;
        // If a literal file named "*.txt" exists (unlikely but possible),
        // it should take precedence over glob interpretation
        std::fs::File::create("*.txt")
            .unwrap()
            .write_all(b"literal")
            .unwrap();

        let result = resolve_file_pattern("*.txt").unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], "*.txt");

        std::fs::remove_file("*.txt").ok();
    }

    #[test]
    fn test_parse_file_patterns_glob_expansion() {
        use std::io::Write;
        // Create test files
        std::fs::File::create("test_ref_1.rs")
            .unwrap()
            .write_all(b"fn test1() {}")
            .unwrap();
        std::fs::File::create("test_ref_2.rs")
            .unwrap()
            .write_all(b"fn test2() {}")
            .unwrap();

        let (modified_prompt, refs, _outputs) =
            parse_file_patterns("analyze @test_ref_*.rs for bugs").unwrap();

        // Should expand pattern in prompt
        assert!(modified_prompt.contains("@test_ref_1.rs"));
        assert!(modified_prompt.contains("@test_ref_2.rs"));
        assert!(modified_prompt.contains("for bugs"));

        // Should load both files
        assert_eq!(refs.len(), 2);

        // Cleanup
        std::fs::remove_file("test_ref_1.rs").ok();
        std::fs::remove_file("test_ref_2.rs").ok();
    }

    #[test]
    fn test_parse_file_patterns_deduplication() {
        use std::io::Write;
        // Create test file with unique name to avoid test interference
        std::fs::File::create("test_dedup_unique_12345.txt")
            .unwrap()
            .write_all(b"content")
            .unwrap();

        // Specify same file via literal and glob pattern
        let (modified_prompt, refs, _outputs) =
            parse_file_patterns("@test_dedup_unique_12345.txt @test_dedup_unique_*.txt").unwrap();

        // Should only load file once
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].filename, "test_dedup_unique_12345.txt");

        // Prompt should show both references (expanded)
        assert!(modified_prompt.contains("@test_dedup_unique_12345.txt"));

        // Cleanup
        std::fs::remove_file("test_dedup_unique_12345.txt").ok();
    }

    #[test]
    fn test_parse_file_patterns_mixed_literal_and_glob() {
        use std::io::Write;
        std::fs::File::create("literal_mixed.txt")
            .unwrap()
            .write_all(b"literal")
            .unwrap();
        std::fs::File::create("glob_a_mixed.txt")
            .unwrap()
            .write_all(b"a")
            .unwrap();
        std::fs::File::create("glob_b_mixed.txt")
            .unwrap()
            .write_all(b"b")
            .unwrap();

        let (modified_prompt, refs, _outputs) =
            parse_file_patterns("check @literal_mixed.txt and @glob_*_mixed.txt").unwrap();

        assert_eq!(refs.len(), 3);
        assert!(modified_prompt.contains("@literal_mixed.txt"));
        assert!(modified_prompt.contains("@glob_a_mixed.txt"));
        assert!(modified_prompt.contains("@glob_b_mixed.txt"));

        // Cleanup
        std::fs::remove_file("literal_mixed.txt").ok();
        std::fs::remove_file("glob_a_mixed.txt").ok();
        std::fs::remove_file("glob_b_mixed.txt").ok();
    }

    #[test]
    fn test_parse_file_patterns_deduplication_multiple_globs() {
        use std::io::Write;
        // Create overlapping files with unique names
        std::fs::File::create("test_dedup_multi.txt")
            .unwrap()
            .write_all(b"1")
            .unwrap();
        std::fs::File::create("test_dedup_multi.rs")
            .unwrap()
            .write_all(b"2")
            .unwrap();
        std::fs::File::create("other_dedup_multi.txt")
            .unwrap()
            .write_all(b"3")
            .unwrap();

        // Two patterns that overlap on test_dedup_multi.txt
        let (_modified_prompt, refs, _outputs) =
            parse_file_patterns("@test_dedup_multi.* @*_dedup_multi.txt").unwrap();

        // test_dedup_multi.txt should appear only once, but test_dedup_multi.rs and other_dedup_multi.txt should also be included
        assert_eq!(refs.len(), 3); // test_dedup_multi.txt, test_dedup_multi.rs, other_dedup_multi.txt

        // Verify each file appears exactly once
        let filenames: Vec<_> = refs.iter().map(|r| r.filename.as_str()).collect();
        assert_eq!(
            filenames
                .iter()
                .filter(|&&f| f == "test_dedup_multi.txt")
                .count(),
            1
        );
        assert!(filenames.contains(&"test_dedup_multi.rs"));
        assert!(filenames.contains(&"other_dedup_multi.txt"));

        // Cleanup
        std::fs::remove_file("test_dedup_multi.txt").ok();
        std::fs::remove_file("test_dedup_multi.rs").ok();
        std::fs::remove_file("other_dedup_multi.txt").ok();
    }

    #[test]
    fn test_parse_file_patterns_output_deduplication() {
        use std::io::Write;
        // Create test file
        std::fs::File::create("test_out_dedup.py")
            .unwrap()
            .write_all(b"content")
            .unwrap();

        // Specify same file twice via different patterns
        let (_modified_prompt, _refs, files) =
            parse_file_patterns("!test_out_dedup.py !test_out_dedup*.py").unwrap();

        // Should only have one OutputFileSpec for test_out_dedup.py
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].filename, "test_out_dedup.py");
        assert!(!files[0].include_as_input);

        // Cleanup
        std::fs::remove_file("test_out_dedup.py").ok();
    }

    #[test]
    fn test_resolve_output_file_pattern_allows_missing() {
        // For output files, non-existing literal files should be allowed
        let result = resolve_output_file_pattern("new_file_to_create.txt");
        assert!(result.is_ok());
        let files = result.unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0], "new_file_to_create.txt");
    }

    #[test]
    fn test_resolve_output_file_pattern_glob_requires_matches() {
        // Even for output files, glob patterns must match at least one file
        let result = resolve_output_file_pattern("*.nonexistent_xyz");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("matched no files"));
    }

    #[test]
    fn test_parse_file_patterns_unified_mixed_syntax() {
        use std::io::Write;

        // Create test files
        std::fs::File::create("test_unified_a.txt")
            .unwrap()
            .write_all(b"content a")
            .unwrap();
        std::fs::File::create("test_unified_b.txt")
            .unwrap()
            .write_all(b"content b")
            .unwrap();

        // Mix all three syntaxes
        let input = "@test_unified_a.txt @!test_unified_b.txt !test_unified_c.txt analyze";

        let (_, file_refs, output_specs) = parse_file_patterns(input).unwrap();

        // @test_unified_a.txt should appear in file_refs
        assert_eq!(
            file_refs
                .iter()
                .filter(|f| f.filename == "test_unified_a.txt")
                .count(),
            1
        );

        // @!test_unified_b.txt should appear in both
        assert_eq!(
            file_refs
                .iter()
                .filter(|f| f.filename == "test_unified_b.txt")
                .count(),
            1
        );
        assert_eq!(
            output_specs
                .iter()
                .filter(|f| f.filename == "test_unified_b.txt")
                .count(),
            1
        );

        // !test_unified_c.txt should appear in output_specs only
        assert_eq!(
            output_specs
                .iter()
                .filter(|f| f.filename == "test_unified_c.txt")
                .count(),
            1
        );
        assert_eq!(
            file_refs
                .iter()
                .filter(|f| f.filename == "test_unified_c.txt")
                .count(),
            0
        );

        // Cleanup
        std::fs::remove_file("test_unified_a.txt").ok();
        std::fs::remove_file("test_unified_b.txt").ok();
    }

    #[test]
    fn test_parse_file_patterns_syntax_precedence_permissive_wins() {
        use std::io::Write;

        // Create test file
        std::fs::File::create("test_precedence.txt")
            .unwrap()
            .write_all(b"content")
            .unwrap();

        // Specify same file with different syntaxes - @! should win
        let input = "@test_precedence.txt !test_precedence.txt @!test_precedence.txt";

        let (_, file_refs, output_specs) = parse_file_patterns(input).unwrap();

        // Should appear exactly once in each (deduplicated)
        assert_eq!(file_refs.len(), 1);
        assert_eq!(output_specs.len(), 1);

        // Should be marked as include_as_input (because @! is most permissive)
        assert!(output_specs[0].include_as_input);

        // Cleanup
        std::fs::remove_file("test_precedence.txt").ok();
    }

    #[test]
    fn test_parse_file_patterns_deduplication_with_globs_across_syntax() {
        use std::io::Write;

        // Create test file
        std::fs::File::create("test_dedup_glob_unified.rs")
            .unwrap()
            .write_all(b"code")
            .unwrap();

        // Specify via literal @ and glob @! - should deduplicate and @! wins
        let input = "@test_dedup_glob_unified.rs @!*.rs analyze";

        let (_, file_refs, output_specs) = parse_file_patterns(input).unwrap();

        // Should only appear once in file_refs
        let count = file_refs
            .iter()
            .filter(|f| f.filename == "test_dedup_glob_unified.rs")
            .count();
        assert_eq!(count, 1);

        // Should appear in output_specs with include_as_input=true (because @! wins)
        let output_file = output_specs
            .iter()
            .find(|f| f.filename == "test_dedup_glob_unified.rs")
            .unwrap();
        assert!(output_file.include_as_input);

        // Cleanup
        std::fs::remove_file("test_dedup_glob_unified.rs").ok();
    }

    // === Trailing Punctuation Tests ===

    #[test]
    fn test_parse_file_patterns_trailing_comma() {
        use std::io::Write;
        let temp_file = "test_trailing_comma.txt";
        std::fs::File::create(temp_file)
            .unwrap()
            .write_all(b"content")
            .unwrap();

        // File specified with trailing comma (common in lists)
        let result = parse_file_patterns("look at @test_trailing_comma.txt, please");
        assert!(result.is_ok());
        let (prompt, refs, _outputs) = result.unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].filename, "test_trailing_comma.txt"); // Trimmed

        // Prompt should preserve original (with comma)
        assert!(prompt.contains("@test_trailing_comma.txt,"));

        std::fs::remove_file(temp_file).ok();
    }

    #[test]
    fn test_parse_file_patterns_trailing_period() {
        use std::io::Write;
        let temp_file = "test_trailing_period.txt";
        std::fs::File::create(temp_file)
            .unwrap()
            .write_all(b"content")
            .unwrap();

        // File at end of sentence with period
        let result = parse_file_patterns("analyze @test_trailing_period.txt.");
        assert!(result.is_ok());
        let (prompt, refs, _outputs) = result.unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].filename, "test_trailing_period.txt");

        // Prompt preserves original
        assert!(prompt.contains("@test_trailing_period.txt."));

        std::fs::remove_file(temp_file).ok();
    }

    #[test]
    fn test_parse_file_patterns_trailing_semicolon() {
        use std::io::Write;
        let temp_file = "test_trailing_semi.txt";
        std::fs::File::create(temp_file)
            .unwrap()
            .write_all(b"content")
            .unwrap();

        let result = parse_file_patterns("check @test_trailing_semi.txt; then continue");
        assert!(result.is_ok());
        let (_prompt, refs, _outputs) = result.unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].filename, "test_trailing_semi.txt");

        std::fs::remove_file(temp_file).ok();
    }

    #[test]
    fn test_parse_file_patterns_trailing_paren() {
        use std::io::Write;
        let temp_file = "test_trailing_paren.txt";
        std::fs::File::create(temp_file)
            .unwrap()
            .write_all(b"content")
            .unwrap();

        let result = parse_file_patterns("see @test_trailing_paren.txt) for details");
        assert!(result.is_ok());
        let (_prompt, refs, _outputs) = result.unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].filename, "test_trailing_paren.txt");

        std::fs::remove_file(temp_file).ok();
    }

    #[test]
    fn test_parse_file_patterns_trailing_colon() {
        use std::io::Write;
        let temp_file = "test_trailing_colon.txt";
        std::fs::File::create(temp_file)
            .unwrap()
            .write_all(b"content")
            .unwrap();

        let result = parse_file_patterns("from @test_trailing_colon.txt: extract data");
        assert!(result.is_ok());
        let (_prompt, refs, _outputs) = result.unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].filename, "test_trailing_colon.txt");

        std::fs::remove_file(temp_file).ok();
    }

    #[test]
    fn test_parse_file_patterns_multiple_trailing_punctuation() {
        use std::io::Write;
        let temp_file = "test_multi_punct.txt";
        std::fs::File::create(temp_file)
            .unwrap()
            .write_all(b"content")
            .unwrap();

        // Multiple punctuation chars at end
        let result = parse_file_patterns("see @test_multi_punct.txt,.");
        assert!(result.is_ok());
        let (_prompt, refs, _outputs) = result.unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].filename, "test_multi_punct.txt");

        std::fs::remove_file(temp_file).ok();
    }

    #[test]
    fn test_parse_file_patterns_multiple_files_with_commas() {
        use std::io::Write;
        let file1 = "test_comma_list1.txt";
        let file2 = "test_comma_list2.txt";
        let file3 = "test_comma_list3.txt";

        std::fs::File::create(file1)
            .unwrap()
            .write_all(b"one")
            .unwrap();
        std::fs::File::create(file2)
            .unwrap()
            .write_all(b"two")
            .unwrap();
        std::fs::File::create(file3)
            .unwrap()
            .write_all(b"three")
            .unwrap();

        // Common user pattern: listing files with commas
        let result = parse_file_patterns(
            "take a look at @test_comma_list1.txt, @test_comma_list2.txt and @test_comma_list3.txt",
        );
        assert!(result.is_ok());
        let (_prompt, refs, _outputs) = result.unwrap();
        assert_eq!(refs.len(), 3);

        // Verify all files were found with trimmed names
        let filenames: Vec<_> = refs.iter().map(|r| r.filename.as_str()).collect();
        assert!(filenames.contains(&"test_comma_list1.txt"));
        assert!(filenames.contains(&"test_comma_list2.txt"));
        assert!(filenames.contains(&"test_comma_list3.txt"));

        std::fs::remove_file(file1).ok();
        std::fs::remove_file(file2).ok();
        std::fs::remove_file(file3).ok();
    }

    #[test]
    fn test_parse_file_patterns_output_file_trims_punctuation() {
        // Output files (!) should now trim punctuation to avoid LLM confusion
        let result = parse_file_patterns("create !output_punct.txt,");
        assert!(result.is_ok());
        let (_prompt, _refs, outputs) = result.unwrap();
        assert_eq!(outputs.len(), 1);
        // Output files should trim trailing punctuation
        assert_eq!(outputs[0].filename, "output_punct.txt");
    }

    #[test]
    fn test_parse_file_patterns_input_output_trims_punctuation() {
        use std::io::Write;
        let temp_file = "test_at_bang_punct.txt";
        std::fs::File::create(temp_file)
            .unwrap()
            .write_all(b"content")
            .unwrap();

        // @! syntax should also trim punctuation (it's an input file too)
        let result = parse_file_patterns("update @!test_at_bang_punct.txt,");
        assert!(result.is_ok());
        let (_prompt, refs, outputs) = result.unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].filename, "test_at_bang_punct.txt");
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].filename, "test_at_bang_punct.txt");

        std::fs::remove_file(temp_file).ok();
    }

    #[test]
    fn test_parse_file_patterns_no_trim_if_file_exists_with_punctuation() {
        use std::io::Write;
        // Edge case: file actually named with trailing comma (unlikely but possible)
        let temp_file = "test_actual_comma.txt,";
        std::fs::File::create(temp_file)
            .unwrap()
            .write_all(b"special")
            .unwrap();

        let result = parse_file_patterns("check @test_actual_comma.txt,");
        assert!(result.is_ok());
        let (_prompt, refs, _outputs) = result.unwrap();
        assert_eq!(refs.len(), 1);
        // Should use the actual filename (with comma) since it exists
        assert_eq!(refs[0].filename, "test_actual_comma.txt,");

        std::fs::remove_file(temp_file).ok();
    }

    #[test]
    fn test_parse_file_patterns_output_file_multiple_punctuation() {
        // Test multiple trailing punctuation chars on output file
        let result = parse_file_patterns("create !result.json,.");
        assert!(result.is_ok());
        let (_prompt, _refs, outputs) = result.unwrap();
        assert_eq!(outputs.len(), 1);
        // Should trim both comma and period
        assert_eq!(outputs[0].filename, "result.json");
    }

    #[test]
    fn test_parse_file_patterns_output_file_natural_sentence() {
        // Common case: output file in natural sentence
        let result = parse_file_patterns("please create !summary.md.");
        assert!(result.is_ok());
        let (_prompt, _refs, outputs) = result.unwrap();
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].filename, "summary.md");
    }

    #[test]
    fn test_parse_file_patterns_output_file_with_semicolon() {
        // Output file followed by semicolon
        let result = parse_file_patterns("write to !output.txt; then stop");
        assert!(result.is_ok());
        let (_prompt, _refs, outputs) = result.unwrap();
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].filename, "output.txt");
    }

    #[test]
    fn test_parse_file_patterns_output_file_existing_with_punctuation() {
        use std::io::Write;
        // Edge case: output file exists with punctuation in name
        let temp_file = "test_out_comma.txt,";
        std::fs::File::create(temp_file)
            .unwrap()
            .write_all(b"existing")
            .unwrap();

        let result = parse_file_patterns("update !test_out_comma.txt,");
        assert!(result.is_ok());
        let (_prompt, _refs, outputs) = result.unwrap();
        assert_eq!(outputs.len(), 1);
        // Should keep punctuation if file actually exists with it
        assert_eq!(outputs[0].filename, "test_out_comma.txt,");

        std::fs::remove_file(temp_file).ok();
    }
}
#[cfg(test)]
mod integration_test {
    use crate::input::parse_input;
    use std::fs;
    use std::io::Write;

    #[test]
    fn test_parse_input_with_read_write_file() {
        // Create test file
        let test_file = "test_integration_rw.txt";
        let mut file = fs::File::create(test_file).unwrap();
        writeln!(file, "existing content").unwrap();
        drop(file);

        // Parse input with @!file syntax
        let args = vec![
            "Update".to_string(),
            format!("@!{}", test_file),
            "with new data".to_string(),
        ];
        let result = parse_input(args);

        assert!(result.is_ok(), "parse_input should succeed");
        let parsed = result.unwrap();

        // Should have 1 output file
        assert_eq!(parsed.output_files.len(), 1, "Should have 1 output file");
        assert_eq!(parsed.output_files[0].filename, test_file);
        assert!(
            parsed.output_files[0].include_as_input,
            "Should be marked for input"
        );

        // Should have 1 file reference with the content
        assert_eq!(
            parsed.file_references.len(),
            1,
            "Should have 1 file reference"
        );
        assert_eq!(parsed.file_references[0].filename, test_file);
        assert_eq!(parsed.file_references[0].content.trim(), "existing content");

        // Cleanup
        fs::remove_file(test_file).ok();
    }
}
