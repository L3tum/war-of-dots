//! Enum-based LLM client for dyn compatibility

use super::agent::{GameState, Tool, ToolCall};
use super::providers::anthropic::AnthropicClient;
use super::providers::ollama::OllamaClient;
use super::providers::openai::OpenAiClient;

/// Concrete LLM client enum
#[derive(Clone)]
pub enum LlmClient {
    OpenAi(OpenAiClient),
    Ollama(OllamaClient),
    Anthropic(AnthropicClient),
}

impl LlmClient {
    /// Create an OpenAI client
    pub fn new_openai(api_key: String, model: String, base_url: String) -> Self {
        LlmClient::OpenAi(OpenAiClient::new(api_key, model, base_url))
    }

    /// Create an Ollama client
    pub fn new_ollama(base_url: String, model: String) -> Self {
        LlmClient::Ollama(OllamaClient::new(base_url, model))
    }

    /// Create an Anthropic client
    pub fn new_anthropic(api_key: String, model: String, base_url: String) -> Self {
        LlmClient::Anthropic(AnthropicClient::new(api_key, model, base_url))
    }

    /// Call the LLM with tools
    pub async fn call(
        &self,
        state: &GameState,
        system_prompt: &str,
        tools: Vec<Tool>,
    ) -> anyhow::Result<Vec<ToolCall>> {
        match self {
            LlmClient::OpenAi(client) => client.call(state, system_prompt, tools).await,
            LlmClient::Ollama(client) => client.call(state, system_prompt, tools).await,
            LlmClient::Anthropic(client) => client.call(state, system_prompt, tools).await,
        }
    }
}
