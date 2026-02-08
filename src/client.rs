use crate::config::Config;
use anyhow::{Context, Result};
use openrouter_rs::OpenRouterClient;

/// Create OpenRouter API client
///
/// Attempts to get API key from:
/// 1. Environment variable OPENROUTER_API_KEY
/// 2. Config file api_key field
///
/// Returns error if no API key is found.
pub fn create_client(config: &Config) -> Result<OpenRouterClient> {
    // Try environment variable first
    let api_key = std::env::var("OPENROUTER_API_KEY")
        .ok()
        .or_else(|| config.api_key.clone())
        .context(
            "No API key found. Please set OPENROUTER_API_KEY environment variable or add api_key to config file.\n\
             Get your API key from: https://openrouter.ai/keys"
        )?;

    // Build and return client
    OpenRouterClient::builder()
        .api_key(&api_key)
        .x_title("zo")
        .build()
        .context("Failed to create OpenRouter client")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_client_no_api_key() {
        // Clear env var if it exists
        unsafe {
            std::env::remove_var("OPENROUTER_API_KEY");
        }

        let config = Config {
            api_key: None,
            default_model: None,
            models: None,
            custom_models: vec![],
            theme: None,
            inline_colors: None,
            history_file: None,
        };

        let result = create_client(&config);

        // Should return error when no API key
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No API key found"));
    }
}
