use anyhow::{Context, Result, bail};
use openrouter_rs::types::typed_tool::TypedTool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Component, PathBuf};

/// Parameters for the save_file tool
#[derive(Serialize, Deserialize, JsonSchema, Debug)]
pub struct SaveFileParams {
    /// The file path to write to
    pub path: String,
    /// The content to write to the file
    pub content: String,
}

impl TypedTool for SaveFileParams {
    fn name() -> &'static str {
        "save_file"
    }

    fn description() -> &'static str {
        "Save content to a file. Only use for files explicitly requested by the user."
    }
}

/// Normalize file path (resolve relative paths, prevent traversal)
///
/// Security rules:
/// - Absolute paths are rejected
/// - Path traversal (..) is blocked
/// - Parent directory must exist for new files
/// - Paths are canonicalized for comparison
pub fn normalize_path(path: &str) -> Result<String> {
    let path_buf = PathBuf::from(path);

    // Prevent absolute paths outside current directory
    if path_buf.is_absolute() {
        bail!("Absolute paths not allowed: {}", path);
    }

    // Check for path traversal attempts
    if path_buf
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        bail!("Path traversal not allowed: {}", path);
    }

    // Resolve relative path
    let current_dir = std::env::current_dir().context("Failed to get current directory")?;
    let full_path = current_dir.join(&path_buf);

    // Canonicalize if exists, otherwise normalize parent
    if full_path.exists() {
        Ok(full_path.canonicalize()?.to_string_lossy().to_string())
    } else {
        // For new files, ensure parent directory exists
        if let Some(parent) = full_path.parent() {
            if !parent.exists() {
                bail!(
                    "Parent directory does not exist: {}. Create it first with: mkdir -p {}",
                    parent.display(),
                    parent.display()
                );
            }
        }
        // Return the full path without canonicalizing (since file doesn't exist yet)
        Ok(full_path.to_string_lossy().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
