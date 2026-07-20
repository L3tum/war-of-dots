//! Helper to create LLM clients from configuration

use super::clients::LlmClient;

/// Create an LLM client from the config provider
pub fn create_llm_client(
    provider_type: &str,
    model: &str,
    api_key: Option<&str>,
    base_url: Option<&str>,
) -> anyhow::Result<LlmClient> {
    match provider_type {
        "openai" => {
            let api_key =
                api_key.ok_or_else(|| anyhow::anyhow!("OpenAI client requires api_key"))?;
            let base_url = base_url.unwrap_or("https://api.openai.com");
            Ok(LlmClient::new_openai(
                api_key.to_string(),
                model.to_string(),
                base_url.to_string(),
            ))
        }
        "ollama" => {
            let base_url = base_url.unwrap_or("http://localhost:11434");
            Ok(LlmClient::new_ollama(
                base_url.to_string(),
                model.to_string(),
            ))
        }
        "anthropic" => {
            let api_key =
                api_key.ok_or_else(|| anyhow::anyhow!("Anthropic client requires api_key"))?;
            let base_url = base_url.unwrap_or("https://api.anthropic.com");
            Ok(LlmClient::new_anthropic(
                api_key.to_string(),
                model.to_string(),
                base_url.to_string(),
            ))
        }
        _ => anyhow::bail!("Unknown provider type: {}", provider_type),
    }
}
