use crate::config::Config;
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use std::collections::HashMap;

/// Model entry containing model ID and optional system prompt
#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub model_id: String,
    pub system_prompt: Option<String>,
}

pub const DEFAULT_TEXT_MODEL_NAME: &str = "codex";
pub const DEFAULT_TEXT_MODEL_ID: &str = "openai/gpt-5.4";

/// Default models mapping short names to OpenRouter model IDs
pub const DEFAULT_MODELS: &[(&str, &str)] = &[
    (DEFAULT_TEXT_MODEL_NAME, DEFAULT_TEXT_MODEL_ID),
    ("flash", "google/gemini-2.5-flash"),
    ("geminipro", "google/gemini-3-pro-preview"),
    ("gpt4.1", "openai/gpt-4.1"),
    ("gpt4o", "openai/gpt-4o"),
    ("gpt4omini", "openai/gpt-4o-mini"),
    ("gpt5", "openai/gpt-5"),
    ("grok", "x-ai/grok-4"),
    ("haiku", "anthropic/claude-3-haiku"),
    ("o1", "openai/o1"),
    ("opus", "anthropic/claude-opus-4.6"),
    ("sonnet", "anthropic/claude-sonnet-4.5"),
    ("sonnet3", "anthropic/claude-3.5-sonnet"),
];

/// Build model map from config or default models.
///
/// If `config.models` is defined, uses only those models (completely overrides defaults).
/// Otherwise, uses the default models defined in [`DEFAULT_MODELS`].
/// In both cases, custom models from `config.custom_models` are added and can override
/// any model with the same name.
///
/// # Arguments
/// * `config` - The loaded configuration containing model definitions
///
/// # Returns
/// A HashMap mapping model names (short names like "sonnet", "gpt4", etc.) to
/// [`ModelEntry`] structs containing the full model ID and optional system prompt.
pub fn build_model_map(config: &Config) -> HashMap<String, ModelEntry> {
    let mut map = HashMap::new();

    // Use config models if present, otherwise use defaults
    if let Some(config_models) = &config.models {
        // Add config-defined models
        for (name, model_id) in config_models {
            map.insert(
                name.clone(),
                ModelEntry {
                    model_id: model_id.clone(),
                    system_prompt: None,
                },
            );
        }
    } else {
        // Add default models
        for (name, model_id) in DEFAULT_MODELS {
            map.insert(
                name.to_string(),
                ModelEntry {
                    model_id: model_id.to_string(),
                    system_prompt: None,
                },
            );
        }
    }

    // Add custom models from config (these override any model with same name)
    for custom in &config.custom_models {
        map.insert(
            custom.name.clone(),
            ModelEntry {
                model_id: custom.model.clone(),
                system_prompt: custom.system_prompt.clone(),
            },
        );
    }

    map
}

/// Select a model using multi-stage matching.
///
/// Matching stages (in order):
/// 1. **Exact match** (case-insensitive): "sonnet" matches "sonnet"
/// 2. **Substring match**: "pro" matches "geminipro" (input is substring of model name)
/// 3. **Fuzzy match**: "sonn" matches "sonnet" (with score >= 50)
///
/// Custom models from the config are checked before base models at each stage.
/// Base models come from either `config.models` (if defined) or `DEFAULT_MODELS`.
/// This ensures that custom models with system prompts take precedence.
///
/// # Arguments
/// * `input` - The user's input string (e.g., "pro", "sonn", "flash", "gpt")
/// * `model_map` - The map of available models built by [`build_model_map`]
/// * `config` - The configuration (needed to determine base models and custom models)
///
/// # Returns
/// * `Some(ModelEntry)` - If a good match is found
/// * `None` - If no match is found at any stage
///
/// # Examples
/// ```ignore
/// let map = build_model_map(&config);
/// let entry = select_model("pro", &map, &config);    // Matches "geminipro" via substring
/// let entry = select_model("sonn", &map, &config);   // Matches "sonnet" via fuzzy
/// let entry = select_model("sonnet", &map, &config); // Matches "sonnet" via exact
/// ```
pub fn select_model(
    input: &str,
    model_map: &HashMap<String, ModelEntry>,
    config: &Config,
) -> Option<ModelEntry> {
    let input_lower = input.to_lowercase();

    // Helper to check a single model against input at a specific stage
    let check_exact = |key: &str| -> bool { key.to_lowercase() == input_lower };

    let check_substring = |key: &str| -> bool { key.to_lowercase().contains(&input_lower) };

    // Stage 1: Try exact match (case-insensitive)
    // Check custom models first
    for custom in &config.custom_models {
        if check_exact(&custom.name) {
            if let Some(entry) = model_map.get(&custom.name) {
                return Some(entry.clone());
            }
        }
    }
    // Then check base models (config.models or DEFAULT_MODELS)
    if let Some(config_models) = &config.models {
        for name in config_models.keys() {
            if check_exact(name) {
                if let Some(entry) = model_map.get(name) {
                    return Some(entry.clone());
                }
            }
        }
    } else {
        for (name, _) in DEFAULT_MODELS {
            if check_exact(name) {
                if let Some(entry) = model_map.get(*name) {
                    return Some(entry.clone());
                }
            }
        }
    }

    // Stage 2: Try substring match (input is contained in model name)
    // Check custom models first
    for custom in &config.custom_models {
        if check_substring(&custom.name) {
            if let Some(entry) = model_map.get(&custom.name) {
                return Some(entry.clone());
            }
        }
    }
    // Then check base models (config.models or DEFAULT_MODELS)
    if let Some(config_models) = &config.models {
        for name in config_models.keys() {
            if check_substring(name) {
                if let Some(entry) = model_map.get(name) {
                    return Some(entry.clone());
                }
            }
        }
    } else {
        for (name, _) in DEFAULT_MODELS {
            if check_substring(name) {
                if let Some(entry) = model_map.get(*name) {
                    return Some(entry.clone());
                }
            }
        }
    }

    // Stage 3: Try fuzzy matching - find best match across all models
    let matcher = SkimMatcherV2::default();
    let mut best_custom_match: Option<(&ModelEntry, i64)> = None;
    let mut best_base_match: Option<(&ModelEntry, i64)> = None;

    // Check custom models
    for custom in &config.custom_models {
        if let Some(entry) = model_map.get(&custom.name) {
            if let Some(score) = matcher.fuzzy_match(&custom.name, input) {
                if let Some((_, best_score)) = best_custom_match {
                    if score > best_score {
                        best_custom_match = Some((entry, score));
                    }
                } else {
                    best_custom_match = Some((entry, score));
                }
            }
        }
    }

    // Check base models (config.models or DEFAULT_MODELS)
    if let Some(config_models) = &config.models {
        for name in config_models.keys() {
            if let Some(entry) = model_map.get(name) {
                if let Some(score) = matcher.fuzzy_match(name, input) {
                    if let Some((_, best_score)) = best_base_match {
                        if score > best_score {
                            best_base_match = Some((entry, score));
                        }
                    } else {
                        best_base_match = Some((entry, score));
                    }
                }
            }
        }
    } else {
        for (name, _) in DEFAULT_MODELS {
            if let Some(entry) = model_map.get(*name) {
                if let Some(score) = matcher.fuzzy_match(name, input) {
                    if let Some((_, best_score)) = best_base_match {
                        if score > best_score {
                            best_base_match = Some((entry, score));
                        }
                    } else {
                        best_base_match = Some((entry, score));
                    }
                }
            }
        }
    }

    // Lowered threshold from 60 to 50 to catch more valid partial matches
    const MIN_SCORE: i64 = 50;

    // Prefer custom model match over base model match
    if let Some((entry, score)) = best_custom_match {
        if score >= MIN_SCORE {
            return Some(entry.clone());
        }
    }

    if let Some((entry, score)) = best_base_match {
        if score >= MIN_SCORE {
            return Some(entry.clone());
        }
    }

    None
}

/// Get top N fuzzy matches for a given input.
///
/// Returns a list of (model_name, model_id, score) tuples sorted by score (descending).
/// Useful for providing "Did you mean?" suggestions when a model is not found.
///
/// # Arguments
/// * `input` - The user's input string
/// * `model_map` - The map of available models
/// * `limit` - Maximum number of matches to return
///
/// # Returns
/// A vector of tuples containing (model_name, model_id, fuzzy_match_score),
/// sorted by score in descending order.
pub fn get_fuzzy_matches(
    input: &str,
    model_map: &HashMap<String, ModelEntry>,
    limit: usize,
) -> Vec<(String, String, i64)> {
    let matcher = SkimMatcherV2::default();
    let mut matches: Vec<(String, String, i64)> = Vec::new();

    for (key, entry) in model_map.iter() {
        if let Some(score) = matcher.fuzzy_match(key, input) {
            matches.push((key.clone(), entry.model_id.clone(), score));
        }
    }

    // Sort by score (descending)
    matches.sort_by(|a, b| b.2.cmp(&a.2));

    // Return top N matches
    matches.into_iter().take(limit).collect()
}

/// Get a list of all available model names.
///
/// Returns a sorted vector of all model names (keys) in the model map.
/// Useful for displaying available models to the user.
///
/// # Arguments
/// * `model_map` - The map of available models
///
/// # Returns
/// A sorted vector of model names (short names like "sonnet", "gpt4", etc.)
pub fn list_model_names(model_map: &HashMap<String, ModelEntry>) -> Vec<String> {
    let mut names: Vec<String> = model_map.keys().cloned().collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, CustomModel};

    fn create_test_config() -> Config {
        Config {
            api_key: None,
            default_model: Some("google/gemini-2.0-flash-exp".to_string()),
            models: None,
            custom_models: vec![CustomModel {
                name: "mymodel".to_string(),
                model: "anthropic/claude-3.5-sonnet".to_string(),
                system_prompt: Some("You are a helpful assistant".to_string()),
            }],
            theme: None,
            inline_colors: None,
            history_file: None,
        }
    }

    #[test]
    fn test_build_model_map_default_models() {
        let config = Config {
            api_key: None,
            default_model: None,
            models: None,
            custom_models: vec![],
            theme: None,
            inline_colors: None,
            history_file: None,
        };

        let model_map = build_model_map(&config);

        // Check that default models are present
        assert!(model_map.contains_key(DEFAULT_TEXT_MODEL_NAME));
        assert!(model_map.contains_key("sonnet"));
        assert!(model_map.contains_key("flash"));
        assert!(model_map.contains_key("gpt4o"));

        // Verify model IDs
        assert_eq!(
            model_map.get(DEFAULT_TEXT_MODEL_NAME).unwrap().model_id,
            DEFAULT_TEXT_MODEL_ID
        );
        assert_eq!(
            model_map.get("sonnet").unwrap().model_id,
            "anthropic/claude-sonnet-4.5"
        );
        assert_eq!(
            model_map.get("flash").unwrap().model_id,
            "google/gemini-2.5-flash"
        );
        assert_eq!(
            model_map.get("geminipro").unwrap().model_id,
            "google/gemini-3-pro-preview"
        );
        assert_eq!(model_map.get("grok").unwrap().model_id, "x-ai/grok-4");
    }

    #[test]
    fn test_build_model_map_custom_models() {
        let config = create_test_config();
        let model_map = build_model_map(&config);

        // Check that custom model is present
        assert!(model_map.contains_key("mymodel"));

        let custom = model_map.get("mymodel").unwrap();
        assert_eq!(custom.model_id, "anthropic/claude-3.5-sonnet");
        assert_eq!(
            custom.system_prompt,
            Some("You are a helpful assistant".to_string())
        );
    }

    #[test]
    fn test_custom_model_overrides_default() {
        let config = Config {
            api_key: None,
            default_model: None,
            models: None,
            custom_models: vec![CustomModel {
                name: "sonnet".to_string(),
                model: "custom/model-id".to_string(),
                system_prompt: Some("Custom prompt".to_string()),
            }],
            theme: None,
            inline_colors: None,
            history_file: None,
        };

        let model_map = build_model_map(&config);
        let sonnet = model_map.get("sonnet").unwrap();

        // Custom model should override default
        assert_eq!(sonnet.model_id, "custom/model-id");
        assert_eq!(sonnet.system_prompt, Some("Custom prompt".to_string()));
    }

    #[test]
    fn test_select_model_exact_match() {
        let config = Config {
            api_key: None,
            default_model: None,
            models: None,
            custom_models: vec![],
            theme: None,
            inline_colors: None,
            history_file: None,
        };
        let model_map = build_model_map(&config);

        // Test exact match
        let result = select_model("sonnet", &model_map, &config);
        assert!(result.is_some());
        assert_eq!(result.unwrap().model_id, "anthropic/claude-sonnet-4.5");
    }

    #[test]
    fn test_select_model_fuzzy_match() {
        let config = Config {
            api_key: None,
            default_model: None,
            models: None,
            custom_models: vec![],
            theme: None,
            inline_colors: None,
            history_file: None,
        };
        let model_map = build_model_map(&config);

        // Test fuzzy matching
        let result = select_model("son", &model_map, &config);
        assert!(result.is_some());
        assert!(result.unwrap().model_id.contains("sonnet"));

        let result2 = select_model("gem", &model_map, &config);
        assert!(result2.is_some());
        // Should match one of the gemini models
        assert!(result2.unwrap().model_id.contains("gemini"));
    }

    #[test]
    fn test_select_model_case_insensitive() {
        let config = Config {
            api_key: None,
            default_model: None,
            models: None,
            custom_models: vec![],
            theme: None,
            inline_colors: None,
            history_file: None,
        };
        let model_map = build_model_map(&config);

        // Test case insensitive exact match
        let result = select_model("SONNET", &model_map, &config);
        assert!(result.is_some());
        assert_eq!(result.unwrap().model_id, "anthropic/claude-sonnet-4.5");

        let result2 = select_model("SoNnEt", &model_map, &config);
        assert!(result2.is_some());
        assert_eq!(result2.unwrap().model_id, "anthropic/claude-sonnet-4.5");
    }

    #[test]
    fn test_select_model_no_match() {
        let config = Config {
            api_key: None,
            default_model: None,
            models: None,
            custom_models: vec![],
            theme: None,
            inline_colors: None,
            history_file: None,
        };
        let model_map = build_model_map(&config);

        // Test with input that shouldn't match anything
        let result = select_model("xyzabc123", &model_map, &config);
        assert!(result.is_none());
    }

    #[test]
    fn test_get_fuzzy_matches() {
        let config = Config {
            api_key: None,
            default_model: None,
            models: None,
            custom_models: vec![],
            theme: None,
            inline_colors: None,
            history_file: None,
        };
        let model_map = build_model_map(&config);

        let matches = get_fuzzy_matches("gpt", &model_map, 3);

        // Should return top 3 matches
        assert!(matches.len() <= 3);
        assert!(matches.len() > 0);

        // Results should be sorted by score (descending)
        for i in 1..matches.len() {
            assert!(matches[i - 1].2 >= matches[i].2);
        }
    }

    #[test]
    fn test_list_model_names() {
        let config = create_test_config();
        let model_map = build_model_map(&config);

        let names = list_model_names(&model_map);

        // Should be sorted
        assert!(names.contains(&"sonnet".to_string()));
        assert!(names.contains(&"mymodel".to_string()));

        // Verify sorting
        for i in 1..names.len() {
            assert!(names[i - 1] <= names[i]);
        }
    }

    #[test]
    fn test_select_model_partial_match_suffix() {
        let config = Config {
            api_key: None,
            default_model: None,
            models: None,
            custom_models: vec![],
            theme: None,
            inline_colors: None,
            history_file: None,
        };
        let model_map = build_model_map(&config);

        // Test partial match with suffix - "pro" should match "geminipro"
        let result = select_model("pro", &model_map, &config);
        assert!(result.is_some());
        assert_eq!(result.unwrap().model_id, "google/gemini-3-pro-preview");
    }

    #[test]
    fn test_select_model_partial_match_prefix() {
        let config = Config {
            api_key: None,
            default_model: None,
            models: None,
            custom_models: vec![],
            theme: None,
            inline_colors: None,
            history_file: None,
        };
        let model_map = build_model_map(&config);

        // Test partial match with prefix - "gem" should match "geminipro"
        let result = select_model("gem", &model_map, &config);
        assert!(result.is_some());
        assert!(result.unwrap().model_id.contains("gemini"));
    }

    #[test]
    fn test_select_model_partial_match_middle() {
        let config = Config {
            api_key: None,
            default_model: None,
            models: None,
            custom_models: vec![],
            theme: None,
            inline_colors: None,
            history_file: None,
        };
        let model_map = build_model_map(&config);

        // Test partial match in middle - "mini" should match "gpt4omini" or "o1mini"
        let result = select_model("mini", &model_map, &config);
        assert!(result.is_some());
        let model_id = result.unwrap().model_id;
        assert!(model_id.contains("mini"));
    }

    #[test]
    fn test_select_model_custom_takes_precedence() {
        let config = Config {
            api_key: None,
            default_model: None,
            models: None,
            custom_models: vec![CustomModel {
                name: "mypro".to_string(),
                model: "custom/my-model".to_string(),
                system_prompt: Some("Custom prompt".to_string()),
            }],
            theme: None,
            inline_colors: None,
            history_file: None,
        };
        let model_map = build_model_map(&config);

        // Test that custom model "mypro" is matched before built-in "geminipro"
        // when searching for "pro"
        let result = select_model("pro", &model_map, &config);
        assert!(result.is_some());
        // Should match "mypro" (custom) not "geminipro" (built-in)
        assert_eq!(result.unwrap().model_id, "custom/my-model");
    }

    #[test]
    fn test_select_model_multiple_partial_matches() {
        let config = Config {
            api_key: None,
            default_model: None,
            models: None,
            custom_models: vec![],
            theme: None,
            inline_colors: None,
            history_file: None,
        };
        let model_map = build_model_map(&config);

        // "gpt" should match one of: gpt4, gpt4o, gpt4omini
        // It should return the first match found (order depends on custom vs default)
        let result = select_model("gpt", &model_map, &config);
        assert!(result.is_some());
        assert!(result.unwrap().model_id.contains("gpt"));
    }

    #[test]
    fn test_config_models_overrides_defaults() {
        use std::collections::HashMap;

        let mut custom_models = HashMap::new();
        custom_models.insert(
            "flash".to_string(),
            "my-provider/my-flash-model".to_string(),
        );
        custom_models.insert(
            "sonnet".to_string(),
            "my-provider/my-sonnet-model".to_string(),
        );

        let config = Config {
            api_key: None,
            default_model: None,
            models: Some(custom_models),
            custom_models: vec![],
            theme: None,
            inline_colors: None,
            history_file: None,
        };

        let model_map = build_model_map(&config);

        // Should only contain the 2 models from config.models
        assert_eq!(model_map.len(), 2);
        assert!(model_map.contains_key("flash"));
        assert!(model_map.contains_key("sonnet"));

        // Should NOT contain default models
        assert!(!model_map.contains_key("gpt4o"));
        assert!(!model_map.contains_key("geminipro"));

        // Verify the custom model IDs
        assert_eq!(
            model_map.get("flash").unwrap().model_id,
            "my-provider/my-flash-model"
        );
        assert_eq!(
            model_map.get("sonnet").unwrap().model_id,
            "my-provider/my-sonnet-model"
        );
    }

    #[test]
    fn test_config_models_with_custom_models() {
        use std::collections::HashMap;

        let mut base_models = HashMap::new();
        base_models.insert(
            "flash".to_string(),
            "my-provider/my-flash-model".to_string(),
        );

        let config = Config {
            api_key: None,
            default_model: None,
            models: Some(base_models),
            custom_models: vec![CustomModel {
                name: "coder".to_string(),
                model: "anthropic/claude-3.5-sonnet".to_string(),
                system_prompt: Some("You are a coding assistant".to_string()),
            }],
            theme: None,
            inline_colors: None,
            history_file: None,
        };

        let model_map = build_model_map(&config);

        // Should contain both config.models and custom_models
        assert_eq!(model_map.len(), 2);
        assert!(model_map.contains_key("flash"));
        assert!(model_map.contains_key("coder"));

        // Verify custom model has system prompt
        let coder = model_map.get("coder").unwrap();
        assert_eq!(coder.model_id, "anthropic/claude-3.5-sonnet");
        assert_eq!(
            coder.system_prompt,
            Some("You are a coding assistant".to_string())
        );
    }
}
