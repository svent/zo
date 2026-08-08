use crate::models::{DEFAULT_MODELS, DEFAULT_TEXT_MODEL_NAME};
use anyhow::{Context, Result};
use crossterm::style::Color;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Main configuration structure
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// OpenRouter API key (optional, can use env var instead)
    pub api_key: Option<String>,

    /// Default model to use when none specified
    pub default_model: Option<String>,

    /// Enable OpenRouter server-side web search by default
    #[serde(default)]
    pub web: bool,

    /// Model mappings (shortname -> OpenRouter model ID)
    /// Adds new aliases, overrides built-in aliases, or disables built-in aliases with ""
    #[serde(default)]
    pub models: Option<std::collections::HashMap<String, String>>,

    /// User-defined custom/virtual models
    #[serde(default)]
    pub custom_models: Vec<CustomModel>,

    /// Syntax highlighting theme for code blocks
    /// Available themes: "base16-ocean.dark", "base16-ocean.light",
    /// "InspiredGitHub", "Solarized (dark)", "Solarized (light)", etc.
    pub theme: Option<String>,

    /// Custom colors for inline markdown elements
    #[serde(default)]
    pub inline_colors: Option<InlineColors>,

    /// Path to chat history file (enables history persistence when set)
    pub history_file: Option<String>,

    /// Shell execution defaults. Policy rules live in ~/.config/zo/policies/.
    #[serde(default)]
    pub shell: ShellConfig,

    /// Safety limits for submitted input and retained conversation context.
    #[serde(default)]
    pub limits: LimitsConfig,
}

pub const DEFAULT_MAX_INPUT_BYTES: usize = 1024 * 1024;
pub const DEFAULT_MAX_SESSION_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LimitsConfig {
    #[serde(default = "default_max_input_bytes")]
    pub max_input_bytes: usize,
    #[serde(default = "default_max_session_bytes")]
    pub max_session_bytes: usize,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
            max_session_bytes: DEFAULT_MAX_SESSION_BYTES,
        }
    }
}

fn default_max_input_bytes() -> usize {
    DEFAULT_MAX_INPUT_BYTES
}

fn default_max_session_bytes() -> usize {
    DEFAULT_MAX_SESSION_BYTES
}

/// Custom model definition
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomModel {
    /// Virtual model name (e.g., "myassistant")
    pub name: String,

    /// Actual OpenRouter model ID
    pub model: String,

    /// Optional system prompt for this model
    pub system_prompt: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ShellPolicyAction {
    Allow,
    Ask,
    Deny,
}

impl Default for ShellPolicyAction {
    fn default() -> Self {
        Self::Ask
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ShellConfig {
    #[serde(default)]
    pub default_action: ShellPolicyAction,
    #[serde(default = "default_allowed_shells")]
    pub allowed_shells: Vec<String>,
    /// Deprecated TOML policy rules. Parsed only to emit a migration error.
    #[serde(default)]
    pub always_on: Vec<toml::Value>,
    /// Deprecated TOML policy sets. Parsed only to emit a migration error.
    #[serde(default)]
    pub policy_sets: Vec<toml::Value>,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            default_action: ShellPolicyAction::Ask,
            allowed_shells: default_allowed_shells(),
            always_on: Vec::new(),
            policy_sets: Vec::new(),
        }
    }
}

fn default_allowed_shells() -> Vec<String> {
    vec![
        "/bin/sh".to_string(),
        "/bin/bash".to_string(),
        "/bin/zsh".to_string(),
    ]
}

/// Custom colors for inline markdown elements
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct InlineColors {
    /// Color for headings (e.g., "cyan", "blue", "#0088FF")
    pub heading: Option<String>,

    /// Color for inline code (e.g., "yellow", "magenta", "#FF8800")
    pub inline_code: Option<String>,

    /// Color for emphasis/strong text (e.g., "white", "black")
    pub emphasis: Option<String>,

    /// Color for chat prompt symbol (e.g., "cyan", "green", "#00FF88")
    pub prompt: Option<String>,
}

impl Default for InlineColors {
    fn default() -> Self {
        Self {
            heading: Some("cyan".to_string()),
            inline_code: Some("yellow".to_string()),
            emphasis: Some("white".to_string()),
            prompt: Some("cyan".to_string()),
        }
    }
}

impl InlineColors {
    /// Create inline colors optimized for the given theme.
    ///
    /// Light themes get darker colors, dark themes get brighter colors.
    pub fn for_theme(theme_name: &str) -> Self {
        if theme_name.contains("light") || theme_name == "InspiredGitHub" {
            // Light theme defaults - use darker colors
            Self {
                heading: Some("blue".to_string()),
                inline_code: Some("magenta".to_string()),
                emphasis: Some("black".to_string()),
                prompt: Some("blue".to_string()),
            }
        } else {
            // Dark theme defaults - use brighter colors
            Self::default()
        }
    }

    /// Parse a named color or hex string into a terminal color.
    ///
    /// Supported values:
    /// - Named colors: black, red/green/yellow/blue/magenta/cyan/white
    /// - Dark variants: darkred/darkgreen/darkyellow/darkblue/darkmagenta/darkcyan
    /// - Grays: grey/gray, darkgrey/darkgray
    /// - Light aliases: lightred/lightgreen/lightyellow/lightblue/lightmagenta/lightcyan
    /// - Hex colors: #RRGGBB
    pub(crate) fn parse_color(value: &str) -> Option<Color> {
        let normalized = value.trim().to_lowercase();
        match normalized.as_str() {
            "black" => Some(Color::Black),
            "darkred" => Some(Color::DarkRed),
            "red" | "lightred" => Some(Color::Red),
            "darkgreen" => Some(Color::DarkGreen),
            "green" | "lightgreen" => Some(Color::Green),
            "darkyellow" => Some(Color::DarkYellow),
            "yellow" | "lightyellow" => Some(Color::Yellow),
            "darkblue" => Some(Color::DarkBlue),
            "blue" | "lightblue" => Some(Color::Blue),
            "darkmagenta" => Some(Color::DarkMagenta),
            "magenta" | "lightmagenta" => Some(Color::Magenta),
            "darkcyan" => Some(Color::DarkCyan),
            "cyan" | "lightcyan" => Some(Color::Cyan),
            "grey" | "gray" => Some(Color::Grey),
            "darkgrey" | "darkgray" => Some(Color::DarkGrey),
            "white" => Some(Color::White),
            // RGB hex format: #RRGGBB
            hex if hex.starts_with('#') && hex.len() == 7 => {
                let r = u8::from_str_radix(&hex[1..3], 16).ok()?;
                let g = u8::from_str_radix(&hex[3..5], 16).ok()?;
                let b = u8::from_str_radix(&hex[5..7], 16).ok()?;
                Some(Color::Rgb { r, g, b })
            }
            _ => None,
        }
    }

    fn validate(&self) -> Result<()> {
        self.validate_field("heading", &self.heading)?;
        self.validate_field("inline_code", &self.inline_code)?;
        self.validate_field("emphasis", &self.emphasis)?;
        self.validate_field("prompt", &self.prompt)?;
        Ok(())
    }

    fn validate_field(&self, field_name: &str, value: &Option<String>) -> Result<()> {
        if let Some(color_value) = value {
            if Self::parse_color(color_value).is_none() {
                anyhow::bail!(
                    "Invalid inline_colors.{} value '{}'. Use a supported named color (e.g., cyan, lightblue, darkgray) or hex color #RRGGBB.",
                    field_name,
                    color_value
                );
            }
        }
        Ok(())
    }
}

fn validate_shell_config(shell: &ShellConfig) -> Result<()> {
    if shell.allowed_shells.is_empty() {
        anyhow::bail!("shell.allowed_shells must contain at least one shell path");
    }

    let mut seen_shells = std::collections::HashSet::new();
    for (index, shell_path) in shell.allowed_shells.iter().enumerate() {
        if shell_path.trim().is_empty() {
            anyhow::bail!("shell.allowed_shells[{}] must not be empty", index);
        }
        if !Path::new(shell_path).is_absolute() {
            anyhow::bail!(
                "shell.allowed_shells[{}] must be an absolute path: {}",
                index,
                shell_path
            );
        }
        if !seen_shells.insert(shell_path) {
            anyhow::bail!("Duplicate shell.allowed_shells entry '{}'", shell_path);
        }
    }

    if !shell.always_on.is_empty() {
        anyhow::bail!(
            "shell.always_on is no longer supported. Move shell policies to files under ~/.config/zo/policies/; for example, put default rules in ~/.config/zo/policies/default using lines like `allow git status`."
        );
    }

    if !shell.policy_sets.is_empty() {
        anyhow::bail!(
            "shell.policy_sets is no longer supported. Move named shell policies to files under ~/.config/zo/policies/; for example, ~/.config/zo/policies/coding is activated with `--policies coding`."
        );
    }

    Ok(())
}

/// Get the config file path.
///
/// Returns `~/.config/zo/config.toml` on both Linux and macOS.
pub fn get_config_path() -> Result<PathBuf> {
    Ok(get_config_dir()?.join("config.toml"))
}

/// Get the config directory path.
///
/// Returns `~/.config/zo` on both Linux and macOS.
pub fn get_config_dir() -> Result<PathBuf> {
    let home_dir = dirs::home_dir().context("Could not determine home directory")?;

    Ok(home_dir.join(".config").join("zo"))
}

/// Load configuration from file.
///
/// If the config file doesn't exist, returns the default configuration.
/// Otherwise, reads and parses the TOML config file and validates it.
///
/// # Errors
/// Returns an error if:
/// - The config file exists but cannot be read
/// - The TOML syntax is invalid
/// - The configuration validation fails (empty names, duplicate models, etc.)
pub fn load_config() -> Result<Config> {
    let config_path = get_config_path()?;

    // If config file doesn't exist, return default config
    if !config_path.exists() {
        return Ok(get_default_config());
    }

    // Read and parse config file
    let config_content = fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config file: {}", config_path.display()))?;

    let config: Config = toml::from_str(&config_content)
        .with_context(|| format!("Failed to parse config file: {}", config_path.display()))?;

    // Validate the config
    validate_config(&config)
        .with_context(|| format!("Invalid configuration in {}", config_path.display()))?;

    Ok(config)
}

/// Validate configuration
///
/// Checks for:
/// - Custom model names are not empty
/// - Custom model IDs are not empty
/// - No duplicate custom model names
/// - Theme name is valid (if specified)
/// - Inline color values are valid (if specified)
/// - Shell policy configuration is valid (if specified)
fn validate_config(config: &Config) -> Result<()> {
    if config.limits.max_input_bytes == 0 {
        anyhow::bail!("limits.max_input_bytes must be greater than zero");
    }
    if config.limits.max_session_bytes == 0 {
        anyhow::bail!("limits.max_session_bytes must be greater than zero");
    }

    if let Some(models) = &config.models {
        for (name, model_id) in models {
            if name.trim().is_empty() {
                anyhow::bail!(
                    "Model alias names in [models] cannot be empty. Please specify a non-empty alias."
                );
            }

            if model_id.trim().is_empty()
                && !DEFAULT_MODELS
                    .iter()
                    .any(|(default_name, _)| *default_name == name)
            {
                anyhow::bail!(
                    "Model alias '{}' in [models] has an empty model ID. Empty values are only valid for disabling built-in aliases.",
                    name
                );
            }
        }
    }

    // Track custom model names to detect duplicates
    let mut seen_names = std::collections::HashSet::new();

    for (i, custom_model) in config.custom_models.iter().enumerate() {
        // Check for empty name
        if custom_model.name.trim().is_empty() {
            anyhow::bail!(
                "Custom model at index {} has an empty name. Each custom model must have a non-empty name.",
                i
            );
        }

        // Check for empty model ID
        if custom_model.model.trim().is_empty() {
            anyhow::bail!(
                "Custom model '{}' has an empty model ID. Please specify a valid OpenRouter model ID.",
                custom_model.name
            );
        }

        // Check for duplicate names
        if !seen_names.insert(custom_model.name.to_lowercase()) {
            anyhow::bail!(
                "Duplicate custom model name '{}'. Each custom model must have a unique name.",
                custom_model.name
            );
        }
    }

    // Validate theme if specified
    if let Some(theme_name) = &config.theme {
        use syntect::highlighting::ThemeSet;

        let theme_set = ThemeSet::load_defaults();
        if !theme_set.themes.contains_key(theme_name) {
            let available: Vec<&str> = theme_set.themes.keys().map(|s| s.as_str()).collect();

            anyhow::bail!(
                "Invalid theme '{}'. Available themes:\n  Dark themes:\n    - base16-ocean.dark\n    - base16-eighties.dark\n    - base16-mocha.dark\n    - Solarized (dark)\n  Light themes:\n    - InspiredGitHub\n    - base16-ocean.light\n    - Solarized (light)\n\nAll available: {}",
                theme_name,
                available.join(", ")
            );
        }
    }

    // Validate inline colors if specified
    if let Some(inline_colors) = &config.inline_colors {
        inline_colors.validate()?;
    }

    validate_shell_config(&config.shell)?;

    Ok(())
}

/// Get default configuration.
///
/// Returns a config with:
/// - No API key (must be provided via env var or config file)
/// - Default model: "sol" (OpenAI GPT-5.6 Sol)
/// - No custom model mappings (uses built-in defaults)
/// - Empty custom models list
/// - Default theme: "base16-ocean.dark" (good for dark terminals)
/// - Default inline colors (None, will use theme-appropriate defaults)
pub fn get_default_config() -> Config {
    Config {
        api_key: None,
        default_model: Some(DEFAULT_TEXT_MODEL_NAME.to_string()),
        web: false,
        models: None,
        custom_models: Vec::new(),
        theme: Some("base16-ocean.dark".to_string()),
        inline_colors: None,
        history_file: None, // History disabled by default
        shell: ShellConfig::default(),
        limits: LimitsConfig::default(),
    }
}

fn build_default_models_example_block() -> String {
    let mut block = String::from("# [models]\n");

    for (name, model_id) in DEFAULT_MODELS {
        block.push_str(&format!("# {} = \"{}\"\n", name, model_id));
    }

    block
}

fn render_init_config_content() -> String {
    let default_models_example_block = build_default_models_example_block();

    format!(
        r##"# zo Configuration File
# https://github.com/svent/zo

# OpenRouter API key
# You can also set this via the OPENROUTER_API_KEY environment variable
# Get your API key from: https://openrouter.ai/keys
# api_key = "sk-or-v1-..."

# Default model to use when none is specified
# This will be used if you don't provide a /model command or --model flag
# Use short names like "sol", "terra", "luna", "sonnet", "flash", "gpt4o", etc.
default_model = "{default_model}"

# Enable OpenRouter server-side web search for text and chat requests
# You can also enable this per request with --web.
web = false

# Chat history file path (uncomment to enable history persistence)
# When set, chat history will be saved and restored between sessions
# history_file = "~/.zo/history.txt"

# Syntax highlighting theme for code blocks
# Available themes:
#   Dark themes (for dark terminal backgrounds):
#     - "base16-ocean.dark" (default)
#     - "base16-eighties.dark"
#     - "base16-mocha.dark"
#     - "Solarized (dark)"
#   
#   Light themes (for light terminal backgrounds):
#     - "InspiredGitHub" (recommended for light backgrounds)
#     - "base16-ocean.light"
#     - "Solarized (light)"
#
# If you have a light terminal background, try "InspiredGitHub"!
theme = "base16-ocean.dark"

# Custom colors for inline markdown elements
# This allows you to customize the appearance of text formatting
# Supported color names:
#   black, red, green, yellow, blue, magenta, cyan, white
#   darkred, darkgreen, darkyellow, darkblue, darkmagenta, darkcyan
#   gray/grey, darkgray/darkgrey
#   lightred, lightgreen, lightyellow, lightblue, lightmagenta, lightcyan
# Or use hex format: "#RRGGBB"
#
# [inline_colors]
# heading = "cyan"        # Color for headers
# inline_code = "yellow"  # Color for inline code
# emphasis = "white"      # Color for italic and bold text
# prompt = "cyan"         # Color for chat prompt symbol
#
# Example for light terminal backgrounds:
# [inline_colors]
# heading = "blue"
# inline_code = "magenta"
# emphasis = "black"
# prompt = "blue"

# Shell execution defaults and policies
# Shell execution is disabled unless you pass --shell.
# Even with --files read --shell, spawned commands still run with your normal user permissions.
#
# [shell]
# default_action = "ask"   # allow | ask | deny
# allowed_shells = ["/bin/sh", "/bin/bash", "/bin/zsh"]
# Policy rules live in ~/.config/zo/policies/.
# For example, ~/.config/zo/policies/default can contain:
#   allow git status
#   deny gh auth **
#   allow gh pr view /\d+/
#   #TEST allow git status
#   #TEST deny gh auth login
#   #TEST allow gh pr view 100
#   #TEST default gh pr view abc

# Model mappings (shortname -> OpenRouter model ID)
# Built-in aliases stay available by default.
# Use this table to override a built-in alias, add a new alias, or disable a built-in alias
# by setting it to an empty string.
#
# Example - uncomment to use your own model list:
{default_models_example_block}
# Disable a built-in alias if it gets in the way of fuzzy matching:
# [models]
# sonnet = ""
#
# Custom model definitions
# Define virtual model names that map to actual OpenRouter models
# You can optionally include a system prompt for each custom model
#
# Example:
# [[custom_models]]
# name = "code"
# model = "anthropic/claude-sonnet-4.5"
# system_prompt = "You are an expert programmer. Provide concise, well-commented code."
#
# [[custom_models]]
# name = "writer"
# model = "openai/gpt-4o"
# system_prompt = "You are a professional writer. Write clearly and engagingly."

# Byte-based safety limits. These are not model token limits.
[limits]
max_input_bytes = {max_input_bytes}
max_session_bytes = {max_session_bytes}
"##,
        default_model = DEFAULT_TEXT_MODEL_NAME,
        max_input_bytes = DEFAULT_MAX_INPUT_BYTES,
        max_session_bytes = DEFAULT_MAX_SESSION_BYTES,
    )
}

fn render_default_policy_content() -> String {
    r#"# zo shell policy: default
# This policy is used when --shell is enabled and no --policies value is provided.
#
# Rules use:
#   <allow|ask|deny> <program> <arg-patterns...>
#
# Examples:
#   allow git status
#   deny gh auth **
#   allow gh pr view /\d+/
#
# Inline tests are checked when shell mode starts.
# Rules are not allowed after the first #TEST line.
#
# Examples:
#   #TEST allow git status
#   #TEST deny gh auth login
#   #TEST allow gh pr view 100
#   #TEST default gh pr view abc
"#
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_config_valid() {
        let config = Config {
            api_key: Some("test-key".to_string()),
            default_model: Some("sol".to_string()),
            web: false,
            models: None,
            custom_models: vec![
                CustomModel {
                    name: "mymodel".to_string(),
                    model: "anthropic/claude-3.5-sonnet".to_string(),
                    system_prompt: Some("Test prompt".to_string()),
                },
                CustomModel {
                    name: "another".to_string(),
                    model: "openai/gpt-4o".to_string(),
                    system_prompt: None,
                },
            ],
            theme: Some("base16-ocean.dark".to_string()),
            inline_colors: None,
            history_file: None,
            shell: ShellConfig::default(),
            limits: LimitsConfig::default(),
        };

        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn test_web_defaults_false_when_missing() {
        let config: Config = toml::from_str(r#"default_model = "sol""#).unwrap();

        assert!(!config.web);
    }

    #[test]
    fn test_web_can_be_enabled_from_config() {
        let config: Config = toml::from_str("web = true").unwrap();

        assert!(config.web);
    }

    #[test]
    fn test_limits_default_when_missing() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.limits, LimitsConfig::default());
    }

    #[test]
    fn test_validate_config_rejects_zero_limits() {
        let mut config = get_default_config();
        config.limits.max_input_bytes = 0;
        assert!(validate_config(&config).is_err());

        config.limits.max_input_bytes = DEFAULT_MAX_INPUT_BYTES;
        config.limits.max_session_bytes = 0;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_rendered_config_contains_valid_limits() {
        let content = render_init_config_content();
        let config: Config = toml::from_str(&content).unwrap();
        assert_eq!(config.limits, LimitsConfig::default());
    }

    #[test]
    fn test_validate_config_empty_model_name() {
        let config = Config {
            api_key: None,
            default_model: None,
            web: false,
            models: None,
            custom_models: vec![CustomModel {
                name: "".to_string(),
                model: "anthropic/claude-3.5-sonnet".to_string(),
                system_prompt: None,
            }],
            theme: None,
            inline_colors: None,
            history_file: None,
            shell: ShellConfig::default(),
            limits: LimitsConfig::default(),
        };

        let result = validate_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty name"));
    }

    #[test]
    fn test_validate_config_empty_model_id() {
        let config = Config {
            api_key: None,
            default_model: None,
            web: false,
            models: None,
            custom_models: vec![CustomModel {
                name: "mymodel".to_string(),
                model: "".to_string(),
                system_prompt: None,
            }],
            theme: None,
            inline_colors: None,
            history_file: None,
            shell: ShellConfig::default(),
            limits: LimitsConfig::default(),
        };

        let result = validate_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty model ID"));
    }

    #[test]
    fn test_validate_config_duplicate_names() {
        let config = Config {
            api_key: None,
            default_model: None,
            web: false,
            models: None,
            custom_models: vec![
                CustomModel {
                    name: "mymodel".to_string(),
                    model: "anthropic/claude-3.5-sonnet".to_string(),
                    system_prompt: None,
                },
                CustomModel {
                    name: "MyModel".to_string(), // Case-insensitive duplicate
                    model: "openai/gpt-4o".to_string(),
                    system_prompt: None,
                },
            ],
            theme: None,
            inline_colors: None,
            history_file: None,
            shell: ShellConfig::default(),
            limits: LimitsConfig::default(),
        };

        let result = validate_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Duplicate"));
    }

    #[test]
    fn test_validate_config_allows_empty_model_for_builtin_disable() {
        let config = Config {
            api_key: None,
            default_model: None,
            web: false,
            models: Some(std::collections::HashMap::from([(
                "sonnet".to_string(),
                "".to_string(),
            )])),
            custom_models: vec![],
            theme: None,
            inline_colors: None,
            history_file: None,
            shell: ShellConfig::default(),
            limits: LimitsConfig::default(),
        };

        let result = validate_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_config_rejects_empty_model_for_unknown_alias() {
        let config = Config {
            api_key: None,
            default_model: None,
            web: false,
            models: Some(std::collections::HashMap::from([(
                "myalias".to_string(),
                "".to_string(),
            )])),
            custom_models: vec![],
            theme: None,
            inline_colors: None,
            history_file: None,
            shell: ShellConfig::default(),
            limits: LimitsConfig::default(),
        };

        let result = validate_config(&config);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Empty values are only valid for disabling built-in aliases")
        );
    }

    #[test]
    fn test_validate_config_valid_theme() {
        let config = Config {
            api_key: None,
            default_model: None,
            web: false,
            models: None,
            custom_models: vec![],
            theme: Some("base16-ocean.dark".to_string()),
            inline_colors: None,
            history_file: None,
            shell: ShellConfig::default(),
            limits: LimitsConfig::default(),
        };

        let result = validate_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_config_invalid_theme() {
        let config = Config {
            api_key: None,
            default_model: None,
            web: false,
            models: None,
            custom_models: vec![],
            theme: Some("nonexistent-theme".to_string()),
            inline_colors: None,
            history_file: None,
            shell: ShellConfig::default(),
            limits: LimitsConfig::default(),
        };

        let result = validate_config(&config);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Invalid theme"));
    }

    #[test]
    fn test_validate_config_valid_inline_colors() {
        let config = Config {
            api_key: None,
            default_model: None,
            web: false,
            models: None,
            custom_models: vec![],
            theme: None,
            inline_colors: Some(InlineColors {
                heading: Some("lightblue".to_string()),
                inline_code: Some("#FF8800".to_string()),
                emphasis: Some("darkgray".to_string()),
                prompt: Some("  CYAN ".to_string()),
            }),
            history_file: None,
            shell: ShellConfig::default(),
            limits: LimitsConfig::default(),
        };

        let result = validate_config(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_config_invalid_inline_color_name() {
        let config = Config {
            api_key: None,
            default_model: None,
            web: false,
            models: None,
            custom_models: vec![],
            theme: None,
            inline_colors: Some(InlineColors {
                heading: Some("bluish".to_string()),
                inline_code: None,
                emphasis: None,
                prompt: None,
            }),
            history_file: None,
            shell: ShellConfig::default(),
            limits: LimitsConfig::default(),
        };

        let result = validate_config(&config);
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(error.contains("inline_colors.heading"));
        assert!(error.contains("bluish"));
    }

    #[test]
    fn test_validate_config_invalid_inline_color_hex() {
        let config = Config {
            api_key: None,
            default_model: None,
            web: false,
            models: None,
            custom_models: vec![],
            theme: None,
            inline_colors: Some(InlineColors {
                heading: None,
                inline_code: Some("#12G45Z".to_string()),
                emphasis: None,
                prompt: None,
            }),
            history_file: None,
            shell: ShellConfig::default(),
            limits: LimitsConfig::default(),
        };

        let result = validate_config(&config);
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(error.contains("inline_colors.inline_code"));
        assert!(error.contains("#12G45Z"));
    }

    #[test]
    fn test_validate_config_rejects_legacy_shell_policy_sets() {
        let config = Config {
            api_key: None,
            default_model: None,
            web: false,
            models: None,
            custom_models: vec![],
            theme: None,
            inline_colors: None,
            history_file: None,
            shell: ShellConfig {
                policy_sets: vec![toml::Value::Table(toml::map::Map::new())],
                ..ShellConfig::default()
            },
            limits: LimitsConfig::default(),
        };

        let result = validate_config(&config);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("shell.policy_sets is no longer supported")
        );
    }

    #[test]
    fn test_validate_config_rejects_legacy_shell_always_on() {
        let config: Config = toml::from_str(
            r#"
                [shell]

                [[shell.always_on]]
                action = "allow"
                program = "git"
            "#,
        )
        .unwrap();

        let result = validate_config(&config);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("shell.always_on is no longer supported")
        );
    }

    #[test]
    fn test_render_init_config_content_uses_default_model_constant() {
        let content = render_init_config_content();

        assert!(content.contains(&format!("default_model = \"{}\"", DEFAULT_TEXT_MODEL_NAME)));
        assert!(content.contains("web = false"));
    }

    #[test]
    fn test_render_init_config_content_lists_all_default_models() {
        let content = render_init_config_content();

        for (name, model_id) in DEFAULT_MODELS {
            assert!(content.contains(&format!("# {} = \"{}\"", name, model_id)));
        }
    }
}

/// Save configuration to file.
///
/// Creates the parent directory if it doesn't exist, then serializes
/// the config to TOML and writes it to the config file.
///
/// # Errors
/// Returns an error if:
/// - The config directory cannot be created
/// - The config cannot be serialized to TOML
/// - The file cannot be written
#[allow(dead_code)]
pub fn save_config(config: &Config) -> Result<()> {
    let config_path = get_config_path()?;

    // Create parent directory if it doesn't exist
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create config directory: {}", parent.display()))?;
    }

    // Serialize config to TOML
    let toml_content =
        toml::to_string_pretty(config).context("Failed to serialize config to TOML")?;

    // Write to file
    fs::write(&config_path, toml_content)
        .with_context(|| format!("Failed to write config file: {}", config_path.display()))?;

    Ok(())
}

/// Initialize config file with defaults and helpful comments.
///
/// Creates a new config file at the default location with:
/// - Commented examples for all configuration options
/// - Default model set to the built-in text default
/// - Example custom model definitions
/// - Instructions for getting an API key
///
/// # Errors
/// Returns an error if:
/// - A config file already exists (will not overwrite)
/// - The config directory cannot be created
/// - The file cannot be written
pub fn init_config() -> Result<()> {
    let config_path = get_config_path()?;
    let default_policy_path = get_config_dir()?.join("policies").join("default");

    if config_path.exists() {
        anyhow::bail!(
            "Config file already exists at: {}\nRemove it first if you want to reinitialize.",
            config_path.display()
        );
    }
    if default_policy_path.exists() {
        anyhow::bail!(
            "Default policy file already exists at: {}\nRemove it first if you want to reinitialize.",
            default_policy_path.display()
        );
    }

    // Create parent directory if it doesn't exist
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create config directory: {}", parent.display()))?;
    }

    // Generate config file with comments
    let config_content = render_init_config_content();

    // Write to file
    fs::write(&config_path, config_content)
        .with_context(|| format!("Failed to write config file: {}", config_path.display()))?;

    if let Some(parent) = default_policy_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create policy directory: {}", parent.display()))?;
    }
    fs::write(&default_policy_path, render_default_policy_content()).with_context(|| {
        format!(
            "Failed to write default policy file: {}",
            default_policy_path.display()
        )
    })?;

    println!("✓ Config file created at: {}", config_path.display());
    println!(
        "✓ Default shell policy created at: {}",
        default_policy_path.display()
    );
    println!("\nNext steps:");
    println!("1. Add your OpenRouter API key to the config file");
    println!("   Or set the OPENROUTER_API_KEY environment variable");
    println!("2. Customize the default_model if desired");
    println!("3. If you have a light terminal, change theme to \"InspiredGitHub\"");
    println!("4. Add custom models with system prompts if needed");

    Ok(())
}
