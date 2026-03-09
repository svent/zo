use crate::input::OutputFileSpec;
use crate::models::ModelEntry;
use crate::tools::ToolMode;

/// Build the system prompt including tool and file access instructions.
pub fn build_system_prompt(
    model_entry: &ModelEntry,
    output_files: &[OutputFileSpec],
    tool_mode: ToolMode,
) -> String {
    let mut system_prompt = model_entry.system_prompt.clone().unwrap_or_default();

    let write_only_files: Vec<&str> = output_files
        .iter()
        .filter(|f| !f.include_as_input)
        .map(|f| f.filename.as_str())
        .collect();
    let read_write_files: Vec<&str> = output_files
        .iter()
        .filter(|f| f.include_as_input)
        .map(|f| f.filename.as_str())
        .collect();

    let tool_instructions = match tool_mode {
        ToolMode::Disabled => {
            if output_files.is_empty() {
                None
            } else {
                Some(format!(
                    "IMPORTANT: You can write files only via the `write_file` tool. \
                     Allowed output files: {}. \
                     Use `write_file` with the real filename (without !/@! markers). \
                     Never write to any other path.",
                    output_files
                        .iter()
                        .map(|f| f.filename.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            }
        }
        ToolMode::ReadOnly => {
            let mut parts = vec![
                "IMPORTANT: Tool mode is enabled in constrained mode.".to_string(),
                "Read tools available: list_files(path), find(glob), grep_regex(pattern, path_glob), grep_exact(text, path_glob), read_file(path, start_line, end_line).".to_string(),
                "Use tools sparingly and with narrow queries; outputs are truncated for safety.".to_string(),
            ];

            if output_files.is_empty() {
                parts.push(
                    "No write permissions are granted in this request. Do not attempt file modifications."
                        .to_string(),
                );
            } else {
                if !write_only_files.is_empty() {
                    parts.push(format!(
                        "Write-only files (`!file`): {}. Allowed write tool: write_file only.",
                        write_only_files.join(", ")
                    ));
                }

                if !read_write_files.is_empty() {
                    parts.push(format!(
                        "Read-write files (`@!file`): {}. Allowed write tools: write_file, edit_file, replace_lines.",
                        read_write_files.join(", ")
                    ));
                }


                parts.push("Use write tools with the real filename (without !/@! markers).".to_string());
                parts.push(
                    "Never modify files outside those explicit write permissions."
                        .to_string(),
                );
            }

            Some(parts.join("\n"))
        }
        ToolMode::ReadWrite => Some(
            "IMPORTANT: Tool mode is enabled in read-write workspace mode. \
             You may read and modify files within the current working directory using tools. \
             Available tools: list_files, find, grep_regex, grep_exact, read_file, write_file, edit_file, replace_lines. \
             Files referenced via @file/@!file are still provided directly in the user message. \
             Keep tool calls focused and minimal; outputs are truncated for safety."
                .to_string(),
        ),
    };

    if let Some(instructions) = tool_instructions {
        if system_prompt.is_empty() {
            system_prompt = instructions;
        } else {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(&instructions);
        }
    }

    system_prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> ModelEntry {
        ModelEntry {
            model_id: "test/model".to_string(),
            system_prompt: None,
        }
    }

    fn output(filename: &str, include_as_input: bool) -> OutputFileSpec {
        OutputFileSpec {
            filename: filename.to_string(),
            normalized_path: format!("/tmp/{}", filename),
            include_as_input,
        }
    }

    #[test]
    fn test_disabled_mode_with_outputs_mentions_write_file() {
        let prompt = build_system_prompt(&model(), &[output("a.txt", false)], ToolMode::Disabled);
        assert!(prompt.contains("write_file"));
        assert!(prompt.contains("a.txt"));
    }

    #[test]
    fn test_read_only_mode_mentions_read_tools() {
        let prompt = build_system_prompt(&model(), &[], ToolMode::ReadOnly);
        assert!(prompt.contains("list_files"));
        assert!(prompt.contains("No write permissions"));
    }

    #[test]
    fn test_read_only_mode_mentions_split_write_permissions() {
        let prompt = build_system_prompt(
            &model(),
            &[output("wo.txt", false), output("rw.txt", true)],
            ToolMode::ReadOnly,
        );
        assert!(prompt.contains("Write-only files"));
        assert!(prompt.contains("wo.txt"));
        assert!(prompt.contains("rw.txt"));
        assert!(prompt.contains("edit_file"));
    }

    #[test]
    fn test_read_write_mode_mentions_full_tools() {
        let prompt = build_system_prompt(&model(), &[], ToolMode::ReadWrite);
        assert!(prompt.contains("replace_lines"));
    }
}
