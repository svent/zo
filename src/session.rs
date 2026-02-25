//! Unified session management for both single-shot and chat modes
//!
//! This module provides the core Session abstraction that handles:
//! - Message history management
//! - API request building and sending
//! - Tool call execution (file writing)
//! - Response streaming and rendering
//! - Output file tracking across turns
//!
//! Both single-shot and chat modes use the same Session, with chat mode
//! simply calling send_message() in a loop.

use anyhow::{Context, Result};
use futures_util::StreamExt;
use openrouter_rs::OpenRouterClient;
use openrouter_rs::api::chat::{ChatCompletionRequest, Message};
use openrouter_rs::types::completion::FinishReason;
use openrouter_rs::types::stream::StreamEvent;
use openrouter_rs::types::{Role, ToolCall};
use std::io::{self, IsTerminal};

use crate::config::InlineColors;
use crate::file_ops::FileWriter;
use crate::input::{FileReference, OutputFileSpec};
use crate::models::ModelEntry;
use crate::render::StreamRenderer;
use crate::system_prompt;
use crate::tools::SaveFileParams;

/// A unified session for interacting with language models
///
/// Handles both single-shot requests and multi-turn conversations.
/// The difference is purely in how many times send_message() is called:
/// - Single-shot: call once, exit
/// - Chat: call in a loop until user exits
pub struct Session {
    /// Conversation history (system, user, assistant messages)
    /// Empty for single-shot mode before first message
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
}

impl Session {
    /// Create a new session
    ///
    /// # Arguments
    ///
    /// * `client` - OpenRouter API client
    /// * `model_entry` - Model configuration (ID + optional system prompt)
    /// * `output_files` - Initial output files allowed for writing
    /// * `auto_approve` - Whether to auto-approve file changes
    /// * `theme_name` - Theme for syntax highlighting
    /// * `inline_colors` - Colors for inline markdown elements
    ///
    /// The session starts with an empty message history. The system prompt
    /// (if present) is added when the first message is sent.
    pub fn new(
        client: OpenRouterClient,
        model_entry: ModelEntry,
        output_files: Vec<OutputFileSpec>,
        auto_approve: bool,
        theme_name: String,
        inline_colors: InlineColors,
    ) -> Self {
        Session {
            messages: Vec::new(),
            model_entry,
            client,
            output_files,
            auto_approve,
            theme_name,
            inline_colors,
        }
    }

    /// Maximum number of tool call round-trips before stopping.
    /// Prevents infinite loops if the model keeps requesting tools.
    const MAX_TOOL_ROUNDS: usize = 32;
    /// Some models reject tool-call assistant messages with empty content.
    const EMPTY_TOOL_CALL_PLACEHOLDER: &'static str = "[tool call]";

    /// Send a user message and get the assistant's response
    ///
    /// This is the core method that handles:
    /// 1. Adding system prompt (on first message only)
    /// 2. Adding user message to history
    /// 3. Streaming the response (with real-time rendering)
    /// 4. Handling tool calls if present (with multi-round support)
    /// 5. Adding assistant response to history
    /// 6. Returning the response text
    ///
    /// # Arguments
    ///
    /// * `user_content` - The user's message content (can include file references, prompt, STDIN)
    ///
    /// # Returns
    ///
    /// The assistant's response text (after all tool calls are resolved)
    pub async fn send_message(&mut self, user_content: String) -> Result<String> {
        // Add system prompt on first message only
        if self.messages.is_empty() {
            let system_prompt = self.build_system_prompt();
            if !system_prompt.is_empty() {
                self.messages
                    .push(Message::new(Role::System, system_prompt.as_str()));
            }
        }

        // Add user message to history
        self.messages
            .push(Message::new(Role::User, user_content.as_str()));

        let mut all_response_text = String::new();

        // Unified streaming loop: handles both plain responses and tool calls
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
                // No tool calls -- add assistant response and we're done
                self.messages
                    .push(Message::new(Role::Assistant, response_text.as_str()));
                break;
            }

            // Tool calls received -- execute them and loop for the follow-up
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

    /// Add new output files to the allowed list
    ///
    /// This is used in chat mode when the user specifies new files in subsequent messages.
    /// The system prompt is updated to include the new files.
    pub fn add_output_files(&mut self, new_files: Vec<OutputFileSpec>) {
        for new_file in new_files {
            if !self
                .output_files
                .iter()
                .any(|f| f.filename == new_file.filename)
            {
                self.output_files.push(new_file);
            }
        }

        // Update system prompt with new file list
        if !self.output_files.is_empty() && !self.messages.is_empty() {
            // Build the new system prompt before borrowing messages mutably
            let new_system = self.build_system_prompt();

            if let Some(first_msg) = self.messages.first_mut() {
                if matches!(first_msg.role, Role::System) {
                    *first_msg = Message::new(Role::System, new_system.as_str());
                }
            }
        }
    }

    /// Build the system prompt including file output instructions
    fn build_system_prompt(&self) -> String {
        system_prompt::build_system_prompt(&self.model_entry, &self.output_files)
    }

    /// Build chat request from current message history
    fn build_request(&self) -> Result<ChatCompletionRequest> {
        let request = if self.output_files.is_empty() {
            ChatCompletionRequest::builder()
                .model(&self.model_entry.model_id)
                .messages(self.messages.clone())
                .temperature(0.5)
                .build()
                .context("Failed to build chat completion request")?
        } else {
            ChatCompletionRequest::builder()
                .model(&self.model_entry.model_id)
                .messages(self.messages.clone())
                .temperature(0.5)
                .typed_tool::<SaveFileParams>()
                .build()
                .context("Failed to build chat completion request")?
        };

        Ok(request)
    }

    /// Stream response, render progressively, and return text + any tool calls
    ///
    /// Uses ToolAwareStream to handle both plain responses and responses with
    /// tool calls in a single code path. Content is rendered in real-time while
    /// tool call fragments are accumulated internally by the stream wrapper.
    ///
    /// # Returns
    ///
    /// A tuple of (accumulated_text, tool_calls). If the model did not invoke
    /// any tools, `tool_calls` will be empty.
    async fn stream_response(
        &self,
        request: &ChatCompletionRequest,
    ) -> Result<(String, Vec<ToolCall>)> {
        // Detect if stdout is a terminal or piped
        let is_terminal = io::stdout().is_terminal();

        // Create appropriate renderer
        let mut renderer = if is_terminal {
            StreamRenderer::with_theme(&self.theme_name, self.inline_colors.clone())
        } else {
            StreamRenderer::with_plain_text()
        };

        let mut accumulated_text = String::new();
        let mut result_tool_calls: Vec<ToolCall> = Vec::new();

        let mut stream = self
            .client
            .stream_chat_completion_tool_aware(request)
            .await
            .context("Failed to start streaming chat completion")?;

        while let Some(event) = stream.next().await {
            match event {
                StreamEvent::ContentDelta(text) => {
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

        renderer
            .flush()
            .context("Failed to flush remaining content")?;
        println!(); // Newline after response

        Ok((accumulated_text, result_tool_calls))
    }

    /// Execute tool calls and add results to message history
    async fn execute_tool_calls(&mut self, tool_calls: &[ToolCall]) -> Result<()> {
        // Create file writer with allowed files
        let allowed_files: Vec<String> = self
            .output_files
            .iter()
            .map(|f| f.normalized_path.clone())
            .collect();
        let file_writer =
            FileWriter::new(allowed_files, self.auto_approve, self.inline_colors.clone());

        // Execute each tool call
        for tool_call in tool_calls {
            let result = match tool_call.name() {
                "save_file" => match tool_call.parse_params::<SaveFileParams>() {
                    Ok(params) => match file_writer.write_file(&params.path, &params.content)? {
                        true => format!("Successfully saved file: {}", params.path),
                        false => format!("User declined changes to: {}", params.path),
                        // Err(e) => format!("Error saving file: {}", e),
                    },
                    Err(e) => format!("Invalid save_file parameters: {}", e),
                },
                unknown => format!("Unknown tool: {}", unknown),
            };

            self.messages
                .push(Message::tool_response(&tool_call.id, result.as_str()));
        }

        Ok(())
    }
}

/// Format file references and prompt into a single user message
///
/// This is a helper function for building the initial message content.
/// File references are formatted as XML tags, followed by the prompt and STDIN.
pub fn build_user_message(
    file_references: &[FileReference],
    prompt: &str,
    stdin_content: Option<&str>,
) -> String {
    let mut parts = Vec::new();

    // Add file references
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

    // Add prompt
    if !prompt.is_empty() {
        parts.push(prompt.to_string());
    }

    // Add STDIN
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
}
