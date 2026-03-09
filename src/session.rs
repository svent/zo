//! Unified session management for both single-shot and chat modes
//!
//! This module provides the core Session abstraction that handles:
//! - Message history management
//! - API request building and sending
//! - Tool call execution with strict permission checks
//! - Response streaming and rendering
//! - Output file tracking across turns

use anyhow::{Context, Result};
use futures_util::StreamExt;
use openrouter_rs::OpenRouterClient;
use openrouter_rs::api::chat::{ChatCompletionRequest, Message};
use openrouter_rs::types::completion::FinishReason;
use openrouter_rs::types::stream::StreamEvent;
use openrouter_rs::types::typed_tool::TypedTool;
use openrouter_rs::types::{Role, ToolCall};
use serde_json::Value;
use std::io::{self, IsTerminal};

use crate::config::InlineColors;
use crate::file_ops::FileWriter;
use crate::input::{FileReference, OutputFileSpec};
use crate::models::ModelEntry;
use crate::render::StreamRenderer;
use crate::system_prompt;
use crate::tools::{
    EditFileParams, FindParams, GrepExactParams, GrepRegexParams, ListFilesParams, ReadFileParams,
    ReplaceLinesParams, ToolMode, WriteFileParams, clamp_tool_output, run_find, run_grep_exact,
    run_grep_regex, run_list_files, run_read_file,
};

/// A unified session for interacting with language models
pub struct Session {
    /// Conversation history (system, user, assistant, tool messages)
    messages: Vec<Message>,
    /// The model being used
    model_entry: ModelEntry,
    /// API client
    client: OpenRouterClient,
    /// Files allowed for writing (accumulates in chat mode)
    output_files: Vec<OutputFileSpec>,
    /// Whether to auto-approve file changes
    auto_approve: bool,
    /// Theme for markdown rendering
    theme_name: String,
    /// Colors for inline markdown
    inline_colors: InlineColors,
    /// Tool access mode
    tool_mode: ToolMode,
    /// Whether to print tool calls requested by the model before executing them
    show_tool_calls: bool,
    /// Whether to show full tool arguments in logs (for debug mode)
    show_full_tool_args: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ToolAvailability {
    read_tools: bool,
    write_file: bool,
    edit_tools: bool,
}

fn determine_tool_availability(
    tool_mode: ToolMode,
    output_files: &[OutputFileSpec],
) -> ToolAvailability {
    match tool_mode {
        ToolMode::Disabled => ToolAvailability {
            read_tools: false,
            write_file: !output_files.is_empty(),
            edit_tools: false,
        },
        ToolMode::ReadOnly => ToolAvailability {
            read_tools: true,
            write_file: !output_files.is_empty(),
            edit_tools: output_files.iter().any(|f| f.include_as_input),
        },
        ToolMode::ReadWrite => ToolAvailability {
            read_tools: true,
            write_file: true,
            edit_tools: true,
        },
    }
}

impl Session {
    /// Maximum number of tool call round-trips before stopping.
    const MAX_TOOL_ROUNDS: usize = 32;
    /// Some models reject tool-call assistant messages with empty content.
    const EMPTY_TOOL_CALL_PLACEHOLDER: &'static str = "[tool call]";
    /// Prefer one-line tool logs when serialized arguments fit this size.
    const TOOL_LOG_ONE_LINE_MAX_CHARS: usize = 220;
    /// In verbose mode (without debug), truncate long string values in JSON args.
    const TOOL_ARG_STRING_MAX_CHARS: usize = 180;

    /// Create a new session.
    pub fn new(
        client: OpenRouterClient,
        model_entry: ModelEntry,
        output_files: Vec<OutputFileSpec>,
        auto_approve: bool,
        theme_name: String,
        inline_colors: InlineColors,
        tool_mode: ToolMode,
        show_tool_calls: bool,
        show_full_tool_args: bool,
    ) -> Self {
        Session {
            messages: Vec::new(),
            model_entry,
            client,
            output_files,
            auto_approve,
            theme_name,
            inline_colors,
            tool_mode,
            show_tool_calls,
            show_full_tool_args,
        }
    }

    /// Send a user message and get the assistant's response.
    pub async fn send_message(&mut self, user_content: String) -> Result<String> {
        if self.messages.is_empty() {
            let system_prompt = self.build_system_prompt();
            if !system_prompt.is_empty() {
                self.messages
                    .push(Message::new(Role::System, system_prompt.as_str()));
            }
        }

        self.messages
            .push(Message::new(Role::User, user_content.as_str()));

        let mut all_response_text = String::new();

        for round in 0..Self::MAX_TOOL_ROUNDS {
            let request = self.build_request()?;
            let (response_text, tool_calls) = self.stream_response(&request).await?;

            if !response_text.is_empty() {
                if !all_response_text.is_empty() {
                    all_response_text.push('\n');
                }
                all_response_text.push_str(&response_text);
            }

            if tool_calls.is_empty() {
                self.messages
                    .push(Message::new(Role::Assistant, response_text.as_str()));
                break;
            }

            let assistant_content = if response_text.trim().is_empty() {
                Self::EMPTY_TOOL_CALL_PLACEHOLDER
            } else {
                response_text.as_str()
            };
            self.messages.push(Message::assistant_with_tool_calls(
                assistant_content,
                tool_calls.clone(),
            ));

            self.execute_tool_calls(&tool_calls).await?;

            if round + 1 >= Self::MAX_TOOL_ROUNDS {
                eprintln!(
                    "Warning: reached maximum tool call rounds ({}), stopping",
                    Self::MAX_TOOL_ROUNDS
                );
            }
        }

        Ok(all_response_text)
    }

    /// Add new output files to the allowed list.
    pub fn add_output_files(&mut self, new_files: Vec<OutputFileSpec>) {
        for new_file in new_files {
            if !self
                .output_files
                .iter()
                .any(|f| f.normalized_path == new_file.normalized_path)
            {
                self.output_files.push(new_file);
            }
        }

        if self.messages.is_empty() {
            return;
        }

        let new_system = self.build_system_prompt();
        if new_system.is_empty() {
            return;
        }

        if let Some(first_msg) = self.messages.first_mut() {
            if matches!(first_msg.role, Role::System) {
                *first_msg = Message::new(Role::System, new_system.as_str());
                return;
            }
        }

        self.messages
            .insert(0, Message::new(Role::System, new_system));
    }

    /// Build the system prompt including tool and file access instructions.
    fn build_system_prompt(&self) -> String {
        system_prompt::build_system_prompt(&self.model_entry, &self.output_files, self.tool_mode)
    }

    fn tool_availability(&self) -> ToolAvailability {
        determine_tool_availability(self.tool_mode, &self.output_files)
    }

    fn output_write_paths(&self) -> Vec<String> {
        self.output_files
            .iter()
            .map(|f| f.normalized_path.clone())
            .collect()
    }

    fn output_edit_paths(&self) -> Vec<String> {
        self.output_files
            .iter()
            .filter(|f| f.include_as_input)
            .map(|f| f.normalized_path.clone())
            .collect()
    }

    /// Build chat request from current message history.
    fn build_request(&self) -> Result<ChatCompletionRequest> {
        let availability = self.tool_availability();

        let mut builder = ChatCompletionRequest::builder();
        builder
            .model(&self.model_entry.model_id)
            .messages(self.messages.clone())
            .temperature(0.5);

        if availability.read_tools {
            builder.tool(ListFilesParams::create_tool());
            builder.tool(FindParams::create_tool());
            builder.tool(GrepRegexParams::create_tool());
            builder.tool(GrepExactParams::create_tool());
            builder.tool(ReadFileParams::create_tool());
        }

        if availability.write_file {
            builder.tool(WriteFileParams::create_tool());
        }

        if availability.edit_tools {
            builder.tool(EditFileParams::create_tool());
            builder.tool(ReplaceLinesParams::create_tool());
        }

        builder
            .build()
            .context("Failed to build chat completion request")
    }

    /// Stream response, render progressively, and return text + any tool calls.
    async fn stream_response(
        &self,
        request: &ChatCompletionRequest,
    ) -> Result<(String, Vec<ToolCall>)> {
        let is_terminal = io::stdout().is_terminal();

        let mut renderer = if is_terminal {
            StreamRenderer::with_theme(&self.theme_name, self.inline_colors.clone())
        } else {
            StreamRenderer::with_plain_text()
        };

        let mut accumulated_text = String::new();
        let mut result_tool_calls: Vec<ToolCall> = Vec::new();
        let mut leading_probe = String::new();
        let mut probing_placeholder = true;

        let mut stream = self
            .client
            .stream_chat_completion_tool_aware(request)
            .await
            .context("Failed to start streaming chat completion")?;

        while let Some(event) = stream.next().await {
            match event {
                StreamEvent::ContentDelta(text) => {
                    if probing_placeholder {
                        leading_probe.push_str(&text);

                        if Self::EMPTY_TOOL_CALL_PLACEHOLDER.starts_with(leading_probe.as_str())
                            && leading_probe.len() <= Self::EMPTY_TOOL_CALL_PLACEHOLDER.len()
                        {
                            continue;
                        }

                        accumulated_text.push_str(&leading_probe);
                        renderer
                            .add_chunk(&leading_probe)
                            .context("Failed to render chunk")?;
                        leading_probe.clear();
                        probing_placeholder = false;
                        continue;
                    }

                    accumulated_text.push_str(&text);
                    renderer
                        .add_chunk(&text)
                        .context("Failed to render chunk")?;
                }
                StreamEvent::Done {
                    tool_calls,
                    finish_reason,
                    ..
                } => {
                    if matches!(finish_reason, Some(FinishReason::ToolCalls))
                        || !tool_calls.is_empty()
                    {
                        result_tool_calls = tool_calls;
                    }
                }
                StreamEvent::Error(e) => {
                    eprintln!("Stream error: {}", e);
                }
                _ => {
                    // ReasoningDelta, ReasoningDetailsDelta -- ignore for now
                }
            }
        }

        if probing_placeholder && leading_probe != Self::EMPTY_TOOL_CALL_PLACEHOLDER {
            accumulated_text.push_str(&leading_probe);
            renderer
                .add_chunk(&leading_probe)
                .context("Failed to render chunk")?;
        }

        renderer
            .flush()
            .context("Failed to flush remaining content")?;
        if !accumulated_text.is_empty() {
            println!();
        }

        Ok((accumulated_text, result_tool_calls))
    }

    /// Execute tool calls and append tool responses to history.
    async fn execute_tool_calls(&mut self, tool_calls: &[ToolCall]) -> Result<()> {
        let availability = self.tool_availability();

        let write_writer = FileWriter::new(
            self.output_write_paths(),
            matches!(self.tool_mode, ToolMode::ReadWrite),
            self.auto_approve,
            self.inline_colors.clone(),
        );

        let edit_writer = FileWriter::new(
            self.output_edit_paths(),
            matches!(self.tool_mode, ToolMode::ReadWrite),
            self.auto_approve,
            self.inline_colors.clone(),
        );

        for tool_call in tool_calls {
            self.log_tool_call(tool_call);

            let tool_result =
                self.execute_single_tool_call(tool_call, availability, &write_writer, &edit_writer);

            let result_text = match tool_result {
                Ok(text) => text,
                Err(err) => format!("Tool error: {}", err),
            };

            self.messages.push(Message::tool_response(
                &tool_call.id,
                clamp_tool_output(result_text),
            ));
        }

        Ok(())
    }

    fn log_tool_call(&self, tool_call: &ToolCall) {
        if !self.show_tool_calls {
            return;
        }

        let raw_args = tool_call.arguments_json();
        let parsed_args = serde_json::from_str::<Value>(raw_args).ok();

        let compact_value = match parsed_args.as_ref() {
            Some(value) if !self.show_full_tool_args => {
                truncate_json_strings(value, Self::TOOL_ARG_STRING_MAX_CHARS)
            }
            Some(value) => value.clone(),
            None => Value::String(raw_args.to_string()),
        };

        let compact_args = serde_json::to_string(&compact_value)
            .unwrap_or_else(|_| truncate_with_suffix(raw_args, Self::TOOL_LOG_ONE_LINE_MAX_CHARS));

        if compact_args.chars().count() <= Self::TOOL_LOG_ONE_LINE_MAX_CHARS {
            eprintln!(
                "[tool] {} id={} args={}",
                tool_call.name(),
                tool_call.id(),
                compact_args
            );
            return;
        }

        if self.show_full_tool_args {
            if let Some(value) = parsed_args {
                match serde_json::to_string_pretty(&value) {
                    Ok(pretty) => {
                        eprintln!("[tool] {} id={}", tool_call.name(), tool_call.id());
                        eprintln!("[tool] args:\n{}", pretty);
                        return;
                    }
                    Err(_) => {}
                }
            }
        }

        let one_line_args = truncate_with_suffix(&compact_args, Self::TOOL_LOG_ONE_LINE_MAX_CHARS);
        eprintln!(
            "[tool] {} id={} args={}",
            tool_call.name(),
            tool_call.id(),
            one_line_args
        );
    }

    fn execute_single_tool_call(
        &self,
        tool_call: &ToolCall,
        availability: ToolAvailability,
        write_writer: &FileWriter,
        edit_writer: &FileWriter,
    ) -> Result<String> {
        match tool_call.name() {
            "list_files" => {
                if !availability.read_tools {
                    return Ok("Tool 'list_files' is not available in current mode.".to_string());
                }
                let params = tool_call
                    .parse_params::<ListFilesParams>()
                    .context("Invalid list_files parameters")?;
                run_list_files(&params.path)
            }
            "find" => {
                if !availability.read_tools {
                    return Ok("Tool 'find' is not available in current mode.".to_string());
                }
                let params = tool_call
                    .parse_params::<FindParams>()
                    .context("Invalid find parameters")?;
                run_find(&params.glob)
            }
            "grep_regex" => {
                if !availability.read_tools {
                    return Ok("Tool 'grep_regex' is not available in current mode.".to_string());
                }
                let params = tool_call
                    .parse_params::<GrepRegexParams>()
                    .context("Invalid grep_regex parameters")?;
                run_grep_regex(&params.pattern, &params.path_glob)
            }
            "grep_exact" => {
                if !availability.read_tools {
                    return Ok("Tool 'grep_exact' is not available in current mode.".to_string());
                }
                let params = tool_call
                    .parse_params::<GrepExactParams>()
                    .context("Invalid grep_exact parameters")?;
                run_grep_exact(&params.text, &params.path_glob)
            }
            "read_file" => {
                if !availability.read_tools {
                    return Ok("Tool 'read_file' is not available in current mode.".to_string());
                }
                let params = tool_call
                    .parse_params::<ReadFileParams>()
                    .context("Invalid read_file parameters")?;
                run_read_file(&params.path, params.start_line, params.end_line)
            }
            "write_file" => {
                if !availability.write_file {
                    return Ok("Tool 'write_file' is not available in current mode.".to_string());
                }
                let params = tool_call
                    .parse_params::<WriteFileParams>()
                    .context("Invalid write_file parameters")?;
                match write_writer.write_file(&params.path, &params.content)? {
                    true => Ok(format!("Successfully wrote file: {}", params.path)),
                    false => Ok(format!("User declined changes to: {}", params.path)),
                }
            }
            "edit_file" => {
                if !availability.edit_tools {
                    return Ok("Tool 'edit_file' is not available in current mode.".to_string());
                }
                let params = tool_call
                    .parse_params::<EditFileParams>()
                    .context("Invalid edit_file parameters")?;
                match edit_writer.edit_file(&params.path, &params.old_string, &params.new_string)? {
                    true => Ok(format!("Successfully edited file: {}", params.path)),
                    false => Ok(format!("User declined changes to: {}", params.path)),
                }
            }
            "replace_lines" => {
                if !availability.edit_tools {
                    return Ok("Tool 'replace_lines' is not available in current mode.".to_string());
                }
                let params = tool_call
                    .parse_params::<ReplaceLinesParams>()
                    .context("Invalid replace_lines parameters")?;
                match edit_writer.replace_lines(
                    &params.path,
                    params.start_line,
                    params.end_line,
                    &params.new_content,
                )? {
                    true => Ok(format!("Successfully replaced lines in: {}", params.path)),
                    false => Ok(format!("User declined changes to: {}", params.path)),
                }
            }
            unknown => Ok(format!("Unknown tool: {}", unknown)),
        }
    }
}

fn truncate_with_suffix(input: &str, max_chars: usize) -> String {
    let total = input.chars().count();
    if total <= max_chars {
        return input.to_string();
    }

    let truncated: String = input.chars().take(max_chars).collect();
    format!("{}...(+{} chars)", truncated, total - max_chars)
}

fn truncate_json_strings(value: &Value, max_chars: usize) -> Value {
    match value {
        Value::String(s) => Value::String(truncate_with_suffix(s, max_chars)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| truncate_json_strings(item, max_chars))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), truncate_json_strings(v, max_chars)))
                .collect(),
        ),
        _ => value.clone(),
    }
}

/// Format file references and prompt into a single user message.
pub fn build_user_message(
    file_references: &[FileReference],
    prompt: &str,
    stdin_content: Option<&str>,
) -> String {
    let mut parts = Vec::new();

    if !file_references.is_empty() {
        let files_formatted = file_references
            .iter()
            .map(|file_ref| {
                format!(
                    "<file name=\"{}\">\n{}\n</file>",
                    file_ref.filename, file_ref.content
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        parts.push(files_formatted);
    }

    if !prompt.is_empty() {
        parts.push(prompt.to_string());
    }

    if let Some(stdin) = stdin_content {
        parts.push(stdin.to_string());
    }

    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_user_message_simple() {
        let msg = build_user_message(&[], "hello world", None);
        assert_eq!(msg, "hello world");
    }

    #[test]
    fn test_build_user_message_with_stdin() {
        let msg = build_user_message(&[], "analyze", Some("data"));
        assert_eq!(msg, "analyze\n\ndata");
    }

    #[test]
    fn test_build_user_message_with_file() {
        let files = vec![FileReference {
            filename: "test.txt".to_string(),
            content: "content".to_string(),
        }];
        let msg = build_user_message(&files, "check this", None);
        assert!(msg.contains("<file name=\"test.txt\">"));
        assert!(msg.contains("content"));
        assert!(msg.contains("check this"));
    }

    #[test]
    fn test_build_user_message_all_parts() {
        let files = vec![FileReference {
            filename: "data.csv".to_string(),
            content: "col1,col2".to_string(),
        }];
        let msg = build_user_message(&files, "analyze", Some("extra data"));
        assert!(msg.contains("<file name=\"data.csv\">"));
        assert!(msg.contains("col1,col2"));
        assert!(msg.contains("analyze"));
        assert!(msg.contains("extra data"));
    }

    #[test]
    fn test_build_user_message_stdin_only() {
        let msg = build_user_message(&[], "", Some("piped input"));
        assert_eq!(msg, "piped input");
    }

    #[test]
    fn test_tool_availability_disabled_no_outputs() {
        let availability = determine_tool_availability(ToolMode::Disabled, &[]);
        assert!(!availability.read_tools);
        assert!(!availability.write_file);
        assert!(!availability.edit_tools);
    }

    #[test]
    fn test_tool_availability_disabled_with_outputs() {
        let output = OutputFileSpec {
            filename: "out.txt".to_string(),
            normalized_path: "/tmp/out.txt".to_string(),
            include_as_input: false,
        };
        let availability = determine_tool_availability(ToolMode::Disabled, &[output]);
        assert!(!availability.read_tools);
        assert!(availability.write_file);
        assert!(!availability.edit_tools);
    }

    #[test]
    fn test_tool_availability_read_only_with_read_write_output() {
        let output = OutputFileSpec {
            filename: "rw.txt".to_string(),
            normalized_path: "/tmp/rw.txt".to_string(),
            include_as_input: true,
        };
        let availability = determine_tool_availability(ToolMode::ReadOnly, &[output]);
        assert!(availability.read_tools);
        assert!(availability.write_file);
        assert!(availability.edit_tools);
    }

    #[test]
    fn test_tool_availability_read_write() {
        let availability = determine_tool_availability(ToolMode::ReadWrite, &[]);
        assert!(availability.read_tools);
        assert!(availability.write_file);
        assert!(availability.edit_tools);
    }

    #[test]
    fn test_truncate_with_suffix_short_string_unchanged() {
        assert_eq!(truncate_with_suffix("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_with_suffix_long_string_truncated() {
        assert_eq!(
            truncate_with_suffix("abcdefghijklmnopqrstuvwxyz", 5),
            "abcde...(+21 chars)"
        );
    }

    #[test]
    fn test_truncate_json_strings_nested_values() {
        let value = serde_json::json!({
            "short": "abc",
            "long": "abcdefghijklmnopqrstuvwxyz",
            "nested": {
                "array": ["1234567890", "ok"]
            }
        });

        let truncated = truncate_json_strings(&value, 4);
        assert_eq!(truncated["short"], "abc");
        assert_eq!(truncated["long"], "abcd...(+22 chars)");
        assert_eq!(truncated["nested"]["array"][0], "1234...(+6 chars)");
        assert_eq!(truncated["nested"]["array"][1], "ok");
    }
}
