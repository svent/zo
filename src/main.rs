//! zo - OpenRouter CLI Assistant
//!
//! A command-line tool for interacting with large language models through OpenRouter.

use anyhow::{Context, Result, bail};
use clap::Parser;
use std::fs::File;
use std::io::{self, BufRead, Write};

mod chat;
mod client;
mod config;
mod file_ops;
mod input;
mod models;
mod readline;
mod render;
mod session;
mod system_prompt;
mod tools;

use config::Config;
use input::ParsedInput;
use models::ModelEntry;

/// zo - OpenRouter CLI Assistant
#[derive(Parser)]
#[command(
    name = "zo",
    about = "Zettabyte Oracle - A CLI tool for interacting with language models via OpenRouter",
    override_usage = r#"zo [OPTIONS] [PROMPT]
       zo --chat
       zo +ACTION"#,
    after_help = r#"
Actions:
    +init-config    Initialize configuration file with defaults and exit
    +list-models    Print a list of all available model names and their IDs

Examples:
    zo +list-models
    zo /sonnet "Explain lifetimes"
    zo --chat
    zo --chat "Let's talk"
    "#
)]
struct Cli {
    /// Prompt text optionally prefixed by /<model>
    #[arg(value_name = "PROMPT", trailing_var_arg = true)]
    args: Vec<String>,

    /// Override model selection (e.g., "gpt4", "sonnet")
    #[arg(short, long)]
    model: Option<String>,

    /// Enable debug mode - show diagnostic info and ask for confirmation before sending request
    #[arg(short, long)]
    debug: bool,

    /// Enable chat mode - have a multi-turn conversation
    #[arg(short, long)]
    chat: bool,

    /// Automatically approve all file changes without confirmation
    #[arg(long, short = 'y')]
    yes: bool,
}

/// Resolve which model to use based on inputs and config
///
/// Priority order:
/// 1. CLI --model flag
/// 2. Slash command in prompt
/// 3. Default model from config
/// 4. Hardcoded fallback (sonnet)
///
/// Returns (model_name, model_entry) tuple so we can display the short name in debug mode
fn resolve_model(model_override: Option<String>, config: &Config) -> Result<(String, ModelEntry)> {
    // Build model map from config and defaults
    let model_map = models::build_model_map(config);

    // Determine which model name to use
    let model_name = if let Some(override_name) = model_override {
        // Use explicit override (from CLI flag or slash command)
        override_name
    } else if let Some(default_model) = &config.default_model {
        // Use default model from config
        default_model.clone()
    } else {
        // Hardcoded fallback to sonnet (anthropic/claude-sonnet-4.5)
        "sonnet".to_string()
    };

    // Use fuzzy matching to find the model
    if let Some(model_entry) = models::select_model(&model_name, &model_map, config) {
        Ok((model_name, model_entry))
    } else {
        // Model not found - provide helpful error message
        let available_models = models::list_model_names(&model_map);
        let suggestions = models::get_fuzzy_matches(&model_name, &model_map, 3);

        let mut error_msg = format!("Model '{}' not found.", model_name);

        if !suggestions.is_empty() {
            error_msg.push_str("\n\nDid you mean:");
            for (name, model_id, score) in suggestions {
                error_msg.push_str(&format!("\n  {} -> {} (score: {})", name, model_id, score));
            }
        }

        error_msg.push_str("\n\nAvailable models:");
        for name in available_models.iter() {
            error_msg.push_str(&format!("\n  {}", name));
        }
        bail!(error_msg);
    }
}

/// Display debug information before sending request
///
/// Shows:
/// - Selected model (both short name and full ID)
/// - System prompt (if any)
/// - User prompt that will be sent
/// - File references (if any)
/// - STDIN content (if any)
fn display_debug_info(model_name: &str, model_entry: &ModelEntry, parsed_input: &ParsedInput) {
    println!("=== DEBUG MODE ===\n");

    println!("Model Selected:");
    println!("  Short name: {}", model_name);
    println!("  Full ID:    {}", model_entry.model_id);

    let system_prompt = system_prompt::build_system_prompt(model_entry, &parsed_input.output_files);

    if !system_prompt.is_empty() {
        println!("\nSystem Prompt:");
        println!("  {}", system_prompt);
    } else {
        println!("\nSystem Prompt: (none)");
    }

    println!("\nUser Prompt:");
    if parsed_input.prompt.is_empty() {
        println!("  (empty - using STDIN only)");
    } else {
        println!("  {}", parsed_input.prompt);
    }

    if !parsed_input.file_references.is_empty() {
        println!("\nFile References:");
        for file_ref in &parsed_input.file_references {
            println!("  - {}", file_ref.filename);
        }
    } else {
        println!("\nFile References: (none)");
    }

    if let Some(stdin) = &parsed_input.stdin_content {
        println!("\nSTDIN Content:");
        // Show first 500 chars of STDIN to avoid overwhelming output
        if stdin.len() > 500 {
            println!("  {}... ({} more chars)", &stdin[..500], stdin.len() - 500);
        } else {
            println!("  {}", stdin);
        }
    } else {
        println!("\nSTDIN Content: (none)");
    }

    if !parsed_input.output_files.is_empty() {
        println!("\nOutput Files:");
        for output_file in &parsed_input.output_files {
            println!(
                "  - {} ({})",
                output_file.filename,
                if output_file.include_as_input {
                    "read+write"
                } else {
                    "write-only"
                }
            );
        }
    } else {
        println!("\nOutput Files: (none)");
    }

    println!("\n==================\n");
}

/// Ask user for confirmation before proceeding
///
/// Returns true if user presses Enter (or types 'y'/'yes')
/// Returns false if user types 'n'/'no' or any other input
///
/// This function tries to read from /dev/tty if available (to handle piped STDIN),
/// otherwise falls back to regular stdin
fn ask_for_confirmation() -> Result<bool> {
    print!("Press Enter to continue (or type 'n' to cancel): ");
    io::stdout().flush().context("Failed to flush stdout")?;

    let mut input = String::new();

    // Try to read from /dev/tty first (works even when STDIN is piped)
    // This is Unix-specific but provides better UX when using piped input
    #[cfg(unix)]
    {
        if let Ok(tty) = File::open("/dev/tty") {
            let mut reader = io::BufReader::new(tty);
            reader
                .read_line(&mut input)
                .context("Failed to read user input from /dev/tty")?;
        } else {
            // Fallback to stdin if /dev/tty is not available
            io::stdin()
                .read_line(&mut input)
                .context("Failed to read user input")?;
        }
    }

    // On non-Unix systems, just use stdin
    #[cfg(not(unix))]
    {
        io::stdin()
            .read_line(&mut input)
            .context("Failed to read user input")?;
    }

    let input = input.trim().to_lowercase();

    // Empty input (just Enter) or explicit yes means continue
    // Anything else (including 'n', 'no', or random text) means cancel
    if input.is_empty() || input == "y" || input == "yes" {
        Ok(true)
    } else {
        Ok(false)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Parse command-line arguments
    let cli = Cli::parse();

    // Handle +actions first (must be first positional argument and start with '+')
    // Available: +init-config, +list-models
    if let Some(first_arg) = cli.args.get(0) {
        if first_arg.starts_with("+init-config") {
            return config::init_config();
        }
        if first_arg.starts_with("+list-models") {
            let config = config::load_config().context("Failed to load configuration")?;
            let model_map = models::build_model_map(&config);
            println!("Available models:");
            for name in models::list_model_names(&model_map) {
                if let Some(entry) = model_map.get(&name) {
                    println!("  {:<16} -> {}", name, entry.model_id);
                }
            }
            return Ok(());
        }
    }

    // If no arguments and not chat mode, show help
    if cli.args.is_empty() && !cli.chat {
        use clap::CommandFactory;
        let mut cmd = Cli::command();
        cmd.print_help()?;
        println!();
        return Ok(());
    }

    // Load configuration
    let config = config::load_config().context("Failed to load configuration")?;

    // Parse input (args + STDIN)
    let parsed_input = input::parse_input(cli.args.clone()).context("Failed to parse input")?;

    // Determine final model override (CLI flag takes precedence over slash command)
    let model_override = cli.model.or(parsed_input.model_override.clone());

    // Resolve which model to use (with fuzzy matching)
    let (model_name, model_entry) =
        resolve_model(model_override, &config).context("Failed to resolve model")?;

    // If debug mode is enabled, show diagnostic info and ask for confirmation
    if cli.debug {
        display_debug_info(&model_name, &model_entry, &parsed_input);

        if !ask_for_confirmation()? {
            println!("Cancelled.");
            return Ok(());
        }

        println!();
    }

    // Create API client
    let client = client::create_client(&config).context("Failed to create API client")?;

    // Get theme and colors from config
    let theme_name = config.theme.as_deref().unwrap_or("base16-ocean.dark");
    let inline_colors = config
        .inline_colors
        .clone()
        .unwrap_or_else(|| config::InlineColors::for_theme(theme_name));

    // Check if chat mode is enabled
    if cli.chat {
        // Run interactive chat session
        chat::run_chat_session(
            client,
            model_entry,
            chat::ChatSessionOptions {
                initial_prompt: parsed_input.prompt,
                initial_stdin: parsed_input.stdin_content,
                auto_approve: cli.yes,
                theme_name: theme_name.to_string(),
                inline_colors,
                history_file: config.history_file,
            },
        )
        .await
        .context("Chat session failed")?;
    } else {
        // Single-shot mode: create session and send one message

        // Validate that we have content
        let has_content = !parsed_input.prompt.trim().is_empty()
            || parsed_input.stdin_content.is_some()
            || !parsed_input.file_references.is_empty();

        if !has_content {
            bail!(
                "No prompt provided. Please provide a prompt, file reference (@filename), or pipe input via STDIN."
            );
        }

        // Build user message
        let user_message = session::build_user_message(
            &parsed_input.file_references,
            &parsed_input.prompt,
            parsed_input.stdin_content.as_deref(),
        );

        // Create session
        let mut session = session::Session::new(
            client,
            model_entry,
            parsed_input.output_files,
            cli.yes,
            theme_name.to_string(),
            inline_colors,
        );

        // Send message and get response
        session
            .send_message(user_message)
            .await
            .context("Failed to send message")?;
    }

    Ok(())
}
