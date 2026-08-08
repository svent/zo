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
mod shell;
mod system_prompt;
mod tools;

use config::{Config, ReasoningEffort};
use input::ParsedInput;
use models::{ModelEntry, ModelMatchKind, ResolvedModel};
use shell::ShellRuntime;
use tools::{FileCliAccess, FileToolMode, ToolAccess};

/// zo - OpenRouter CLI Assistant
#[derive(Parser)]
#[command(
    name = "zo",
    version = env!("CARGO_PKG_VERSION"),
    about = "Zettabyte Oracle - A CLI tool for interacting with language models via OpenRouter",
    override_usage = r#"zo [OPTIONS] [PROMPT]
       zo --chat
       zo --image <FILE> [PROMPT]
       zo +ACTION"#,
    after_help = r#"
Actions:
    +init-config    Initialize configuration file with defaults and exit
    +list-models    Print a list of all available model names and their IDs

Examples:
    zo +list-models
    zo /sol "Explain lifetimes"
    zo --files read "inspect this project"
    zo --files write --accept-writes "refactor the repo"
    zo --shell "run tests in this repo"
    zo --chat
    zo --chat "Let's talk"
    zo --image assets/cat.png "Watercolor cat portrait"
    "#
)]
struct Cli {
    /// Prompt text optionally prefixed by /<model>
    #[arg(value_name = "PROMPT", trailing_var_arg = true)]
    args: Vec<String>,

    /// Override model selection (e.g., "sol", "sonnet")
    #[arg(short, long)]
    model: Option<String>,

    /// Set reasoning effort for this request or chat session
    #[arg(long, value_enum, conflicts_with = "image")]
    reasoning_effort: Option<ReasoningEffort>,

    /// Enable debug mode - show diagnostic info and ask for confirmation before sending request
    #[arg(short, long, conflicts_with = "non_interactive")]
    debug: bool,

    /// Show tool calls requested by the model as they are executed
    #[arg(short, long)]
    verbose: bool,

    /// Enable chat mode - have a multi-turn conversation
    #[arg(short, long)]
    chat: bool,

    /// Generate a single image and save it to FILE
    #[arg(long, value_name = "FILE", conflicts_with_all = ["chat", "files", "shell", "policies"])]
    image: Option<String>,

    /// Approve file overwrites and edits without confirmation
    #[arg(long, short = 'y')]
    accept_writes: bool,

    /// Enable file tools: read or write
    #[arg(long, value_enum)]
    files: Option<FileCliAccess>,

    /// Enable shell tools
    #[arg(long)]
    shell: bool,

    /// Enable OpenRouter server-side web search
    #[arg(long, conflicts_with = "image")]
    web: bool,

    /// Activate named shell policies (comma-separated)
    #[arg(long, value_delimiter = ',')]
    policies: Vec<String>,

    /// Never prompt; anything requiring approval is denied unless already accepted
    #[arg(long)]
    non_interactive: bool,

    /// Allow tool access to hidden files/directories (dotfiles)
    #[arg(long)]
    hidden: bool,

    /// Maximum aggregate bytes accepted for one submitted turn
    #[arg(long, value_name = "BYTES")]
    max_input_bytes: Option<usize>,

    /// Maximum serialized conversation bytes retained for an API request
    #[arg(long, value_name = "BYTES")]
    max_session_bytes: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    InitConfig,
    ListModels,
}

fn parse_action(cli: &Cli) -> Result<Option<Action>> {
    let Some(first) = cli.args.first() else {
        return Ok(None);
    };
    if !first.starts_with('+') {
        return Ok(None);
    }

    let action = match first.as_str() {
        "+init-config" => Action::InitConfig,
        "+list-models" => Action::ListModels,
        unknown => bail!(
            "Unknown action '{}'. Valid actions are +init-config and +list-models.",
            unknown
        ),
    };

    if cli.args.len() != 1
        || cli.model.is_some()
        || cli.reasoning_effort.is_some()
        || cli.debug
        || cli.verbose
        || cli.chat
        || cli.image.is_some()
        || cli.accept_writes
        || cli.files.is_some()
        || cli.shell
        || cli.web
        || !cli.policies.is_empty()
        || cli.non_interactive
        || cli.hidden
        || cli.max_input_bytes.is_some()
        || cli.max_session_bytes.is_some()
    {
        bail!("{} must be used as a standalone action", first);
    }

    Ok(Some(action))
}

/// Resolve a named model using aliases, config overrides, and fuzzy matching.
///
/// Returns (model_name, model_entry) tuple so we can display the short name in debug mode
fn resolve_named_model(model_name: String, config: &Config) -> Result<ResolvedModel> {
    // Build model map from config and defaults
    let model_map = models::build_model_map(config);

    // Use fuzzy matching to find the model
    match models::resolve_model(&model_name, &model_map, config) {
        Ok(resolved) => Ok(resolved),
        Err(models::ModelSelectionError::Ambiguous { aliases, .. }) => {
            let mut error_msg = format!("Model '{}' is ambiguous. Matching aliases:", model_name);
            for alias in aliases {
                if let Some(entry) = model_map.get(&alias) {
                    error_msg.push_str(&format!("\n  {} -> {}", alias, entry.model_id));
                }
            }
            bail!(error_msg)
        }
        Err(models::ModelSelectionError::NotFound(_)) => {
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
}

/// Resolve a model for text/chat requests.
///
/// Priority order:
/// 1. CLI --model flag
/// 2. Slash command in prompt
/// 3. Default model from config
/// 4. Built-in fallback model
fn resolve_text_model(model_override: Option<String>, config: &Config) -> Result<ResolvedModel> {
    if let Some(override_name) = model_override {
        resolve_named_model(override_name, config)
    } else if let Some(default_model) = &config.default_model {
        resolve_named_model(default_model.clone(), config)
    } else {
        Ok(ResolvedModel {
            canonical_alias: None,
            entry: ModelEntry {
                model_id: models::DEFAULT_TEXT_MODEL_ID.to_string(),
                system_prompt: None,
                reasoning_effort: None,
            },
            match_kind: ModelMatchKind::DirectId,
        })
    }
}

/// Resolve a model for image requests.
///
/// Image mode honors explicit CLI/slash selection, otherwise it falls back to a
/// built-in image-capable model instead of using the text default from config.
fn resolve_image_model(
    model_override: Option<String>,
    config: &Config,
) -> Result<(ResolvedModel, bool)> {
    if let Some(model_name) = model_override {
        Ok((resolve_named_model(model_name, config)?, false))
    } else {
        Ok((
            ResolvedModel {
                canonical_alias: None,
                entry: ModelEntry {
                    model_id: image::DEFAULT_IMAGE_MODEL_ID.to_string(),
                    system_prompt: None,
                    reasoning_effort: None,
                },
                match_kind: ModelMatchKind::DirectId,
            },
            true,
        ))
    }
}

fn resolve_reasoning_effort(
    request: Option<ReasoningEffort>,
    model: Option<ReasoningEffort>,
    global: Option<ReasoningEffort>,
) -> ReasoningEffort {
    request
        .or(model)
        .or(global)
        .unwrap_or(ReasoningEffort::High)
}

fn resolved_model_name(model: &ResolvedModel) -> &str {
    model
        .canonical_alias
        .as_deref()
        .unwrap_or(&model.entry.model_id)
}

fn display_verbose_model(model: &ResolvedModel) {
    match model.match_kind {
        ModelMatchKind::DirectId => eprintln!("Model: {}", model.entry.model_id),
        _ => eprintln!(
            "Model: {} -> {}",
            model.canonical_alias.as_deref().unwrap_or("<unknown>"),
            model.entry.model_id
        ),
    }
}

fn preview_text(input: &str, max_chars: usize) -> (String, usize) {
    let total = input.chars().count();
    if total <= max_chars {
        return (input.to_string(), 0);
    }
    (input.chars().take(max_chars).collect(), total - max_chars)
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
    tool_access: ToolAccess,
    active_policies: &[String],
    accept_writes: bool,
    non_interactive: bool,
    allow_hidden: bool,
    web_search: bool,
    reasoning_effort: ReasoningEffort,
) {
    println!("=== DEBUG MODE ===\n");

    println!("Model Selected:");
    println!("  Short name: {}", model_name);
    println!("  Full ID:    {}", model_entry.model_id);
    println!(
        "  Reasoning:  {}{}",
        reasoning_effort.as_str(),
        if reasoning_effort == ReasoningEffort::Auto {
            " (OpenRouter default)"
        } else {
            ""
        }
    );

    let system_prompt = system_prompt::build_system_prompt(
        model_entry,
        &parsed_input.output_files,
        tool_access,
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
        let (preview, omitted) = preview_text(stdin, 500);
        if omitted > 0 {
            println!("  {}... ({} more chars)", preview, omitted);
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
    match tool_access.file_mode {
        FileToolMode::Disabled => println!("  file tools: disabled"),
        FileToolMode::ReadOnly => {
            println!("  file tools: read (constrained read tools + scoped writes)")
        }
        FileToolMode::ReadWrite => println!("  file tools: write (full workspace tools)"),
    }
    println!(
        "  shell tools: {}",
        if tool_access.shell_enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    if !active_policies.is_empty() {
        println!("  shell policies: {}", active_policies.join(", "));
    }
    println!(
        "  overwrite approvals: {}",
        if accept_writes {
            "auto (--accept-writes)"
        } else {
            "ask when needed"
        }
    );
    println!(
        "  interactive confirmations: {}",
        if non_interactive {
            "disabled (--non-interactive)"
        } else {
            "enabled"
        }
    );
    println!(
        "  hidden paths: {}",
        if allow_hidden {
            "enabled (--hidden)"
        } else {
            "blocked by default"
        }
    );
    println!(
        "  web search: {}",
        if web_search { "enabled" } else { "disabled" }
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
    let (preview, omitted) = preview_text(prompt, 500);
    if omitted > 0 {
        println!("  {}... ({} more chars)", preview, omitted);
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
        .image
        .clone()
        .expect("clap should require an output path when --image is set");
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

    let (resolved_model, using_default_image_model) =
        resolve_image_model(model_override, config).context("Failed to resolve image model")?;
    let model_name = resolved_model_name(&resolved_model);
    let model_entry = &resolved_model.entry;

    let client = client::create_client(config).context("Failed to create API client")?;
    let modalities =
        image::derive_image_modalities(&client, &model_entry.model_id, using_default_image_model)
            .await
            .context("Failed to determine image output modalities")?;

    if cli.debug {
        display_image_debug_info(
            model_name,
            model_entry,
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
        resolved_model.entry,
        &user_message,
        modalities,
        image::ImageGenerationOptions {
            output_path,
            accept_writes: cli.accept_writes,
            allow_hidden: cli.hidden,
            non_interactive: cli.non_interactive,
        },
    )
    .await
    .context("Image generation failed")
}

#[tokio::main]
async fn main() -> Result<()> {
    // Parse command-line arguments
    let cli = Cli::parse();

    if let Some(action) = parse_action(&cli)? {
        match action {
            Action::InitConfig => return config::init_config(),
            Action::ListModels => {
                let config = config::load_config().context("Failed to load configuration")?;
                let model_map = models::build_model_map(&config);
                println!("Configured model aliases:");
                for name in models::list_model_names(&model_map) {
                    if let Some(entry) = model_map.get(&name) {
                        println!("  {:<16} -> {}", name, entry.model_id);
                    }
                }
                return Ok(());
            }
        }
    }

    // If no arguments and not chat mode, show help
    if cli.args.is_empty() && !cli.chat && cli.image.is_none() {
        use clap::CommandFactory;
        let mut cmd = Cli::command();
        cmd.print_help()?;
        println!();
        return Ok(());
    }

    // Load configuration
    let config = config::load_config().context("Failed to load configuration")?;
    let max_input_bytes = cli.max_input_bytes.unwrap_or(config.limits.max_input_bytes);
    let max_session_bytes = cli
        .max_session_bytes
        .unwrap_or(config.limits.max_session_bytes);
    if max_input_bytes == 0 {
        bail!("--max-input-bytes must be greater than zero");
    }
    if max_session_bytes == 0 {
        bail!("--max-session-bytes must be greater than zero");
    }

    // Parse input (args + STDIN)
    let parsed_input = if cli.image.is_some() {
        input::parse_image_input_limited(cli.args.clone(), max_input_bytes)
            .context("Failed to parse image input")?
    } else {
        input::parse_input_limited(cli.args.clone(), max_input_bytes)
            .context("Failed to parse input")?
    };

    // Determine final model override (CLI flag takes precedence over slash command)
    let model_override = cli.model.clone().or(parsed_input.model_override.clone());

    if cli.image.is_some() {
        return run_image_mode(&cli, &config, parsed_input, model_override).await;
    }

    let web_search = cli.web || config.web;
    let tool_access = ToolAccess::from_cli(cli.files, cli.shell);
    if !cli.shell && !cli.policies.is_empty() {
        bail!("--policies requires --shell");
    }
    #[cfg(not(unix))]
    if cli.shell {
        bail!("Shell tools are only supported on Unix platforms in this version.");
    }
    let show_tool_calls = cli.debug || cli.verbose;
    let tool_log_mode = if cli.debug {
        session::ToolLogMode::Full
    } else if cli.verbose {
        session::ToolLogMode::Compact
    } else {
        session::ToolLogMode::Off
    };

    let shell_runtime = if tool_access.shell_enabled {
        let config_dir = config::get_config_dir().context("Failed to resolve config directory")?;
        let policy_registry = shell::load_shell_policy_registry(&config.shell, &config_dir)
            .context("Failed to load shell policies")?;
        Some(
            ShellRuntime::new_with_policy_registry(
                &config.shell,
                &policy_registry,
                &cli.policies,
                cli.non_interactive,
                show_tool_calls,
            )
            .context("Invalid shell policy selection")?,
        )
    } else {
        None
    };
    let active_shell_policies = shell_runtime
        .as_ref()
        .map(|runtime| runtime.active_policy_names().to_vec())
        .unwrap_or_default();

    // Resolve which model to use (with fuzzy matching)
    let resolved_model =
        resolve_text_model(model_override, &config).context("Failed to resolve model")?;
    let model_name = resolved_model_name(&resolved_model);
    let model_entry = &resolved_model.entry;
    let reasoning_effort = resolve_reasoning_effort(
        cli.reasoning_effort,
        model_entry.reasoning_effort,
        config.reasoning_effort,
    );

    if cli.verbose && !cli.debug {
        display_verbose_model(&resolved_model);
    }

    // If debug mode is enabled, show diagnostic info and ask for confirmation
    if cli.debug {
        display_debug_info(
            model_name,
            model_entry,
            &parsed_input,
            tool_access,
            &active_shell_policies,
            cli.accept_writes,
            cli.non_interactive,
            cli.hidden,
            web_search,
            reasoning_effort,
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
            resolved_model.entry,
            chat::ChatSessionOptions {
                initial_input: parsed_input,
                session: session::SessionOptions {
                    output_files: Vec::new(),
                    accept_writes: cli.accept_writes,
                    theme_name: theme_name.to_string(),
                    inline_colors,
                    tool_access,
                    web_search,
                    reasoning_effort,
                    shell_runtime: shell_runtime.clone(),
                    non_interactive: cli.non_interactive,
                    allow_hidden: cli.hidden,
                    tool_log_mode,
                    max_session_bytes,
                },
                history_file: config.history_file,
                max_input_bytes,
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
            resolved_model.entry,
            session::SessionOptions {
                output_files: parsed_input.output_files,
                accept_writes: cli.accept_writes,
                theme_name: theme_name.to_string(),
                inline_colors,
                tool_access,
                web_search,
                reasoning_effort,
                shell_runtime: shell_runtime.clone(),
                non_interactive: cli.non_interactive,
                allow_hidden: cli.hidden,
                tool_log_mode,
                max_session_bytes,
            },
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
    use crate::config::ShellConfig;

    #[test]
    fn test_resolve_text_model_uses_builtin_fallback_when_config_default_missing() {
        let config = Config {
            api_key: None,
            default_model: None,
            reasoning_effort: None,
            web: false,
            models: Some(std::collections::HashMap::new()),
            custom_models: Vec::new(),
            theme: None,
            inline_colors: None,
            history_file: None,
            shell: ShellConfig::default(),
            limits: config::LimitsConfig::default(),
        };

        let resolved = resolve_text_model(None, &config).unwrap();

        assert_eq!(
            resolved_model_name(&resolved),
            models::DEFAULT_TEXT_MODEL_ID
        );
        assert_eq!(resolved.entry.model_id, models::DEFAULT_TEXT_MODEL_ID);
        assert_eq!(resolved.entry.system_prompt, None);
    }

    #[test]
    fn test_resolve_text_model_uses_config_default_when_present() {
        let config = Config {
            api_key: None,
            default_model: Some("mydefault".to_string()),
            reasoning_effort: None,
            web: false,
            models: Some(std::collections::HashMap::from([(
                "mydefault".to_string(),
                "provider/custom-model".to_string(),
            )])),
            custom_models: Vec::new(),
            theme: None,
            inline_colors: None,
            history_file: None,
            shell: ShellConfig::default(),
            limits: config::LimitsConfig::default(),
        };

        let resolved = resolve_text_model(None, &config).unwrap();

        assert_eq!(resolved_model_name(&resolved), "mydefault");
        assert_eq!(resolved.entry.model_id, "provider/custom-model");
    }

    #[test]
    fn test_reasoning_effort_precedence_auto_reset_and_high_default() {
        assert_eq!(
            resolve_reasoning_effort(
                Some(ReasoningEffort::Low),
                Some(ReasoningEffort::High),
                Some(ReasoningEffort::Medium),
            ),
            ReasoningEffort::Low
        );
        assert_eq!(
            resolve_reasoning_effort(
                None,
                Some(ReasoningEffort::High),
                Some(ReasoningEffort::Medium),
            ),
            ReasoningEffort::High
        );
        assert_eq!(
            resolve_reasoning_effort(None, None, Some(ReasoningEffort::Medium)),
            ReasoningEffort::Medium
        );
        assert_eq!(
            resolve_reasoning_effort(
                Some(ReasoningEffort::Auto),
                Some(ReasoningEffort::High),
                Some(ReasoningEffort::Medium),
            ),
            ReasoningEffort::Auto
        );
        assert_eq!(
            resolve_reasoning_effort(
                None,
                Some(ReasoningEffort::Auto),
                Some(ReasoningEffort::High)
            ),
            ReasoningEffort::Auto
        );
        assert_eq!(
            resolve_reasoning_effort(None, None, None),
            ReasoningEffort::High
        );
    }

    #[test]
    fn test_reasoning_effort_flag_parses_and_rejects_invalid_values() {
        let cases = [
            ("auto", ReasoningEffort::Auto),
            ("max", ReasoningEffort::Max),
            ("xhigh", ReasoningEffort::Xhigh),
            ("high", ReasoningEffort::High),
            ("medium", ReasoningEffort::Medium),
            ("low", ReasoningEffort::Low),
            ("minimal", ReasoningEffort::Minimal),
            ("none", ReasoningEffort::None),
        ];
        for (value, expected) in cases {
            let cli = Cli::try_parse_from(["zo", "--reasoning-effort", value, "hello"]).unwrap();
            assert_eq!(cli.reasoning_effort, Some(expected));
        }

        let result = Cli::try_parse_from(["zo", "--reasoning-effort", "extreme", "hello"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_files_flag_explicit_read() {
        let cli = Cli::try_parse_from(["zo", "--files", "read", "hello"]).unwrap();
        assert_eq!(cli.files, Some(FileCliAccess::Read));
        assert_eq!(cli.args, vec!["hello"]);
    }

    #[test]
    fn test_files_flag_explicit_write() {
        let cli = Cli::try_parse_from(["zo", "--files", "write", "hello"]).unwrap();
        assert_eq!(cli.files, Some(FileCliAccess::Write));
        assert_eq!(cli.args, vec!["hello"]);
    }

    #[test]
    fn test_files_flag_absent() {
        let cli = Cli::try_parse_from(["zo", "hello"]).unwrap();
        assert_eq!(cli.files, None);
    }

    #[test]
    fn test_files_flag_invalid_mode() {
        let result = Cli::try_parse_from(["zo", "--files", "invalid", "hello"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_shell_flag_composes_with_files() {
        let cli = Cli::try_parse_from(["zo", "--files", "read", "--shell", "hello"]).unwrap();
        assert_eq!(cli.files, Some(FileCliAccess::Read));
        assert!(cli.shell);
    }

    #[test]
    fn test_accept_writes_flag_present() {
        let cli = Cli::try_parse_from(["zo", "--accept-writes", "hello"]).unwrap();
        assert!(cli.accept_writes);
    }

    #[test]
    fn test_non_interactive_conflicts_with_debug() {
        let result = Cli::try_parse_from(["zo", "--debug", "--non-interactive", "hello"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_verbose_flag_present() {
        let cli = Cli::try_parse_from(["zo", "--verbose", "hello"]).unwrap();
        assert!(cli.verbose);
    }

    #[test]
    fn test_actions_require_exact_standalone_syntax() {
        let list = Cli::try_parse_from(["zo", "+list-models"]).unwrap();
        assert_eq!(parse_action(&list).unwrap(), Some(Action::ListModels));

        let typo = Cli::try_parse_from(["zo", "+list-models-typo"]).unwrap();
        assert!(parse_action(&typo).is_err());

        let trailing = Cli::try_parse_from(["zo", "+init-config", "extra"]).unwrap();
        assert!(parse_action(&trailing).is_err());

        let flagged = Cli::try_parse_from(["zo", "--verbose", "+list-models"]).unwrap();
        assert!(parse_action(&flagged).is_err());

        let reasoned =
            Cli::try_parse_from(["zo", "--reasoning-effort", "high", "+list-models"]).unwrap();
        assert!(parse_action(&reasoned).is_err());
    }

    #[test]
    fn test_preview_text_is_unicode_safe() {
        let input = format!("{}étail", "a".repeat(499));
        let (preview, omitted) = preview_text(&input, 500);
        assert_eq!(preview.chars().count(), 500);
        assert!(preview.ends_with('é'));
        assert_eq!(omitted, 4);
    }

    #[test]
    fn test_limit_flags_parse() {
        let cli = Cli::try_parse_from([
            "zo",
            "--max-input-bytes",
            "1024",
            "--max-session-bytes",
            "4096",
            "hello",
        ])
        .unwrap();
        assert_eq!(cli.max_input_bytes, Some(1024));
        assert_eq!(cli.max_session_bytes, Some(4096));
    }

    #[test]
    fn test_web_flag_present() {
        let cli = Cli::try_parse_from(["zo", "--web", "hello"]).unwrap();
        assert!(cli.web);
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
    fn test_image_requires_output_path() {
        let result = Cli::try_parse_from(["zo", "--image"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_output_flag_is_removed() {
        let result = Cli::try_parse_from(["zo", "--output", "out.png", "hello"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_image_conflicts_with_chat() {
        let result = Cli::try_parse_from(["zo", "--chat", "--image", "out.png", "hello"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_image_conflicts_with_files() {
        let result = Cli::try_parse_from(["zo", "--files", "read", "--image", "out.png", "hello"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_image_conflicts_with_shell() {
        let result = Cli::try_parse_from(["zo", "--shell", "--image", "out.png", "hello"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_image_conflicts_with_web() {
        let result = Cli::try_parse_from(["zo", "--web", "--image", "out.png", "hello"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_image_conflicts_with_reasoning_effort() {
        let result = Cli::try_parse_from([
            "zo",
            "--reasoning-effort",
            "high",
            "--image",
            "out.png",
            "hello",
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn test_image_flag_parses_output_path() {
        let cli = Cli::try_parse_from(["zo", "--image", "out.png", "hello"]).unwrap();
        assert_eq!(cli.image.as_deref(), Some("out.png"));
        assert_eq!(cli.args, vec!["hello"]);
    }

    #[test]
    fn test_image_flag_parses_output_path_with_following_option() {
        let cli =
            Cli::try_parse_from(["zo", "--image", "out.png", "--accept-writes", "hello"]).unwrap();
        assert_eq!(cli.image.as_deref(), Some("out.png"));
        assert!(cli.accept_writes);
        assert_eq!(cli.args, vec!["hello"]);
    }
}
