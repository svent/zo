use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use openrouter_rs::OpenRouterClient;
use openrouter_rs::api::chat::{ChatCompletionRequest, Message, Modality};
use openrouter_rs::api::discovery::UserModel;
use openrouter_rs::types::Role;
use openrouter_rs::types::completion::{Choice, CompletionsResponse};
use serde_json::Value;

use crate::file_ops::write_binary_file;
use crate::models::ModelEntry;

pub const DEFAULT_IMAGE_MODEL_ID: &str = "google/gemini-2.5-flash-image";

pub struct ImageGenerationOptions {
    pub output_path: String,
    pub accept_writes: bool,
    pub allow_hidden: bool,
    pub non_interactive: bool,
}

pub async fn derive_image_modalities(
    client: &OpenRouterClient,
    model_id: &str,
    using_default_model: bool,
) -> Result<Vec<Modality>> {
    if using_default_model {
        return Ok(vec![Modality::Image, Modality::Text]);
    }

    let user_models = client
        .list_models_for_user()
        .await
        .context("Failed to inspect image model capabilities via OpenRouter /models/user")?;

    derive_image_modalities_from_models(&user_models, model_id)
}

pub fn format_modalities(modalities: &[Modality]) -> String {
    modalities
        .iter()
        .map(|modality| match modality {
            Modality::Text => "text",
            Modality::Image => "image",
            Modality::Audio => "audio",
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub async fn run_image_generation(
    client: OpenRouterClient,
    model_entry: ModelEntry,
    prompt: &str,
    modalities: Vec<Modality>,
    options: ImageGenerationOptions,
) -> Result<()> {
    let request = build_image_request(&model_entry, prompt, modalities)
        .context("Failed to build image generation request")?;

    let response = client
        .send_chat_completion(&request)
        .await
        .context("Failed to send image generation request")?;

    let data_url = extract_generated_image_data_url(&response)?;
    let image_bytes = decode_image_data_url(data_url)?;

    if !write_binary_file(
        &options.output_path,
        &image_bytes,
        options.allow_hidden,
        options.accept_writes,
        options.non_interactive,
    )? {
        println!("Cancelled.");
        return Ok(());
    }

    println!("Saved image to {}", options.output_path);
    Ok(())
}

fn derive_image_modalities_from_models(
    user_models: &[UserModel],
    model_id: &str,
) -> Result<Vec<Modality>> {
    let model = user_models
        .iter()
        .find(|model| model.id == model_id || model.canonical_slug == model_id)
        .with_context(|| {
            format!(
                "Model '{}' was not found in OpenRouter /models/user, so image output could not be verified.",
                model_id
            )
        })?;

    let output_modalities = model
        .architecture
        .output_modalities
        .as_deref()
        .unwrap_or_default();

    let has_image = output_modalities
        .iter()
        .any(|modality| modality.eq_ignore_ascii_case("image"));
    let has_text = output_modalities
        .iter()
        .any(|modality| modality.eq_ignore_ascii_case("text"));

    if !has_image {
        bail!(
            "Model '{}' does not advertise image output. Choose an image-capable model or omit --model to use {}.",
            model_id,
            DEFAULT_IMAGE_MODEL_ID
        );
    }

    if has_text {
        Ok(vec![Modality::Image, Modality::Text])
    } else {
        Ok(vec![Modality::Image])
    }
}

fn build_image_request(
    model_entry: &ModelEntry,
    prompt: &str,
    modalities: Vec<Modality>,
) -> Result<ChatCompletionRequest> {
    let mut messages = Vec::new();

    if let Some(system_prompt) = &model_entry.system_prompt {
        messages.push(Message::new(Role::System, system_prompt.as_str()));
    }

    messages.push(Message::new(Role::User, prompt));

    let mut builder = ChatCompletionRequest::builder();
    builder
        .model(&model_entry.model_id)
        .messages(messages)
        .modalities(modalities);

    builder
        .build()
        .context("request builder rejected image request")
}

fn extract_generated_image_data_url(response: &CompletionsResponse) -> Result<&str> {
    let choice = response
        .choices
        .first()
        .context("Image generation returned no choices")?;

    let images = match choice {
        Choice::NonStreaming(choice) => choice
            .message
            .images
            .as_deref()
            .context("Image generation response did not include any images")?,
        _ => bail!("Image generation returned an unexpected response shape"),
    };

    let image = images
        .first()
        .context("Image generation response did not include any images")?;

    extract_image_url(image).context("Generated image response did not include a usable URL")
}

fn extract_image_url(image: &Value) -> Option<&str> {
    image.get("url").and_then(Value::as_str).or_else(|| {
        image
            .get("image_url")
            .and_then(|image_url| image_url.get("url"))
            .and_then(Value::as_str)
    })
}

fn decode_image_data_url(data_url: &str) -> Result<Vec<u8>> {
    let (metadata, encoded) = data_url
        .split_once(',')
        .context("Generated image URL was malformed: missing data separator")?;
    let metadata = metadata
        .strip_prefix("data:")
        .context("Expected generated image data URL starting with 'data:'")?;

    let mut metadata_parts = metadata.split(';');
    let media_type = metadata_parts
        .next()
        .context("Generated image data URL was missing a media type")?;

    if !media_type.starts_with("image/") {
        bail!(
            "Expected generated image data URL, but received media type '{}'.",
            media_type
        );
    }

    if !metadata_parts.any(|part| part.eq_ignore_ascii_case("base64")) {
        bail!("Generated image data URL was not base64-encoded");
    }

    STANDARD
        .decode(encoded)
        .context("Failed to decode generated image data URL")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user_model(id: &str, output_modalities: Option<&[&str]>) -> UserModel {
        let output_modalities = match output_modalities {
            Some(modalities) => json!(modalities),
            None => Value::Null,
        };

        serde_json::from_value(json!({
            "id": id,
            "canonical_slug": id,
            "hugging_face_id": null,
            "name": "Test model",
            "created": 1710000000,
            "description": "Test model",
            "pricing": {
                "prompt": "0.0",
                "completion": "0.0"
            },
            "context_length": 128000,
            "architecture": {
                "tokenizer": "test",
                "instruct_type": "chatml",
                "modality": "text->image",
                "input_modalities": ["text"],
                "output_modalities": output_modalities
            },
            "top_provider": {
                "context_length": 128000,
                "max_completion_tokens": 8192,
                "is_moderated": false
            },
            "per_request_limits": null,
            "supported_parameters": []
        }))
        .unwrap()
    }

    fn response_with_images(images: Value) -> CompletionsResponse {
        serde_json::from_value(json!({
            "id": "gen-image-1",
            "choices": [{
                "finish_reason": "stop",
                "index": 0,
                "message": {
                    "role": "assistant",
                    "images": images
                }
            }],
            "created": 1700000000,
            "model": "test-model",
            "object": "chat.completion"
        }))
        .unwrap()
    }

    #[test]
    fn test_derive_image_modalities_image_only() {
        let models = vec![user_model("test/image-only", Some(&["image"]))];
        let modalities = derive_image_modalities_from_models(&models, "test/image-only").unwrap();

        assert_eq!(modalities, vec![Modality::Image]);
    }

    #[test]
    fn test_derive_image_modalities_text_and_image() {
        let models = vec![user_model("test/text-image", Some(&["text", "image"]))];
        let modalities = derive_image_modalities_from_models(&models, "test/text-image").unwrap();

        assert_eq!(modalities, vec![Modality::Image, Modality::Text]);
    }

    #[test]
    fn test_derive_image_modalities_rejects_non_image_model() {
        let models = vec![user_model("test/text-only", Some(&["text"]))];
        let error = derive_image_modalities_from_models(&models, "test/text-only").unwrap_err();

        assert!(
            error
                .to_string()
                .contains("does not advertise image output")
        );
    }

    #[test]
    fn test_extract_generated_image_data_url_rejects_zero_images() {
        let response = response_with_images(json!([]));
        let error = extract_generated_image_data_url(&response).unwrap_err();

        assert!(error.to_string().contains("did not include any images"));
    }

    #[test]
    fn test_extract_generated_image_data_url_uses_first_of_multiple_images() {
        let response = response_with_images(json!([
            {"url": "data:image/png;base64,AA=="},
            {"url": "data:image/png;base64,BB=="}
        ]));
        let data_url = extract_generated_image_data_url(&response).unwrap();

        assert_eq!(data_url, "data:image/png;base64,AA==");
    }

    #[test]
    fn test_extract_generated_image_data_url_supports_nested_url() {
        let response = response_with_images(json!([
            {"image_url": {"url": "data:image/png;base64,iVBORw=="}}
        ]));
        let data_url = extract_generated_image_data_url(&response).unwrap();

        assert_eq!(data_url, "data:image/png;base64,iVBORw==");
    }

    #[test]
    fn test_decode_image_data_url_rejects_malformed_data_url() {
        let error = decode_image_data_url("https://example.com/image.png").unwrap_err();
        assert!(error.to_string().contains("missing data separator"));

        let error = decode_image_data_url("data:text/plain;base64,SGVsbG8=").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Expected generated image data URL")
        );
    }

    #[test]
    fn test_decode_image_data_url_rejects_invalid_base64() {
        let error = decode_image_data_url("data:image/png;base64,%%%").unwrap_err();
        assert!(error.to_string().contains("Failed to decode"));
    }

    #[test]
    fn test_decode_image_data_url_png_payload() {
        let expected = vec![0x89, 0x50, 0x4e, 0x47];
        let encoded = STANDARD.encode(&expected);
        let decoded = decode_image_data_url(&format!("data:image/png;base64,{encoded}")).unwrap();

        assert_eq!(decoded, expected);
    }

    #[test]
    fn test_decode_image_data_url_jpeg_payload() {
        let expected = vec![0xff, 0xd8, 0xff, 0xe0];
        let encoded = STANDARD.encode(&expected);
        let decoded = decode_image_data_url(&format!("data:image/jpeg;base64,{encoded}")).unwrap();

        assert_eq!(decoded, expected);
    }
}
