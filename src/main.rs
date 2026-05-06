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
mod image;
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
use tools::{ToolMode, ToolsCliMode};

/// zo - OpenRouter CLI Assistant
#[derive(Parser)]
#[command(
    name = "zo",
    version = env!("CARGO_PKG_VERSION"),
    about = "Zettabyte Oracle - A CLI tool for interacting with language models via OpenRouter",
    override_usage = r#"zo [OPTIONS] [PROMPT]
       zo --chat
       zo --image -o <FILE> [PROMPT]
       zo +ACTION"#,
    after_help = r#"
Actions:
    +init-config    Initialize configuration file with defaults and exit
    +list-models    Print a list of all available model names and their IDs

Examples:
    zo +list-models
    zo /codex "Explain lifetimes"
    zo --tools ro "inspect this project"
    zo --tools rw "refactor the repo"
    zo --chat
    zo --chat "Let's talk"
    zo --image -o assets/cat.png "Watercolor cat portrait"
    "#
)]
struct Cli {
    /// Prompt text optionally prefixed by /<model>
    #[arg(value_name = "PROMPT", trailing_var_arg = true)]
    args: Vec<String>,

    /// Override model selection (e.g., "codex", "sonnet")
    #[arg(short, long)]
    model: Option<String>,

    /// Enable debug mode - show diagnostic info and ask for confirmation before sending request
    #[arg(short, long)]
    debug: bool,

    /// Show tool calls requested by the model as they are executed
    #[arg(short, long)]
    verbose: bool,

    /// Enable chat mode - have a multi-turn conversation
    #[arg(short, long)]
    chat: bool,

    /// Generate a single image and exit
    #[arg(long, requires = "output", conflicts_with_all = ["chat", "tools"])]
    image: bool,

    /// Save generated image to FILE (required with --image)
    #[arg(short, long, value_name = "FILE", requires = "image")]
    output: Option<String>,

    /// Automatically approve all file changes without confirmation
    #[arg(long, short = 'y')]
    yes: bool,

    /// Enable comprehensive tool access
    #[arg(short = 't', long, value_enum)]
    tools: Option<ToolsCliMode>,

    /// Allow tool access to hidden files/directories (dotfiles)
    #[arg(long)]
    hidden: bool,
}

/// Resolve a named model using aliases, config overrides, and fuzzy matching.
///
/// Returns (model_name, model_entry) tuple so we can display the short name in debug mode
fn resolve_named_model(model_name: String, config: &Config) -> Result<(String, ModelEntry)> {
    // Build model map from config and defaults
    let model_map = models::build_model_map(config);

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

/// Resolve a model for text/chat requests.
///
/// Priority order:
/// 1. CLI --model flag
/// 2. Slash command in prompt
/// 3. Default model from config
/// 4. Built-in fallback model
fn resolve_text_model(
    model_override: Option<String>,
    config: &Config,
) -> Result<(String, ModelEntry)> {
    if let Some(override_name) = model_override {
        resolve_named_model(override_name, config)
    } else if let Some(default_model) = &config.default_model {
        resolve_named_model(default_model.clone(), config)
    } else {
        Ok((
            models::DEFAULT_TEXT_MODEL_ID.to_string(),
            ModelEntry {
                model_id: models::DEFAULT_TEXT_MODEL_ID.to_string(),
                system_prompt: None,
            },
        ))
    }
}

/// Resolve a model for image requests.
///
/// Image mode honors explicit CLI/slash selection, otherwise it falls back to a
/// built-in image-capable model instead of using the text default from config.
fn resolve_image_model(
    model_override: Option<String>,
    config: &Config,
) -> Result<(String, ModelEntry, bool)> {
    if let Some(model_name) = model_override {
        let (model_name, model_entry) = resolve_named_model(model_name, config)?;
        Ok((model_name, model_entry, false))
    } else {
        Ok((
            image::DEFAULT_IMAGE_MODEL_ID.to_string(),
            ModelEntry {
                model_id: image::DEFAULT_IMAGE_MODEL_ID.to_string(),
                system_prompt: None,
            },
            true,
        ))
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
fn display_debug_info(
    model_name: &str,
    model_entry: &ModelEntry,
    parsed_input: &ParsedInput,
    tool_mode: ToolMode,
    allow_hidden: bool,
) {
    println!("=== DEBUG MODE ===\n");

    println!("Model Selected:");
    println!("  Short name: {}", model_name);
    println!("  Full ID:    {}", model_entry.model_id);

    let system_prompt = system_prompt::build_system_prompt(
        model_entry,
        &parsed_input.output_files,
        tool_mode,
        allow_hidden,
    );

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

    println!("\nTool Mode:");
    match tool_mode {
        ToolMode::Disabled => println!("  disabled"),
        ToolMode::ReadOnly => println!("  ro (constrained read tools + scoped writes)"),
        ToolMode::ReadWrite => println!("  rw (full workspace tools)"),
    }
    println!(
        "  hidden paths: {}",
        if allow_hidden {
            "enabled (--hidden)"
        } else {
            "blocked by default"
        }
    );

    println!("\n==================\n");
}

fn display_image_debug_info(
    model_name: &str,
    model_entry: &ModelEntry,
    prompt: &str,
    output_path: &str,
    modalities: &[openrouter_rs::api::chat::Modality],
) {
    println!("=== DEBUG MODE ===\n");

    println!("Model Selected:");
    println!("  Short name: {}", model_name);
    println!("  Full ID:    {}", model_entry.model_id);

    if let Some(system_prompt) = &model_entry.system_prompt {
        println!("\nSystem Prompt:");
        println!("  {}", system_prompt);
    } else {
        println!("\nSystem Prompt: (none)");
    }

    println!("\nDerived Modalities:");
    println!("  {}", image::format_modalities(modalities));

    println!("\nPrompt Preview:");
    if prompt.len() > 500 {
        println!(
            "  {}... ({} more chars)",
            &prompt[..500],
            prompt.len() - 500
        );
    } else {
        println!("  {}", prompt);
    }

    println!("\nOutput Path:");
    println!("  {}", output_path);

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

async fn run_image_mode(
    cli: &Cli,
    config: &Config,
    parsed_input: ParsedInput,
    model_override: Option<String>,
) -> Result<()> {
    let output_path = cli
        .output
        .clone()
        .expect("clap should require --output when --image is set");
    file_ops::validate_binary_output_path(&output_path, cli.hidden)
        .context("Invalid image output path")?;

    let has_content =
        !parsed_input.prompt.trim().is_empty() || parsed_input.stdin_content.is_some();
    if !has_content {
        bail!("No prompt provided. Please provide a prompt or pipe input via STDIN.");
    }

    let user_message = session::build_user_message(
        &[],
        &parsed_input.prompt,
        parsed_input.stdin_content.as_deref(),
    );

    let (model_name, model_entry, using_default_image_model) =
        resolve_image_model(model_override, config).context("Failed to resolve image model")?;

    let client = client::create_client(config).context("Failed to create API client")?;
    let modalities =
        image::derive_image_modalities(&client, &model_entry.model_id, using_default_image_model)
            .await
            .context("Failed to determine image output modalities")?;

    if cli.debug {
        display_image_debug_info(
            &model_name,
            &model_entry,
            &user_message,
            &output_path,
            &modalities,
        );

        if !ask_for_confirmation()? {
            println!("Cancelled.");
            return Ok(());
        }

        println!();
    }

    image::run_image_generation(
        client,
        model_entry,
        &user_message,
        modalities,
        image::ImageGenerationOptions {
            output_path,
            auto_approve: cli.yes,
            allow_hidden: cli.hidden,
        },
    )
    .await
    .context("Image generation failed")
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
    if cli.args.is_empty() && !cli.chat && !cli.image {
        use clap::CommandFactory;
        let mut cmd = Cli::command();
        cmd.print_help()?;
        println!();
        return Ok(());
    }

    // Load configuration
    let config = config::load_config().context("Failed to load configuration")?;

    // Parse input (args + STDIN)
    let parsed_input = if cli.image {
        input::parse_image_input(cli.args.clone()).context("Failed to parse image input")?
    } else {
        input::parse_input(cli.args.clone()).context("Failed to parse input")?
    };

    // Determine final model override (CLI flag takes precedence over slash command)
    let model_override = cli.model.clone().or(parsed_input.model_override.clone());

    if cli.image {
        return run_image_mode(&cli, &config, parsed_input, model_override).await;
    }

    // Resolve tool mode from CLI
    let tool_mode = ToolMode::from(cli.tools);
    let show_tool_calls = cli.debug || cli.verbose;
    let show_full_tool_args = cli.debug;

    // Resolve which model to use (with fuzzy matching)
    let (model_name, model_entry) =
        resolve_text_model(model_override, &config).context("Failed to resolve model")?;

    // If debug mode is enabled, show diagnostic info and ask for confirmation
    if cli.debug {
        display_debug_info(
            &model_name,
            &model_entry,
            &parsed_input,
            tool_mode,
            cli.hidden,
        );

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
                tool_mode,
                allow_hidden: cli.hidden,
                show_tool_calls,
                show_full_tool_args,
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
            tool_mode,
            cli.hidden,
            show_tool_calls,
            show_full_tool_args,
        );

        // Send message and get response
        session
            .send_message(user_message)
            .await
            .context("Failed to send message")?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_text_model_uses_builtin_fallback_when_config_default_missing() {
        let config = Config {
            api_key: None,
            default_model: None,
            models: Some(std::collections::HashMap::new()),
            custom_models: Vec::new(),
            theme: None,
            inline_colors: None,
            history_file: None,
        };

        let (model_name, model_entry) = resolve_text_model(None, &config).unwrap();

        assert_eq!(model_name, models::DEFAULT_TEXT_MODEL_ID);
        assert_eq!(model_entry.model_id, models::DEFAULT_TEXT_MODEL_ID);
        assert_eq!(model_entry.system_prompt, None);
    }

    #[test]
    fn test_resolve_text_model_uses_config_default_when_present() {
        let config = Config {
            api_key: None,
            default_model: Some("mydefault".to_string()),
            models: Some(std::collections::HashMap::from([(
                "mydefault".to_string(),
                "provider/custom-model".to_string(),
            )])),
            custom_models: Vec::new(),
            theme: None,
            inline_colors: None,
            history_file: None,
        };

        let (model_name, model_entry) = resolve_text_model(None, &config).unwrap();

        assert_eq!(model_name, "mydefault");
        assert_eq!(model_entry.model_id, "provider/custom-model");
    }

    #[test]
    fn test_tools_flag_explicit_ro() {
        let cli = Cli::try_parse_from(["zo", "--tools", "ro", "hello"]).unwrap();
        assert_eq!(cli.tools, Some(ToolsCliMode::Ro));
        assert_eq!(cli.args, vec!["hello"]);
    }

    #[test]
    fn test_tools_flag_explicit_rw() {
        let cli = Cli::try_parse_from(["zo", "--tools", "rw", "hello"]).unwrap();
        assert_eq!(cli.tools, Some(ToolsCliMode::Rw));
        assert_eq!(cli.args, vec!["hello"]);
    }

    #[test]
    fn test_tools_flag_absent() {
        let cli = Cli::try_parse_from(["zo", "hello"]).unwrap();
        assert_eq!(cli.tools, None);
    }

    #[test]
    fn test_tools_flag_invalid_mode() {
        let result = Cli::try_parse_from(["zo", "--tools invalid", "hello"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_verbose_flag_present() {
        let cli = Cli::try_parse_from(["zo", "--verbose", "hello"]).unwrap();
        assert!(cli.verbose);
    }

    #[test]
    fn test_debug_implies_tool_call_logging() {
        let cli = Cli::try_parse_from(["zo", "--debug", "hello"]).unwrap();
        let show_tool_calls = cli.debug || cli.verbose;
        assert!(show_tool_calls);
    }

    #[test]
    fn test_hidden_flag_absent_by_default() {
        let cli = Cli::try_parse_from(["zo", "hello"]).unwrap();
        assert!(!cli.hidden);
    }

    #[test]
    fn test_hidden_flag_present() {
        let cli = Cli::try_parse_from(["zo", "--hidden", "hello"]).unwrap();
        assert!(cli.hidden);
    }

    #[test]
    fn test_image_requires_output() {
        let result = Cli::try_parse_from(["zo", "--image", "hello"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_output_requires_image() {
        let result = Cli::try_parse_from(["zo", "--output", "out.png", "hello"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_image_conflicts_with_chat() {
        let result = Cli::try_parse_from(["zo", "--image", "--chat", "-o", "out.png", "hello"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_image_conflicts_with_tools() {
        let result =
            Cli::try_parse_from(["zo", "--image", "--tools", "ro", "-o", "out.png", "hello"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_image_output_flag_parses() {
        let cli = Cli::try_parse_from(["zo", "--image", "-o", "out.png", "hello"]).unwrap();
        assert!(cli.image);
        assert_eq!(cli.output.as_deref(), Some("out.png"));
    }
}
