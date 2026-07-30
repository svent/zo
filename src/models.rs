use crate::config::Config;
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use std::collections::HashMap;
use std::error::Error;
use std::fmt;

/// Model entry containing model ID and optional system prompt
#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub model_id: String,
    pub system_prompt: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelMatchKind {
    ExactAlias,
    DirectId,
    SubstringAlias,
    FuzzyAlias,
}

#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub canonical_alias: Option<String>,
    pub entry: ModelEntry,
    pub match_kind: ModelMatchKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelSelectionError {
    NotFound(String),
    Ambiguous { input: String, aliases: Vec<String> },
}

impl fmt::Display for ModelSelectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(input) => write!(f, "Model '{}' not found", input),
            Self::Ambiguous { input, aliases } => write!(
                f,
                "Model '{}' is ambiguous; matching aliases: {}",
                input,
                aliases.join(", ")
            ),
        }
    }
}

impl Error for ModelSelectionError {}

pub const DEFAULT_TEXT_MODEL_NAME: &str = "codex";
pub const DEFAULT_TEXT_MODEL_ID: &str = "openai/gpt-5.4";

/// Default models mapping short names to OpenRouter model IDs
pub const DEFAULT_MODELS: &[(&str, &str)] = &[
    (DEFAULT_TEXT_MODEL_NAME, DEFAULT_TEXT_MODEL_ID),
    ("flash", "google/gemini-3-flash-preview"),
    ("geminipro", "google/gemini-pro-latest"),
    ("gpt4.1", "openai/gpt-4.1"),
    ("gpt4o", "openai/gpt-4o"),
    ("gpt4omini", "openai/gpt-4o-mini"),
    ("grok", "x-ai/grok-4.3"),
    ("haiku", "anthropic/claude-3-haiku"),
    ("o1", "openai/o1"),
    ("opus", "anthropic/claude-opus-4.6"),
    ("sonnet", "anthropic/claude-sonnet-4.5"),
    ("sonnet3", "anthropic/claude-3.5-sonnet"),
    // image models
    ("banana", "google/gemini-3-pro-image-preview"),
];

struct ResolvedBaseModels {
    builtins: Vec<(String, ModelEntry)>,
    extras: Vec<(String, ModelEntry)>,
}

fn is_builtin_model_name(name: &str) -> bool {
    DEFAULT_MODELS
        .iter()
        .any(|(default_name, _)| *default_name == name)
}

fn is_disabled_builtin_alias(config: &Config, input: &str) -> bool {
    DEFAULT_MODELS.iter().any(|(name, _)| {
        name.eq_ignore_ascii_case(input)
            && config
                .models
                .as_ref()
                .and_then(|models| models.get(*name))
                .is_some_and(|model_id| model_id.trim().is_empty())
    })
}

fn resolve_base_models(config: &Config) -> ResolvedBaseModels {
    let mut builtins = Vec::new();

    for (name, default_model_id) in DEFAULT_MODELS {
        match config.models.as_ref().and_then(|models| models.get(*name)) {
            Some(model_id) if model_id.trim().is_empty() => {}
            Some(model_id) => builtins.push((
                name.to_string(),
                ModelEntry {
                    model_id: model_id.clone(),
                    system_prompt: None,
                },
            )),
            None => builtins.push((
                name.to_string(),
                ModelEntry {
                    model_id: default_model_id.to_string(),
                    system_prompt: None,
                },
            )),
        }
    }

    let mut extras = Vec::new();
    if let Some(config_models) = &config.models {
        let mut extra_names: Vec<&String> = config_models
            .keys()
            .filter(|name| !is_builtin_model_name(name))
            .collect();
        extra_names.sort();

        for name in extra_names {
            let model_id = config_models.get(name).unwrap();
            if model_id.trim().is_empty() {
                continue;
            }

            extras.push((
                name.clone(),
                ModelEntry {
                    model_id: model_id.clone(),
                    system_prompt: None,
                },
            ));
        }
    }

    ResolvedBaseModels { builtins, extras }
}

/// Build model map from built-in models, `[models]` config overrides, and custom models.
///
/// Built-in aliases remain available by default. Entries in `[models]` can:
/// - override a built-in alias by reusing the same name with a non-empty model ID
/// - disable a built-in alias by assigning an empty string
/// - add extra aliases with new names
///
/// `custom_models` are applied last and can override any alias with the same name.
///
/// # Arguments
/// * `config` - The loaded configuration containing model definitions
///
/// # Returns
/// A HashMap mapping model names (short names like "sonnet", "gpt4", etc.) to
/// [`ModelEntry`] structs containing the full model ID and optional system prompt.
pub fn build_model_map(config: &Config) -> HashMap<String, ModelEntry> {
    let mut map = HashMap::new();
    let base_models = resolve_base_models(config);

    for (name, entry) in base_models.builtins.iter().chain(base_models.extras.iter()) {
        map.insert(name.clone(), entry.clone());
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
/// Among non-custom aliases, built-in aliases are checked before extra aliases added
/// via `[models]`. This keeps built-ins available unless they are explicitly disabled.
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
pub fn resolve_model(
    input: &str,
    model_map: &HashMap<String, ModelEntry>,
    config: &Config,
) -> Result<ResolvedModel, ModelSelectionError> {
    if is_disabled_builtin_alias(config, input) {
        return Err(ModelSelectionError::NotFound(input.to_string()));
    }

    let base_models = resolve_base_models(config);
    let input_lower = input.to_lowercase();

    // Helper to check a single model against input at a specific stage
    let check_exact = |key: &str| -> bool { key.to_lowercase() == input_lower };

    let check_substring = |key: &str| -> bool { key.to_lowercase().contains(&input_lower) };

    // Stage 1: Try exact match (case-insensitive)
    // Check custom models first
    for custom in &config.custom_models {
        if check_exact(&custom.name)
            && let Some(entry) = model_map.get(&custom.name)
        {
            return Ok(ResolvedModel {
                canonical_alias: Some(custom.name.clone()),
                entry: entry.clone(),
                match_kind: ModelMatchKind::ExactAlias,
            });
        }
    }
    // Then check built-in aliases and extra aliases from [models]
    for (name, _) in base_models.builtins.iter().chain(base_models.extras.iter()) {
        if check_exact(name)
            && let Some(entry) = model_map.get(name)
        {
            return Ok(ResolvedModel {
                canonical_alias: Some(name.clone()),
                entry: entry.clone(),
                match_kind: ModelMatchKind::ExactAlias,
            });
        }
    }

    if is_direct_model_id(input) {
        return Ok(ResolvedModel {
            canonical_alias: None,
            entry: ModelEntry {
                model_id: input.to_string(),
                system_prompt: None,
            },
            match_kind: ModelMatchKind::DirectId,
        });
    }

    // Stage 2: Try substring match (input is contained in model name)
    // Check custom models first
    for custom in &config.custom_models {
        if check_substring(&custom.name)
            && let Some(entry) = model_map.get(&custom.name)
        {
            return Ok(ResolvedModel {
                canonical_alias: Some(custom.name.clone()),
                entry: entry.clone(),
                match_kind: ModelMatchKind::SubstringAlias,
            });
        }
    }
    for (name, _) in &base_models.builtins {
        if check_substring(name)
            && let Some(entry) = model_map.get(name)
        {
            return Ok(ResolvedModel {
                canonical_alias: Some(name.clone()),
                entry: entry.clone(),
                match_kind: ModelMatchKind::SubstringAlias,
            });
        }
    }
    for (name, _) in &base_models.extras {
        if check_substring(name)
            && let Some(entry) = model_map.get(name)
        {
            return Ok(ResolvedModel {
                canonical_alias: Some(name.clone()),
                entry: entry.clone(),
                match_kind: ModelMatchKind::SubstringAlias,
            });
        }
    }

    // Stage 3: Try fuzzy matching - find best match across all models
    let matcher = SkimMatcherV2::default();
    const MIN_SCORE: i64 = 50;

    let custom_names: Vec<String> = config
        .custom_models
        .iter()
        .map(|m| m.name.clone())
        .collect();
    let builtin_names: Vec<String> = base_models
        .builtins
        .iter()
        .map(|(n, _)| n.clone())
        .collect();
    let extra_names: Vec<String> = base_models.extras.iter().map(|(n, _)| n.clone()).collect();

    for names in [&custom_names, &builtin_names, &extra_names] {
        let mut scored: Vec<(String, i64)> = names
            .iter()
            .filter_map(|name| {
                matcher
                    .fuzzy_match(name, input)
                    .map(|score| (name.clone(), score))
            })
            .collect();
        scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        let Some((_, best_score)) = scored.first() else {
            continue;
        };
        if *best_score < MIN_SCORE {
            continue;
        }

        let tied: Vec<String> = scored
            .iter()
            .take_while(|(_, score)| score == best_score)
            .map(|(name, _)| name.clone())
            .collect();
        if tied.len() > 1 {
            return Err(ModelSelectionError::Ambiguous {
                input: input.to_string(),
                aliases: tied,
            });
        }

        let name = &scored[0].0;
        if let Some(entry) = model_map.get(name) {
            return Ok(ResolvedModel {
                canonical_alias: Some(name.clone()),
                entry: entry.clone(),
                match_kind: ModelMatchKind::FuzzyAlias,
            });
        }
    }

    Err(ModelSelectionError::NotFound(input.to_string()))
}

fn is_direct_model_id(input: &str) -> bool {
    let Some((provider, model)) = input.split_once('/') else {
        return false;
    };
    !provider.is_empty()
        && !model.is_empty()
        && !model.contains('/')
        && !input.chars().any(char::is_whitespace)
}

#[cfg(test)]
pub fn select_model(
    input: &str,
    model_map: &HashMap<String, ModelEntry>,
    config: &Config,
) -> Option<ModelEntry> {
    resolve_model(input, model_map, config)
        .ok()
        .map(|resolved| resolved.entry)
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

    // Sort by score (descending), then name (ascending) for stable output
    matches.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));

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
    use crate::config::{Config, CustomModel, ShellConfig};

    fn create_test_config() -> Config {
        Config {
            api_key: None,
            default_model: Some("google/gemini-2.0-flash-exp".to_string()),
            web: false,
            models: None,
            custom_models: vec![CustomModel {
                name: "mymodel".to_string(),
                model: "anthropic/claude-3.5-sonnet".to_string(),
                system_prompt: Some("You are a helpful assistant".to_string()),
            }],
            theme: None,
            inline_colors: None,
            history_file: None,
            shell: ShellConfig::default(),
            limits: crate::config::LimitsConfig::default(),
        }
    }

    #[test]
    fn test_build_model_map_default_models() {
        let config = Config {
            api_key: None,
            default_model: None,
            web: false,
            models: None,
            custom_models: vec![],
            theme: None,
            inline_colors: None,
            history_file: None,
            shell: ShellConfig::default(),
            limits: crate::config::LimitsConfig::default(),
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
            "google/gemini-3-flash-preview"
        );
        assert_eq!(
            model_map.get("geminipro").unwrap().model_id,
            "google/gemini-pro-latest"
        );
        assert_eq!(model_map.get("grok").unwrap().model_id, "x-ai/grok-4.3");
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
            web: false,
            models: None,
            custom_models: vec![CustomModel {
                name: "sonnet".to_string(),
                model: "custom/model-id".to_string(),
                system_prompt: Some("Custom prompt".to_string()),
            }],
            theme: None,
            inline_colors: None,
            history_file: None,
            shell: ShellConfig::default(),
            limits: crate::config::LimitsConfig::default(),
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
            web: false,
            models: None,
            custom_models: vec![],
            theme: None,
            inline_colors: None,
            history_file: None,
            shell: ShellConfig::default(),
            limits: crate::config::LimitsConfig::default(),
        };
        let model_map = build_model_map(&config);

        // Test exact match
        let result = select_model("sonnet", &model_map, &config);
        assert!(result.is_some());
        assert_eq!(result.unwrap().model_id, "anthropic/claude-sonnet-4.5");
    }

    #[test]
    fn test_resolve_model_returns_canonical_alias() {
        let config = create_test_config();
        let model_map = build_model_map(&config);
        let resolved = resolve_model("sonn", &model_map, &config).unwrap();
        assert_eq!(resolved.canonical_alias.as_deref(), Some("sonnet"));
        assert_eq!(resolved.match_kind, ModelMatchKind::SubstringAlias);
    }

    #[test]
    fn test_resolve_model_accepts_direct_provider_id() {
        let config = create_test_config();
        let model_map = build_model_map(&config);
        let resolved = resolve_model("provider/model", &model_map, &config).unwrap();
        assert_eq!(resolved.canonical_alias, None);
        assert_eq!(resolved.entry.model_id, "provider/model");
        assert_eq!(resolved.match_kind, ModelMatchKind::DirectId);
    }

    #[test]
    fn test_select_model_fuzzy_match() {
        let config = Config {
            api_key: None,
            default_model: None,
            web: false,
            models: None,
            custom_models: vec![],
            theme: None,
            inline_colors: None,
            history_file: None,
            shell: ShellConfig::default(),
            limits: crate::config::LimitsConfig::default(),
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
            web: false,
            models: None,
            custom_models: vec![],
            theme: None,
            inline_colors: None,
            history_file: None,
            shell: ShellConfig::default(),
            limits: crate::config::LimitsConfig::default(),
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
            web: false,
            models: None,
            custom_models: vec![],
            theme: None,
            inline_colors: None,
            history_file: None,
            shell: ShellConfig::default(),
            limits: crate::config::LimitsConfig::default(),
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
            web: false,
            models: None,
            custom_models: vec![],
            theme: None,
            inline_colors: None,
            history_file: None,
            shell: ShellConfig::default(),
            limits: crate::config::LimitsConfig::default(),
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
            web: false,
            models: None,
            custom_models: vec![],
            theme: None,
            inline_colors: None,
            history_file: None,
            shell: ShellConfig::default(),
            limits: crate::config::LimitsConfig::default(),
        };
        let model_map = build_model_map(&config);

        // Test partial match with suffix - "pro" should match "geminipro"
        let result = select_model("pro", &model_map, &config);
        assert!(result.is_some());
        assert_eq!(result.unwrap().model_id, "google/gemini-pro-latest");
    }

    #[test]
    fn test_select_model_partial_match_prefix() {
        let config = Config {
            api_key: None,
            default_model: None,
            web: false,
            models: None,
            custom_models: vec![],
            theme: None,
            inline_colors: None,
            history_file: None,
            shell: ShellConfig::default(),
            limits: crate::config::LimitsConfig::default(),
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
            web: false,
            models: None,
            custom_models: vec![],
            theme: None,
            inline_colors: None,
            history_file: None,
            shell: ShellConfig::default(),
            limits: crate::config::LimitsConfig::default(),
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
            web: false,
            models: None,
            custom_models: vec![CustomModel {
                name: "mypro".to_string(),
                model: "custom/my-model".to_string(),
                system_prompt: Some("Custom prompt".to_string()),
            }],
            theme: None,
            inline_colors: None,
            history_file: None,
            shell: ShellConfig::default(),
            limits: crate::config::LimitsConfig::default(),
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
            web: false,
            models: None,
            custom_models: vec![],
            theme: None,
            inline_colors: None,
            history_file: None,
            shell: ShellConfig::default(),
            limits: crate::config::LimitsConfig::default(),
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
        custom_models.insert(
            "myalias".to_string(),
            "my-provider/my-extra-model".to_string(),
        );

        let config = Config {
            api_key: None,
            default_model: None,
            web: false,
            models: Some(custom_models),
            custom_models: vec![],
            theme: None,
            inline_colors: None,
            history_file: None,
            shell: ShellConfig::default(),
            limits: crate::config::LimitsConfig::default(),
        };

        let model_map = build_model_map(&config);

        // Built-ins stay available, existing aliases can be overridden, and new aliases can be added
        assert_eq!(model_map.len(), DEFAULT_MODELS.len() + 1);
        assert!(model_map.contains_key("flash"));
        assert!(model_map.contains_key("sonnet"));
        assert!(model_map.contains_key("myalias"));

        // Other default models remain available
        assert!(model_map.contains_key("gpt4o"));
        assert!(model_map.contains_key("geminipro"));

        // Verify the custom model IDs
        assert_eq!(
            model_map.get("flash").unwrap().model_id,
            "my-provider/my-flash-model"
        );
        assert_eq!(
            model_map.get("sonnet").unwrap().model_id,
            "my-provider/my-sonnet-model"
        );
        assert_eq!(
            model_map.get("myalias").unwrap().model_id,
            "my-provider/my-extra-model"
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
            web: false,
            models: Some(base_models),
            custom_models: vec![CustomModel {
                name: "coder".to_string(),
                model: "anthropic/claude-3.5-sonnet".to_string(),
                system_prompt: Some("You are a coding assistant".to_string()),
            }],
            theme: None,
            inline_colors: None,
            history_file: None,
            shell: ShellConfig::default(),
            limits: crate::config::LimitsConfig::default(),
        };

        let model_map = build_model_map(&config);

        // Built-ins remain, flash is overridden, and the custom model is added
        assert_eq!(model_map.len(), DEFAULT_MODELS.len() + 1);
        assert!(model_map.contains_key("flash"));
        assert!(model_map.contains_key("coder"));
        assert!(model_map.contains_key("sonnet"));

        // Verify custom model has system prompt
        let coder = model_map.get("coder").unwrap();
        assert_eq!(coder.model_id, "anthropic/claude-3.5-sonnet");
        assert_eq!(
            coder.system_prompt,
            Some("You are a coding assistant".to_string())
        );
    }

    #[test]
    fn test_config_models_can_disable_builtin_alias() {
        use std::collections::HashMap;

        let mut config_models = HashMap::new();
        config_models.insert("sonnet".to_string(), "".to_string());

        let config = Config {
            api_key: None,
            default_model: None,
            web: false,
            models: Some(config_models),
            custom_models: vec![],
            theme: None,
            inline_colors: None,
            history_file: None,
            shell: ShellConfig::default(),
            limits: crate::config::LimitsConfig::default(),
        };

        let model_map = build_model_map(&config);

        assert!(!model_map.contains_key("sonnet"));
        assert!(model_map.contains_key("flash"));
        assert_eq!(model_map.len(), DEFAULT_MODELS.len() - 1);
        assert!(select_model("sonnet", &model_map, &config).is_none());
    }

    #[test]
    fn test_select_model_builtin_substring_takes_precedence_over_added_alias() {
        use std::collections::HashMap;

        let mut config_models = HashMap::new();
        config_models.insert("mypro".to_string(), "custom/my-pro-model".to_string());

        let config = Config {
            api_key: None,
            default_model: None,
            web: false,
            models: Some(config_models),
            custom_models: vec![],
            theme: None,
            inline_colors: None,
            history_file: None,
            shell: ShellConfig::default(),
            limits: crate::config::LimitsConfig::default(),
        };

        let model_map = build_model_map(&config);
        let result = select_model("pro", &model_map, &config).unwrap();

        assert_eq!(result.model_id, "google/gemini-pro-latest");
    }

    #[test]
    fn test_select_model_reports_builtin_fuzzy_tie() {
        use std::collections::HashMap;

        let mut config_models = HashMap::new();
        config_models.insert("sxonnet".to_string(), "custom/my-sonnet-model".to_string());

        let config = Config {
            api_key: None,
            default_model: None,
            web: false,
            models: Some(config_models),
            custom_models: vec![],
            theme: None,
            inline_colors: None,
            history_file: None,
            shell: ShellConfig::default(),
            limits: crate::config::LimitsConfig::default(),
        };

        let model_map = build_model_map(&config);
        let matcher = SkimMatcherV2::default();
        assert!(matcher.fuzzy_match("sonnet", "sonet").is_some());
        assert!(matcher.fuzzy_match("sxonnet", "sonet").is_some());

        let error = resolve_model("sonet", &model_map, &config).unwrap_err();
        assert!(matches!(error, ModelSelectionError::Ambiguous { .. }));
    }
}
