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
use openrouter_rs::types::{Effort, Role, ServerTool, ToolCall};
use serde_json::Value;
use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::io::{self, IsTerminal};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::{InlineColors, ReasoningEffort};
use crate::file_ops::FileWriter;
use crate::input::{FileReference, OutputFileSpec};
use crate::models::ModelEntry;
use crate::render::StreamRenderer;
use crate::shell::{RunProgramParams, RunShellCommandParams, ShellRuntime};
use crate::system_prompt;
use crate::tools::{
    EditFileParams, FileToolMode, FindParams, GrepExactParams, GrepRegexParams, ListFilesParams,
    ReadFileParams, ReplaceLinesParams, ToolAccess, WriteFileParams, clamp_tool_output, run_find,
    run_grep_exact, run_grep_regex, run_list_files, run_read_file,
};

/// A unified session for interacting with language models
pub struct Session {
    /// Conversation history (system, user, assistant, tool messages)
    messages: Vec<Message>,
    /// Stable session identifier reused across all requests in this session
    session_id: String,
    /// The model being used
    model_entry: ModelEntry,
    /// API client
    client: OpenRouterClient,
    /// Files allowed for writing (accumulates in chat mode)
    output_files: Vec<OutputFileSpec>,
    /// Whether to auto-approve file overwrites and edits
    accept_writes: bool,
    /// Theme for markdown rendering
    theme_name: String,
    /// Colors for inline markdown
    inline_colors: InlineColors,
    /// Tool access capabilities
    tool_access: ToolAccess,
    /// Whether to enable OpenRouter server-side web search
    web_search: bool,
    /// Effective reasoning effort for all requests in this session
    reasoning_effort: ReasoningEffort,
    /// Optional shell runtime when shell tools are enabled
    shell_runtime: Option<ShellRuntime>,
    /// Whether confirmation prompts should be suppressed
    non_interactive: bool,
    /// Whether hidden files/directories are accessible to tools
    allow_hidden: bool,
    tool_log_mode: ToolLogMode,
    max_session_bytes: usize,
    pending_turn: bool,
    retry_kind: Option<RetryKind>,
    /// Commands denied or rejected earlier in the session
    failed_shell_calls: HashSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolLogMode {
    Off,
    Compact,
    Full,
}

#[derive(Debug, Clone)]
pub struct SessionOptions {
    pub output_files: Vec<OutputFileSpec>,
    pub accept_writes: bool,
    pub theme_name: String,
    pub inline_colors: InlineColors,
    pub tool_access: ToolAccess,
    pub web_search: bool,
    pub reasoning_effort: ReasoningEffort,
    pub shell_runtime: Option<ShellRuntime>,
    pub non_interactive: bool,
    pub allow_hidden: bool,
    pub tool_log_mode: ToolLogMode,
    pub max_session_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryKind {
    Stream,
    PartialStream,
    ToolContinuation,
}

#[derive(Debug)]
struct StreamFailure {
    message: String,
    partial_output: bool,
}

impl fmt::Display for StreamFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for StreamFailure {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ToolAvailability {
    read_tools: bool,
    write_file: bool,
    edit_tools: bool,
    shell_tools: bool,
}

fn openrouter_reasoning_effort(effort: ReasoningEffort) -> Option<Effort> {
    match effort {
        ReasoningEffort::Auto => None,
        ReasoningEffort::Max => Some(Effort::Max),
        ReasoningEffort::Xhigh => Some(Effort::Xhigh),
        ReasoningEffort::High => Some(Effort::High),
        ReasoningEffort::Medium => Some(Effort::Medium),
        ReasoningEffort::Low => Some(Effort::Low),
        ReasoningEffort::Minimal => Some(Effort::Minimal),
        ReasoningEffort::None => Some(Effort::None),
    }
}

fn determine_tool_availability(
    tool_access: ToolAccess,
    output_files: &[OutputFileSpec],
) -> ToolAvailability {
    match tool_access.file_mode {
        FileToolMode::Disabled => ToolAvailability {
            read_tools: false,
            write_file: !output_files.is_empty(),
            edit_tools: false,
            shell_tools: tool_access.shell_enabled,
        },
        FileToolMode::ReadOnly => ToolAvailability {
            read_tools: true,
            write_file: !output_files.is_empty(),
            edit_tools: output_files.iter().any(|f| f.include_as_input),
            shell_tools: tool_access.shell_enabled,
        },
        FileToolMode::ReadWrite => ToolAvailability {
            read_tools: true,
            write_file: true,
            edit_tools: true,
            shell_tools: tool_access.shell_enabled,
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
    pub fn new(client: OpenRouterClient, model_entry: ModelEntry, options: SessionOptions) -> Self {
        Session {
            messages: Vec::new(),
            session_id: generate_session_id(),
            model_entry,
            client,
            output_files: options.output_files,
            accept_writes: options.accept_writes,
            theme_name: options.theme_name,
            inline_colors: options.inline_colors,
            tool_access: options.tool_access,
            web_search: options.web_search,
            reasoning_effort: options.reasoning_effort,
            shell_runtime: options.shell_runtime,
            non_interactive: options.non_interactive,
            allow_hidden: options.allow_hidden,
            tool_log_mode: options.tool_log_mode,
            max_session_bytes: options.max_session_bytes,
            pending_turn: false,
            retry_kind: None,
            failed_shell_calls: HashSet::new(),
        }
    }

    /// Send a user message and get the assistant's response.
    pub async fn send_message(&mut self, user_content: String) -> Result<String> {
        self.begin_turn(user_content)?;
        self.continue_pending_turn().await
    }

    fn begin_turn(&mut self, user_content: String) -> Result<()> {
        if self.pending_turn {
            anyhow::bail!("Cannot add a new message while the previous turn is pending");
        }

        if self.messages.is_empty() {
            let system_prompt = self.build_system_prompt();
            if !system_prompt.is_empty() {
                self.messages
                    .push(Message::new(Role::System, system_prompt.as_str()));
            }
        }

        self.messages
            .push(Message::new(Role::User, user_content.as_str()));
        self.pending_turn = true;
        Ok(())
    }

    pub async fn retry_pending(&mut self) -> Result<String> {
        if !self.pending_turn {
            anyhow::bail!("There is no pending turn to retry");
        }
        self.continue_pending_turn().await
    }

    pub fn retry_kind(&self) -> Option<RetryKind> {
        self.retry_kind
    }

    async fn continue_pending_turn(&mut self) -> Result<String> {
        self.retry_kind = None;

        let mut all_response_text = String::new();

        for _round in 0..Self::MAX_TOOL_ROUNDS {
            self.trim_context_to_limit()?;
            let request = self.build_request()?;
            let (response_text, tool_calls) = match self.stream_response(&request).await {
                Ok(response) => response,
                Err(error) => {
                    if let Some(stream_failure) = error.downcast_ref::<StreamFailure>() {
                        self.retry_kind = Some(if stream_failure.partial_output {
                            RetryKind::PartialStream
                        } else {
                            RetryKind::Stream
                        });
                    }
                    return Err(error);
                }
            };

            if !response_text.is_empty() {
                if !all_response_text.is_empty() {
                    all_response_text.push('\n');
                }
                all_response_text.push_str(&response_text);
            }

            if tool_calls.is_empty() {
                self.messages
                    .push(Message::new(Role::Assistant, response_text.as_str()));
                self.pending_turn = false;
                self.retry_kind = None;
                return Ok(all_response_text);
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
        }

        self.retry_kind = Some(RetryKind::ToolContinuation);
        anyhow::bail!(
            "Reached the maximum of {} tool-call rounds; the turn can be continued",
            Self::MAX_TOOL_ROUNDS
        )
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

        if let Some(first_msg) = self.messages.first_mut()
            && matches!(first_msg.role, Role::System)
        {
            *first_msg = Message::new(Role::System, new_system.as_str());
            return;
        }

        self.messages
            .insert(0, Message::new(Role::System, new_system));
    }

    /// Build the system prompt including tool and file access instructions.
    fn build_system_prompt(&self) -> String {
        system_prompt::build_system_prompt(
            &self.model_entry,
            &self.output_files,
            self.tool_access,
            self.allow_hidden,
        )
    }

    fn tool_availability(&self) -> ToolAvailability {
        determine_tool_availability(self.tool_access, &self.output_files)
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

    fn trim_context_to_limit(&mut self) -> Result<()> {
        let mut serialized_bytes = serde_json::to_vec(&self.messages)
            .context("Failed to measure conversation context")?
            .len();
        if serialized_bytes <= self.max_session_bytes {
            return Ok(());
        }

        let original_bytes = serialized_bytes;
        let mut removed_turns = 0;

        while serialized_bytes > self.max_session_bytes {
            let Some(first_user) = self
                .messages
                .iter()
                .position(|message| matches!(message.role, Role::User))
            else {
                break;
            };
            let Some(next_user_offset) = self.messages[first_user + 1..]
                .iter()
                .position(|message| matches!(message.role, Role::User))
            else {
                break;
            };
            let next_user = first_user + 1 + next_user_offset;
            self.messages.drain(first_user..next_user);
            removed_turns += 1;
            serialized_bytes = serde_json::to_vec(&self.messages)
                .context("Failed to measure conversation context")?
                .len();
        }

        if removed_turns > 0 {
            eprintln!(
                "Warning: removed {} old conversation turn(s) ({} bytes) to fit the session limit",
                removed_turns,
                original_bytes.saturating_sub(serialized_bytes)
            );
        }

        if serialized_bytes > self.max_session_bytes {
            anyhow::bail!(
                "The current turn requires {} serialized bytes, exceeding the {} byte session limit",
                serialized_bytes,
                self.max_session_bytes
            );
        }

        Ok(())
    }

    /// Build chat request from current message history.
    fn build_request(&self) -> Result<ChatCompletionRequest> {
        let availability = self.tool_availability();

        let mut builder = ChatCompletionRequest::builder();
        builder
            .model(&self.model_entry.model_id)
            .messages(self.messages.clone())
            .session_id(&self.session_id);

        if let Some(effort) = openrouter_reasoning_effort(self.reasoning_effort) {
            builder.reasoning_effort(effort);
        }

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

        if availability.shell_tools {
            builder.tool(RunProgramParams::create_tool());
            builder.tool(RunShellCommandParams::create_tool());
        }

        if self.web_search {
            builder.server_tool(ServerTool::web_search());
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
            .chat()
            .stream_tool_aware(request)
            .await
            .map_err(|error| {
                anyhow::Error::new(StreamFailure {
                    message: format!("Failed to start streaming chat completion: {}", error),
                    partial_output: false,
                })
            })?;
        let mut saw_done = false;
        let mut stream_error = None;

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
                    saw_done = true;
                    if matches!(finish_reason, Some(FinishReason::ToolCalls))
                        || !tool_calls.is_empty()
                    {
                        result_tool_calls = tool_calls;
                    }
                }
                StreamEvent::Error(e) => {
                    stream_error = Some(e.to_string());
                    break;
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

        if let Some(error) = stream_error {
            return Err(anyhow::Error::new(StreamFailure {
                message: format!("Stream error: {}", error),
                partial_output: !accumulated_text.is_empty(),
            }));
        }
        if !saw_done {
            return Err(anyhow::Error::new(StreamFailure {
                message: "Stream ended before the completion marker".to_string(),
                partial_output: !accumulated_text.is_empty(),
            }));
        }

        Ok((accumulated_text, result_tool_calls))
    }

    /// Execute tool calls and append tool responses to history.
    async fn execute_tool_calls(&mut self, tool_calls: &[ToolCall]) -> Result<()> {
        let availability = self.tool_availability();

        let write_writer = FileWriter::new(
            self.output_write_paths(),
            self.tool_access.file_mode == FileToolMode::ReadWrite,
            self.allow_hidden,
            self.accept_writes,
            self.non_interactive,
            self.inline_colors.clone(),
        );

        let edit_writer = FileWriter::new(
            self.output_edit_paths(),
            self.tool_access.file_mode == FileToolMode::ReadWrite,
            self.allow_hidden,
            self.accept_writes,
            self.non_interactive,
            self.inline_colors.clone(),
        );

        for tool_call in tool_calls {
            self.log_tool_call(tool_call);

            let tool_result = self
                .execute_single_tool_call(tool_call, availability, &write_writer, &edit_writer)
                .await;

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
        if self.tool_log_mode == ToolLogMode::Off {
            return;
        }

        let raw_args = tool_call.arguments_json();
        let parsed_args = serde_json::from_str::<Value>(raw_args).ok();

        let compact_value = match parsed_args.as_ref() {
            Some(value) if self.tool_log_mode != ToolLogMode::Full => {
                truncate_json_strings(value, Self::TOOL_ARG_STRING_MAX_CHARS)
            }
            Some(value) => value.clone(),
            None => Value::String(raw_args.to_string()),
        };

        let compact_args = serde_json::to_string(&compact_value)
            .unwrap_or_else(|_| truncate_with_suffix(raw_args, Self::TOOL_LOG_ONE_LINE_MAX_CHARS));

        if compact_args.chars().count() <= Self::TOOL_LOG_ONE_LINE_MAX_CHARS {
            eprintln!("[tool] {} args={}", tool_call.name(), compact_args);
            return;
        }

        if self.tool_log_mode == ToolLogMode::Full
            && let Some(value) = parsed_args
            && let Ok(pretty) = serde_json::to_string_pretty(&value)
        {
            eprintln!("[tool] {} id={}", tool_call.name(), tool_call.id());
            eprintln!("[tool] args:\n{}", pretty);
            return;
        }

        let one_line_args = truncate_with_suffix(&compact_args, Self::TOOL_LOG_ONE_LINE_MAX_CHARS);
        eprintln!("[tool] {} args={}", tool_call.name(), one_line_args);
    }

    async fn execute_single_tool_call(
        &mut self,
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
                run_list_files(&params.path, self.allow_hidden)
            }
            "find" => {
                if !availability.read_tools {
                    return Ok("Tool 'find' is not available in current mode.".to_string());
                }
                let params = tool_call
                    .parse_params::<FindParams>()
                    .context("Invalid find parameters")?;
                run_find(&params.glob, self.allow_hidden)
            }
            "grep_regex" => {
                if !availability.read_tools {
                    return Ok("Tool 'grep_regex' is not available in current mode.".to_string());
                }
                let params = tool_call
                    .parse_params::<GrepRegexParams>()
                    .context("Invalid grep_regex parameters")?;
                run_grep_regex(&params.pattern, &params.path_glob, self.allow_hidden)
            }
            "grep_exact" => {
                if !availability.read_tools {
                    return Ok("Tool 'grep_exact' is not available in current mode.".to_string());
                }
                let params = tool_call
                    .parse_params::<GrepExactParams>()
                    .context("Invalid grep_exact parameters")?;
                run_grep_exact(&params.text, &params.path_glob, self.allow_hidden)
            }
            "read_file" => {
                if !availability.read_tools {
                    return Ok("Tool 'read_file' is not available in current mode.".to_string());
                }
                let params = tool_call
                    .parse_params::<ReadFileParams>()
                    .context("Invalid read_file parameters")?;
                run_read_file(
                    &params.path,
                    params.start_line,
                    params.end_line,
                    self.allow_hidden,
                )
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
                    false => Ok(format!("Skipped changes to: {}", params.path)),
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
                    false => Ok(format!("Skipped changes to: {}", params.path)),
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
                    false => Ok(format!("Skipped changes to: {}", params.path)),
                }
            }
            "run_program" => {
                if !availability.shell_tools {
                    return Ok("Tool 'run_program' is not available in current mode.".to_string());
                }
                let params = tool_call
                    .parse_params::<RunProgramParams>()
                    .context("Invalid run_program parameters")?;
                let runtime = self
                    .shell_runtime
                    .clone()
                    .context("Shell runtime is unavailable")?;
                runtime
                    .execute_program(
                        params,
                        self.allow_hidden,
                        &self.inline_colors,
                        &mut self.failed_shell_calls,
                    )
                    .await
            }
            "run_shell_command" => {
                if !availability.shell_tools {
                    return Ok(
                        "Tool 'run_shell_command' is not available in current mode.".to_string()
                    );
                }
                let params = tool_call
                    .parse_params::<RunShellCommandParams>()
                    .context("Invalid run_shell_command parameters")?;
                let runtime = self
                    .shell_runtime
                    .clone()
                    .context("Shell runtime is unavailable")?;
                runtime
                    .execute_shell_command(
                        params,
                        self.allow_hidden,
                        &self.inline_colors,
                        &mut self.failed_shell_calls,
                    )
                    .await
            }
            unknown => Ok(format!("Unknown tool: {}", unknown)),
        }
    }
}

fn generate_session_id() -> String {
    static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

    let counter = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
    let timestamp_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();

    format!(
        "zo-{:x}-{:x}-{:x}",
        std::process::id(),
        timestamp_nanos,
        counter
    )
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
    use openrouter_rs::OpenRouterClient;

    fn test_session() -> Session {
        test_session_with_client(OpenRouterClient::builder().build().unwrap())
    }

    fn test_session_with_client(client: OpenRouterClient) -> Session {
        Session::new(
            client,
            ModelEntry {
                model_id: "openai/gpt-5.6-sol".to_string(),
                system_prompt: None,
                reasoning_effort: None,
            },
            SessionOptions {
                output_files: vec![],
                accept_writes: false,
                theme_name: "base16-ocean.dark".to_string(),
                inline_colors: InlineColors::default(),
                tool_access: ToolAccess {
                    file_mode: FileToolMode::Disabled,
                    shell_enabled: false,
                },
                web_search: false,
                reasoning_effort: ReasoningEffort::Auto,
                shell_runtime: None,
                non_interactive: false,
                allow_hidden: false,
                tool_log_mode: ToolLogMode::Off,
                max_session_bytes: crate::config::DEFAULT_MAX_SESSION_BYTES,
            },
        )
    }

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
        let availability = determine_tool_availability(
            ToolAccess {
                file_mode: FileToolMode::Disabled,
                shell_enabled: false,
            },
            &[],
        );
        assert!(!availability.read_tools);
        assert!(!availability.write_file);
        assert!(!availability.edit_tools);
        assert!(!availability.shell_tools);
    }

    #[test]
    fn test_tool_availability_disabled_with_outputs() {
        let output = OutputFileSpec {
            filename: "out.txt".to_string(),
            normalized_path: "/tmp/out.txt".to_string(),
            include_as_input: false,
        };
        let availability = determine_tool_availability(
            ToolAccess {
                file_mode: FileToolMode::Disabled,
                shell_enabled: false,
            },
            &[output],
        );
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
        let availability = determine_tool_availability(
            ToolAccess {
                file_mode: FileToolMode::ReadOnly,
                shell_enabled: false,
            },
            &[output],
        );
        assert!(availability.read_tools);
        assert!(availability.write_file);
        assert!(availability.edit_tools);
    }

    #[test]
    fn test_tool_availability_read_write() {
        let availability = determine_tool_availability(
            ToolAccess {
                file_mode: FileToolMode::ReadWrite,
                shell_enabled: true,
            },
            &[],
        );
        assert!(availability.read_tools);
        assert!(availability.write_file);
        assert!(availability.edit_tools);
        assert!(availability.shell_tools);
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

    #[test]
    fn test_build_request_includes_session_id() {
        let session = test_session();

        let request = session.build_request().unwrap();
        let json = serde_json::to_value(&request).unwrap();

        assert_eq!(json["session_id"], session.session_id);
        assert!(session.session_id.starts_with("zo-"));
        assert!(json.get("temperature").is_none());
        assert!(json.get("reasoning").is_none());
    }

    #[test]
    fn test_build_request_serializes_explicit_reasoning_efforts() {
        let cases = [
            (ReasoningEffort::Max, "max"),
            (ReasoningEffort::Xhigh, "xhigh"),
            (ReasoningEffort::High, "high"),
            (ReasoningEffort::Medium, "medium"),
            (ReasoningEffort::Low, "low"),
            (ReasoningEffort::Minimal, "minimal"),
            (ReasoningEffort::None, "none"),
        ];

        for (effort, expected) in cases {
            let mut session = test_session();
            session.reasoning_effort = effort;

            let request = session.build_request().unwrap();
            let json = serde_json::to_value(&request).unwrap();

            assert_eq!(json["reasoning"]["effort"], expected);
        }
    }

    #[test]
    fn test_build_request_includes_web_search_tool_when_enabled() {
        let mut session = test_session();
        session.web_search = true;

        let request = session.build_request().unwrap();
        let json = serde_json::to_value(&request).unwrap();

        assert_eq!(json["tools"][0]["type"], "openrouter:web_search");
    }

    #[test]
    fn test_build_request_reuses_same_session_id() {
        let mut session = test_session();
        session.messages.push(Message::new(Role::User, "first"));

        let first_request = session.build_request().unwrap();
        let second_request = session.build_request().unwrap();

        let first_json = serde_json::to_value(&first_request).unwrap();
        let second_json = serde_json::to_value(&second_request).unwrap();

        assert_eq!(first_json["session_id"], second_json["session_id"]);
        assert_eq!(first_json["session_id"], session.session_id);
    }

    #[test]
    fn test_pending_turn_is_added_once() {
        let mut session = test_session();
        session.begin_turn("hello".to_string()).unwrap();
        assert!(session.pending_turn);
        assert_eq!(
            session
                .messages
                .iter()
                .filter(|message| matches!(message.role, Role::User))
                .count(),
            1
        );
        assert!(session.begin_turn("duplicate".to_string()).is_err());
        assert_eq!(
            session
                .messages
                .iter()
                .filter(|message| matches!(message.role, Role::User))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn test_failed_stream_retry_does_not_duplicate_user_message() {
        let client = OpenRouterClient::builder()
            .base_url("not a valid URL")
            .api_key("test")
            .build()
            .unwrap();
        let mut session = test_session_with_client(client);

        assert!(session.send_message("hello".to_string()).await.is_err());
        assert_eq!(session.retry_kind(), Some(RetryKind::Stream));
        assert!(session.retry_pending().await.is_err());
        assert_eq!(
            session
                .messages
                .iter()
                .filter(|message| matches!(message.role, Role::User))
                .count(),
            1
        );
    }

    #[test]
    fn test_context_trimming_removes_complete_oldest_turn() {
        let mut session = test_session();
        session.messages = vec![
            Message::new(Role::System, "system"),
            Message::new(Role::User, "old user"),
            Message::new(Role::Assistant, "x".repeat(1_000)),
            Message::new(Role::User, "current user"),
        ];
        session.pending_turn = true;
        session.max_session_bytes = 300;

        session.trim_context_to_limit().unwrap();

        assert_eq!(session.messages.len(), 2);
        assert!(matches!(session.messages[0].role, Role::System));
        assert!(matches!(session.messages[1].role, Role::User));
        let json = serde_json::to_string(&session.messages).unwrap();
        assert!(json.contains("current user"));
        assert!(!json.contains("old user"));
    }

    #[test]
    fn test_context_trimming_rejects_oversized_current_turn() {
        let mut session = test_session();
        session.messages = vec![Message::new(Role::User, "x".repeat(1_000))];
        session.pending_turn = true;
        session.max_session_bytes = 100;
        assert!(session.trim_context_to_limit().is_err());
    }
}
