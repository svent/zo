use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use grep::regex::RegexMatcherBuilder;
use grep::searcher::SearcherBuilder;
use grep::searcher::sinks::UTF8;
use ignore::WalkBuilder;
use openrouter_rs::types::typed_tool::TypedTool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Component, Path, PathBuf};

const MAX_TOOL_OUTPUT_CHARS: usize = 24_000;
const MAX_LIST_FILES_RESULTS: usize = 200;
const MAX_FIND_RESULTS: usize = 200;
const MAX_GREP_MATCHES: usize = 200;
const MAX_GREP_LINE_CHARS: usize = 300;
const MAX_READ_FILE_LINES: usize = 400;
const MAX_READ_FILE_CHARS: usize = 24_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ToolsCliMode {
    Ro,
    Rw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolMode {
    Disabled,
    ReadOnly,
    ReadWrite,
}

impl From<Option<ToolsCliMode>> for ToolMode {
    fn from(value: Option<ToolsCliMode>) -> Self {
        match value {
            None => ToolMode::Disabled,
            Some(ToolsCliMode::Ro) => ToolMode::ReadOnly,
            Some(ToolsCliMode::Rw) => ToolMode::ReadWrite,
        }
    }
}

/// Parameters for the write_file tool
#[derive(Serialize, Deserialize, JsonSchema, Debug)]
pub struct WriteFileParams {
    /// The file path to write to
    pub path: String,
    /// The content to write to the file
    pub content: String,
}

impl TypedTool for WriteFileParams {
    fn name() -> &'static str {
        "write_file"
    }

    fn description() -> &'static str {
        "Write full file content to a file path. Use this for complete file creation or replacement."
    }
}

/// Parameters for list_files(path)
#[derive(Serialize, Deserialize, JsonSchema, Debug)]
pub struct ListFilesParams {
    /// Directory path to list (relative to workspace root)
    pub path: String,
}

impl TypedTool for ListFilesParams {
    fn name() -> &'static str {
        "list_files"
    }

    fn description() -> &'static str {
        "List direct children of a directory path in the current workspace."
    }
}

/// Parameters for find(glob)
#[derive(Serialize, Deserialize, JsonSchema, Debug)]
pub struct FindParams {
    /// Glob pattern to match against workspace-relative paths
    pub glob: String,
}

impl TypedTool for FindParams {
    fn name() -> &'static str {
        "find"
    }

    fn description() -> &'static str {
        "Find files matching a glob pattern within the current workspace."
    }
}

/// Parameters for grep_regex(pattern, path_glob)
#[derive(Serialize, Deserialize, JsonSchema, Debug)]
pub struct GrepRegexParams {
    /// Rust regular expression pattern
    pub pattern: String,
    /// Glob used to filter candidate file paths
    pub path_glob: String,
}

impl TypedTool for GrepRegexParams {
    fn name() -> &'static str {
        "grep_regex"
    }

    fn description() -> &'static str {
        "Search file contents with a regex pattern. Returns file:line:match snippets."
    }
}

/// Parameters for grep_exact(text, path_glob)
#[derive(Serialize, Deserialize, JsonSchema, Debug)]
pub struct GrepExactParams {
    /// Exact text to search for
    pub text: String,
    /// Glob used to filter candidate file paths
    pub path_glob: String,
}

impl TypedTool for GrepExactParams {
    fn name() -> &'static str {
        "grep_exact"
    }

    fn description() -> &'static str {
        "Search file contents for exact text. Returns file:line:match snippets."
    }
}

/// Parameters for read_file(path, start_line, end_line)
#[derive(Serialize, Deserialize, JsonSchema, Debug)]
pub struct ReadFileParams {
    /// File path to read
    pub path: String,
    /// 1-based start line (optional)
    pub start_line: Option<usize>,
    /// 1-based end line inclusive (optional)
    pub end_line: Option<usize>,
}

impl TypedTool for ReadFileParams {
    fn name() -> &'static str {
        "read_file"
    }

    fn description() -> &'static str {
        "Read a file with optional 1-based inclusive line range."
    }
}

/// Parameters for edit_file(path, old_string, new_string)
#[derive(Serialize, Deserialize, JsonSchema, Debug)]
pub struct EditFileParams {
    /// File path to modify
    pub path: String,
    /// Existing text to replace (must match exactly once)
    pub old_string: String,
    /// Replacement text
    pub new_string: String,
}

impl TypedTool for EditFileParams {
    fn name() -> &'static str {
        "edit_file"
    }

    fn description() -> &'static str {
        "Replace exactly one matching text occurrence in a file. Fails on zero or multiple matches."
    }
}

/// Parameters for replace_lines(path, start_line, end_line, new_content)
#[derive(Serialize, Deserialize, JsonSchema, Debug)]
pub struct ReplaceLinesParams {
    /// File path to modify
    pub path: String,
    /// 1-based inclusive start line
    pub start_line: usize,
    /// 1-based inclusive end line
    pub end_line: usize,
    /// Replacement content for the line range
    pub new_content: String,
}

impl TypedTool for ReplaceLinesParams {
    fn name() -> &'static str {
        "replace_lines"
    }

    fn description() -> &'static str {
        "Replace an inclusive line range in a file with new content."
    }
}

fn workspace_root() -> Result<PathBuf> {
    std::env::current_dir()
        .context("Failed to resolve current working directory")?
        .canonicalize()
        .context("Failed to canonicalize current working directory")
}

fn ensure_relative_no_traversal(path: &str) -> Result<PathBuf> {
    let path_buf = PathBuf::from(path);

    if path_buf.is_absolute() {
        bail!("Absolute paths not allowed: {}", path);
    }

    if path_buf
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        bail!("Path traversal not allowed: {}", path);
    }

    Ok(path_buf)
}

fn ensure_within_workspace(path: &Path, root: &Path, original: &str) -> Result<()> {
    if !path.starts_with(root) {
        bail!(
            "Path '{}' resolves outside workspace root '{}': {}",
            original,
            root.display(),
            path.display()
        );
    }

    Ok(())
}

pub fn resolve_workspace_path(path: &str, allow_missing: bool) -> Result<PathBuf> {
    let relative = ensure_relative_no_traversal(path)?;
    let root = workspace_root()?;
    let full_path = root.join(&relative);

    if full_path.exists() {
        let canonical = full_path
            .canonicalize()
            .with_context(|| format!("Failed to canonicalize path: {}", full_path.display()))?;
        ensure_within_workspace(&canonical, &root, path)?;
        return Ok(canonical);
    }

    if !allow_missing {
        bail!("File not found: {}", path);
    }

    let parent = full_path
        .parent()
        .with_context(|| format!("Invalid path (missing parent): {}", path))?;

    if !parent.exists() {
        bail!(
            "Parent directory does not exist: {}. Create it first with: mkdir -p {}",
            parent.display(),
            parent.display()
        );
    }

    let canonical_parent = parent.canonicalize().with_context(|| {
        format!(
            "Failed to canonicalize parent directory: {}",
            parent.display()
        )
    })?;
    ensure_within_workspace(&canonical_parent, &root, path)?;

    let file_name = full_path
        .file_name()
        .with_context(|| format!("Invalid filename in path: {}", path))?;
    Ok(canonical_parent.join(file_name))
}

/// Normalize file path (resolve relative paths, prevent traversal)
///
/// Security rules:
/// - Absolute paths are rejected
/// - Path traversal (..) is blocked
/// - Parent directory must exist for new files
/// - Canonical target must remain within current workspace root
pub fn normalize_path(path: &str) -> Result<String> {
    Ok(resolve_workspace_path(path, true)?
        .to_string_lossy()
        .to_string())
}

fn relative_display_path(path: &Path) -> Result<String> {
    let root = workspace_root()?;
    let relative = path
        .strip_prefix(&root)
        .with_context(|| format!("Path not under workspace root: {}", path.display()))?;

    if relative.as_os_str().is_empty() {
        Ok(".".to_string())
    } else {
        Ok(relative.to_string_lossy().to_string())
    }
}

fn has_hidden_component(path: &Path) -> bool {
    path.components().any(|component| match component {
        Component::Normal(name) => name.to_string_lossy().starts_with('.'),
        _ => false,
    })
}

pub fn enforce_tool_path_policy(resolved: &Path, original: &str, allow_hidden: bool) -> Result<()> {
    let root = workspace_root()?;
    ensure_within_workspace(resolved, &root, original)?;

    if allow_hidden {
        return Ok(());
    }

    let relative = resolved
        .strip_prefix(&root)
        .with_context(|| format!("Path not under workspace root: {}", resolved.display()))?;

    if has_hidden_component(relative) {
        bail!(
            "Hidden path access is disabled by default. Re-run with --hidden to access: {}",
            original
        );
    }

    Ok(())
}

fn walk_builder(root: &Path, allow_hidden: bool) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(!allow_hidden)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .parents(true);
    builder
}

pub fn clamp_tool_output(output: String) -> String {
    if output.chars().count() <= MAX_TOOL_OUTPUT_CHARS {
        return output;
    }

    let truncated: String = output.chars().take(MAX_TOOL_OUTPUT_CHARS).collect();
    format!(
        "{}\n\n[truncated: response exceeded {} characters]",
        truncated, MAX_TOOL_OUTPUT_CHARS
    )
}

pub fn run_list_files(path: &str, allow_hidden: bool) -> Result<String> {
    let resolved = resolve_workspace_path(path, false)
        .with_context(|| format!("Invalid path for list_files: {}", path))?;
    enforce_tool_path_policy(&resolved, path, allow_hidden)?;

    if resolved.is_file() {
        let display = relative_display_path(&resolved)?;
        return Ok(format!("1 entry\n{}", display));
    }

    if !resolved.is_dir() {
        bail!("Path is not a file or directory: {}", path);
    }

    let mut entries = Vec::new();
    let mut omitted = 0usize;

    for result in walk_builder(&resolved, allow_hidden)
        .max_depth(Some(1))
        .build()
    {
        let dent = match result {
            Ok(dent) => dent,
            Err(err) => {
                omitted += 1;
                eprintln!("Warning: list_files skipped entry: {}", err);
                continue;
            }
        };

        let dent_path = dent.path();
        if dent_path == resolved {
            continue;
        }

        if entries.len() >= MAX_LIST_FILES_RESULTS {
            omitted += 1;
            continue;
        }

        let mut display = relative_display_path(dent_path)?;
        if dent.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
            display.push('/');
        }
        entries.push(display);
    }

    entries.sort();

    let mut output = format!("{} entries", entries.len());
    if omitted > 0 {
        output.push_str(&format!(" ({} omitted)", omitted));
    }

    if !entries.is_empty() {
        output.push('\n');
        output.push_str(&entries.join("\n"));
    }

    Ok(clamp_tool_output(output))
}

pub fn run_find(glob_pattern: &str, allow_hidden: bool) -> Result<String> {
    let pattern = glob::Pattern::new(glob_pattern)
        .with_context(|| format!("Invalid glob pattern: {}", glob_pattern))?;

    let root = workspace_root()?;
    let mut matches = Vec::new();
    let mut omitted = 0usize;

    for result in walk_builder(&root, allow_hidden).build() {
        let dent = match result {
            Ok(dent) => dent,
            Err(err) => {
                omitted += 1;
                eprintln!("Warning: find skipped entry: {}", err);
                continue;
            }
        };

        if !dent.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            continue;
        }

        let rel = relative_display_path(dent.path())?;
        if !pattern.matches_path(Path::new(&rel)) {
            continue;
        }

        if matches.len() >= MAX_FIND_RESULTS {
            omitted += 1;
            continue;
        }

        matches.push(rel);
    }

    matches.sort();

    let mut output = format!("{} matches", matches.len());
    if omitted > 0 {
        output.push_str(&format!(" ({} omitted)", omitted));
    }

    if !matches.is_empty() {
        output.push('\n');
        output.push_str(&matches.join("\n"));
    }

    Ok(clamp_tool_output(output))
}

fn run_grep(pattern: &str, path_glob: &str, exact: bool, allow_hidden: bool) -> Result<String> {
    if pattern.is_empty() {
        bail!("Pattern must not be empty");
    }

    let path_pattern = glob::Pattern::new(path_glob)
        .with_context(|| format!("Invalid path_glob pattern: {}", path_glob))?;

    let regex_pattern = if exact {
        regex::escape(pattern)
    } else {
        pattern.to_string()
    };

    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(false)
        .build(&regex_pattern)
        .with_context(|| format!("Invalid regex pattern: {}", pattern))?;

    let root = workspace_root()?;
    let mut searcher = SearcherBuilder::new().line_number(true).build();
    let mut matches = Vec::new();
    let mut omitted = 0usize;

    'walk: for result in walk_builder(&root, allow_hidden).build() {
        let dent = match result {
            Ok(dent) => dent,
            Err(err) => {
                omitted += 1;
                eprintln!("Warning: grep skipped entry: {}", err);
                continue;
            }
        };

        if !dent.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            continue;
        }

        let rel = relative_display_path(dent.path())?;
        if !path_pattern.matches_path(Path::new(&rel)) {
            continue;
        }

        let rel_for_line = rel.clone();
        searcher
            .search_path(
                &matcher,
                dent.path(),
                UTF8(|line_number, line| {
                    if matches.len() >= MAX_GREP_MATCHES {
                        omitted += 1;
                        return Ok(false);
                    }

                    let line_text = line.trim_end_matches(['\n', '\r']);
                    if line_text.chars().count() > MAX_GREP_LINE_CHARS {
                        let truncated: String =
                            line_text.chars().take(MAX_GREP_LINE_CHARS).collect();
                        matches.push(format!("{}:{}:{}...", rel_for_line, line_number, truncated));
                    } else {
                        matches.push(format!("{}:{}:{}", rel_for_line, line_number, line_text));
                    }

                    Ok(matches.len() < MAX_GREP_MATCHES)
                }),
            )
            .with_context(|| format!("Failed to search file: {}", rel))?;

        if matches.len() >= MAX_GREP_MATCHES {
            break 'walk;
        }
    }

    matches.sort();

    let mut output = format!("{} matches", matches.len());
    if omitted > 0 {
        output.push_str(&format!(" ({} omitted)", omitted));
    }

    if !matches.is_empty() {
        output.push('\n');
        output.push_str(&matches.join("\n"));
    }

    Ok(clamp_tool_output(output))
}

pub fn run_grep_regex(pattern: &str, path_glob: &str, allow_hidden: bool) -> Result<String> {
    run_grep(pattern, path_glob, false, allow_hidden)
}

pub fn run_grep_exact(text: &str, path_glob: &str, allow_hidden: bool) -> Result<String> {
    run_grep(text, path_glob, true, allow_hidden)
}

pub fn run_read_file(
    path: &str,
    start_line: Option<usize>,
    end_line: Option<usize>,
    allow_hidden: bool,
) -> Result<String> {
    let resolved = resolve_workspace_path(path, false)
        .with_context(|| format!("Invalid path for read_file: {}", path))?;
    enforce_tool_path_policy(&resolved, path, allow_hidden)?;

    if !resolved.is_file() {
        bail!("Path is not a readable file: {}", path);
    }

    let content =
        fs::read_to_string(&resolved).with_context(|| format!("Failed to read file: {}", path))?;

    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();

    if total_lines == 0 {
        return Ok("0 lines\n".to_string());
    }

    let start = start_line.unwrap_or(1);
    if start == 0 {
        bail!("start_line must be >= 1");
    }

    let end = end_line.unwrap_or(total_lines);
    if end == 0 {
        bail!("end_line must be >= 1");
    }

    if end < start {
        bail!("end_line must be >= start_line");
    }

    if start > total_lines {
        bail!(
            "start_line {} out of range for file with {} lines",
            start,
            total_lines
        );
    }

    let bounded_end = end.min(total_lines);
    let mut selected: Vec<(usize, &str)> = (start..=bounded_end)
        .map(|line_num| (line_num, lines[line_num - 1]))
        .collect();

    let mut omitted = 0usize;

    if selected.len() > MAX_READ_FILE_LINES {
        omitted = selected.len() - MAX_READ_FILE_LINES;
        selected.truncate(MAX_READ_FILE_LINES);
    }

    let mut rendered = String::new();
    for (line_num, line) in &selected {
        rendered.push_str(&format!("{:>6}: {}\n", line_num, line));
    }

    if rendered.chars().count() > MAX_READ_FILE_CHARS {
        rendered = rendered.chars().take(MAX_READ_FILE_CHARS).collect();
        omitted = omitted.max(1);
    }

    let mut header = format!(
        "{} lines (showing {}-{} of {})",
        selected.len(),
        selected.first().map(|(n, _)| *n).unwrap_or(0),
        selected.last().map(|(n, _)| *n).unwrap_or(0),
        total_lines
    );

    if omitted > 0 {
        header.push_str(&format!(" ({} omitted)", omitted));
    }

    Ok(clamp_tool_output(format!("{}\n{}", header, rendered)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_normalize_path_relative() {
        let path = normalize_path("test.txt").unwrap();
        assert!(!path.contains(".."));
        assert!(path.ends_with("test.txt"));
    }

    #[test]
    fn test_normalize_path_prevents_traversal() {
        let result = normalize_path("../../etc/passwd");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Path traversal"));
    }

    #[test]
    fn test_normalize_path_prevents_absolute() {
        let result = normalize_path("/etc/passwd");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Absolute paths"));
    }

    #[cfg(unix)]
    #[test]
    fn test_normalize_path_blocks_symlink_escape() {
        use std::os::unix::fs::symlink;

        let outside_target = "/tmp/zo_symlink_escape_target.txt";
        let link_path = "test_symlink_escape_link.txt";

        fs::write(outside_target, "outside").unwrap();
        let _ = fs::remove_file(link_path);
        symlink(outside_target, link_path).unwrap();

        let result = normalize_path(link_path);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("outside workspace")
        );

        fs::remove_file(link_path).ok();
        fs::remove_file(outside_target).ok();
    }

    #[test]
    fn test_run_read_file_range() {
        let test_file = "test_tool_read_file.txt";
        let mut file = fs::File::create(test_file).unwrap();
        writeln!(file, "line1").unwrap();
        writeln!(file, "line2").unwrap();
        writeln!(file, "line3").unwrap();

        let out = run_read_file(test_file, Some(2), Some(3), false).unwrap();
        assert!(out.contains("2: line2"));
        assert!(out.contains("3: line3"));
        assert!(!out.contains("1: line1"));

        fs::remove_file(test_file).ok();
    }

    #[test]
    fn test_enforce_tool_path_policy_blocks_hidden_by_default() {
        let hidden_file = ".test_hidden_policy_block.txt";
        fs::write(hidden_file, "secret").unwrap();

        let resolved = resolve_workspace_path(hidden_file, false).unwrap();
        let blocked = enforce_tool_path_policy(&resolved, hidden_file, false);
        assert!(blocked.is_err());
        assert!(blocked.unwrap_err().to_string().contains("--hidden"));

        let allowed = enforce_tool_path_policy(&resolved, hidden_file, true);
        assert!(allowed.is_ok());

        fs::remove_file(hidden_file).ok();
    }

    #[test]
    fn test_run_find_hides_hidden_files_unless_enabled() {
        let hidden_file = ".test_hidden_find_case.txt";
        fs::write(hidden_file, "secret").unwrap();

        let blocked = run_find(".test_hidden_find_case.txt", false).unwrap();
        assert!(blocked.starts_with("0 matches"));

        let allowed = run_find(".test_hidden_find_case.txt", true).unwrap();
        assert!(allowed.contains(hidden_file));

        fs::remove_file(hidden_file).ok();
    }
}
