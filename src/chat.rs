//! Chat mode functionality for multi-turn conversations
//!
//! This module provides the interactive chat loop where users can have
//! back-and-forth conversations. The core session logic is handled by
//! the unified Session struct in session.rs - this module only handles
//! the interactive UI loop and user input reading.

use anyhow::{Context, Result};
use openrouter_rs::OpenRouterClient;
use std::io::{self, BufRead, Write};

use crate::input::{ParsedInput, parse_file_patterns_limited};
use crate::models::ModelEntry;
use crate::readline::ChatReadline;
use crate::session::{RetryKind, Session, SessionOptions, build_user_message};

/// Configuration for starting a chat session.
#[derive(Debug, Clone)]
pub struct ChatSessionOptions {
    pub initial_input: ParsedInput,
    pub session: SessionOptions,
    pub history_file: Option<String>,
    pub max_input_bytes: usize,
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
    let mut session_options = options.session;
    session_options.output_files = options.initial_input.output_files;

    // Build first message combining file references, prompt, and STDIN
    let first_message = build_user_message(
        &options.initial_input.file_references,
        &options.initial_input.prompt,
        options.initial_input.stdin_content.as_deref(),
    );

    // Create unified session
    let non_interactive = session_options.non_interactive;
    let prompt_color = session_options.inline_colors.get_prompt_color();
    let mut session = Session::new(client, model_entry, session_options);

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
            Err(error) => {
                if !retry_pending_turn(&mut session, error, non_interactive).await? {
                    return Ok(());
                }
            }
        }
    }

    // Main chat loop
    loop {
        // Prompt for next user input using readline
        match readline.read_input(prompt_color) {
            Ok(Some(input)) => {
                if input.is_empty() {
                    println!("(Empty input ignored)");
                    continue;
                }

                // Parse all file patterns from user input in a single pass
                let (final_input_prompt, file_refs, new_output_files) =
                    match parse_file_patterns_limited(&input, options.max_input_bytes) {
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
                    Err(error) => {
                        if !retry_pending_turn(&mut session, error, non_interactive).await? {
                            break;
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

async fn retry_pending_turn(
    session: &mut Session,
    mut error: anyhow::Error,
    non_interactive: bool,
) -> Result<bool> {
    loop {
        let retry_kind = session.retry_kind();
        if non_interactive || retry_kind.is_none() {
            return Err(error).context("Chat request failed");
        }

        eprintln!("\nError: {}", error);
        let prompt = match retry_kind {
            Some(RetryKind::PartialStream) => {
                "Retry this turn? Some response text may be repeated. [Y/n]: "
            }
            Some(RetryKind::ToolContinuation) => {
                "Continue this turn for another tool-call batch? [Y/n]: "
            }
            Some(RetryKind::Stream) => "Retry this turn? [Y/n]: ",
            None => unreachable!(),
        };
        eprint!("{}", prompt);
        io::stderr().flush()?;

        if !is_confirmation_approved(&read_retry_response()?) {
            return Ok(false);
        }

        match session.retry_pending().await {
            Ok(_) => return Ok(true),
            Err(next_error) => error = next_error,
        }
    }
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
