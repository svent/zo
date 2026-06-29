//! Chat mode functionality for multi-turn conversations
//!
//! This module provides the interactive chat loop where users can have
//! back-and-forth conversations. The core session logic is handled by
//! the unified Session struct in session.rs - this module only handles
//! the interactive UI loop and user input reading.

use anyhow::{Context, Result};
use openrouter_rs::OpenRouterClient;
use std::io::{self, BufRead, Write};

use crate::config::InlineColors;
use crate::input::parse_file_patterns;
use crate::models::ModelEntry;
use crate::readline::ChatReadline;
use crate::session::{Session, build_user_message};
use crate::shell::ShellRuntime;
use crate::tools::ToolAccess;

/// Configuration for starting a chat session.
#[derive(Debug, Clone)]
pub struct ChatSessionOptions {
    /// Initial user prompt (can be empty if STDIN is provided)
    pub initial_prompt: String,
    /// Optional STDIN content to include in first message
    pub initial_stdin: Option<String>,
    /// Whether to auto-approve file overwrites and edits
    pub accept_writes: bool,
    /// Theme for markdown rendering
    pub theme_name: String,
    /// Colors for inline markdown
    pub inline_colors: InlineColors,
    /// Optional path to history file for persistence
    pub history_file: Option<String>,
    /// Tool access for this chat session
    pub tool_access: ToolAccess,
    /// Whether to enable OpenRouter server-side web search
    pub web_search: bool,
    /// Optional shell runtime for this chat session
    pub shell_runtime: Option<ShellRuntime>,
    /// Whether confirmation prompts should be suppressed
    pub non_interactive: bool,
    /// Whether hidden files/directories are accessible to tools
    pub allow_hidden: bool,
    /// Whether to log model-requested tool calls during execution
    pub show_tool_calls: bool,
    /// Whether to show full tool arguments in logs (debug mode)
    pub show_full_tool_args: bool,
}

/// Run an interactive chat session
///
/// # Arguments
///
/// * `client` - OpenRouter API client
/// * `model_entry` - Model configuration
/// * `options` - Chat session configuration
///
/// The session runs in a loop:
/// 1. Send current message to API and display response
/// 2. Prompt user for next input
/// 3. Parse file references and output files
/// 4. Repeat until user exits
pub async fn run_chat_session(
    client: OpenRouterClient,
    model_entry: ModelEntry,
    options: ChatSessionOptions,
) -> Result<()> {
    // Parse all file patterns from initial prompt in a single pass
    let (final_initial_prompt, initial_file_refs, output_files) =
        parse_file_patterns(&options.initial_prompt)
            .context("Failed to parse file patterns from initial prompt")?;

    // Build first message combining file references, prompt, and STDIN
    let first_message = build_user_message(
        &initial_file_refs,
        &final_initial_prompt,
        options.initial_stdin.as_deref(),
    );

    // Create unified session
    let mut session = Session::new(
        client,
        model_entry,
        output_files.clone(),
        options.accept_writes,
        options.theme_name.clone(),
        options.inline_colors.clone(),
        options.tool_access,
        options.web_search,
        options.shell_runtime.clone(),
        options.non_interactive,
        options.allow_hidden,
        options.show_tool_calls,
        options.show_full_tool_args,
    );

    // Create readline with optional history
    let mut readline = ChatReadline::new(options.history_file.as_deref())
        .context("Failed to initialize readline")?;

    println!("=== Chat Mode ===");
    println!("Type your messages below.");
    println!("For multiline input: Alt-Enter, Ctrl-O, or Ctrl-J to add a newline.");
    println!("Type 'exit', 'quit', or press Ctrl+D to end the conversation.\n");

    // Send first message only if there's content
    let has_initial_content = !first_message.trim().is_empty();
    if has_initial_content {
        match session.send_message(first_message).await {
            Ok(_) => {
                // Response already displayed during streaming
            }
            Err(e) => {
                if options.non_interactive {
                    return Err(e).context("Initial chat request failed");
                }
                eprintln!("\nError: {}", e);
                eprintln!("Would you like to retry? [Y/n]: ");
                io::stdout().flush()?;

                let retry = read_retry_response()?;

                if !is_confirmation_approved(&retry) {
                    return Ok(()); // Exit chat
                }
                // Retry happens naturally on next loop iteration
            }
        }
    }

    // Main chat loop
    loop {
        // Prompt for next user input using readline
        match readline.read_input(options.inline_colors.get_prompt_color()) {
            Ok(Some(input)) => {
                if input.is_empty() {
                    println!("(Empty input ignored)");
                    continue;
                }

                // Parse all file patterns from user input in a single pass
                let (final_input_prompt, file_refs, new_output_files) =
                    match parse_file_patterns(&input) {
                        Ok(result) => result,
                        Err(e) => {
                            eprintln!("\nError parsing file patterns: {}", e);
                            continue;
                        }
                    };

                // Add new output files to session
                if !new_output_files.is_empty() {
                    session.add_output_files(new_output_files);
                }

                // Build message with file references (use expanded prompt)
                let message = build_user_message(&file_refs, &final_input_prompt, None);

                // Send message and get response
                match session.send_message(message).await {
                    Ok(_) => {
                        // Response already displayed during streaming
                    }
                    Err(e) => {
                        if options.non_interactive {
                            return Err(e).context("Chat request failed");
                        }
                        eprintln!("\nError: {}", e);
                        eprintln!("Would you like to retry? [Y/n]: ");
                        io::stdout().flush()?;

                        let retry = read_retry_response()?;

                        if is_confirmation_approved(&retry) {
                            continue; // Retry the same request
                        } else {
                            break; // Exit chat
                        }
                    }
                }
            }
            Ok(None) => {
                // User wants to exit
                println!("\nGoodbye!");
                break;
            }
            Err(e) => {
                eprintln!("\nError reading input: {}", e);
                break;
            }
        }
    }

    Ok(())
}

fn read_retry_response() -> Result<String> {
    let mut response = String::new();

    #[cfg(unix)]
    {
        use std::fs::File;
        if let Ok(tty) = File::open("/dev/tty") {
            let mut reader = io::BufReader::new(tty);
            reader.read_line(&mut response)?;
            return Ok(response);
        }
    }

    io::stdin().read_line(&mut response)?;
    Ok(response)
}

fn is_confirmation_approved(response: &str) -> bool {
    let response = response.trim();
    response.is_empty()
        || response.eq_ignore_ascii_case("y")
        || response.eq_ignore_ascii_case("yes")
}

#[cfg(test)]
mod tests {
    use super::is_confirmation_approved;

    #[test]
    fn test_confirmation_defaults_to_yes() {
        assert!(is_confirmation_approved(""));
        assert!(is_confirmation_approved("y"));
        assert!(is_confirmation_approved("yes"));
        assert!(is_confirmation_approved(" Y "));
        assert!(!is_confirmation_approved("n"));
        assert!(!is_confirmation_approved("no"));
        assert!(!is_confirmation_approved("anything else"));
    }
}
