//! Anthropic LLM provider with tool use

use serde::{Deserialize, Serialize};

use crate::llm::agent::{Action, GameState, Tool, ToolCall};

#[derive(Clone)]
pub struct AnthropicClient {
    api_key: String,
    model: String,
    base_url: String,
    /// Shared HTTP client (connection-pooled; reused across calls instead of
    /// constructing a fresh `reqwest::Client` per request).
    http: reqwest::Client,
}

/// Request body for Anthropic messages API
#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: String,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicTool>>,
    max_tokens: u32,
}

#[derive(Debug, Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct AnthropicMessage {
    role: &'static str,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_use: Option<Vec<AnthropicToolUse>>,
}

#[derive(Debug, Serialize)]
struct AnthropicToolUse {
    id: String,
    name: String,
    input: serde_json::Value,
}

/// Response from Anthropic
#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content: Vec<ContentBlock>,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    #[expect(dead_code)]
    text: Option<String>,
    #[serde(rename = "id")]
    tool_use_id: Option<String>,
    name: Option<String>,
    input: Option<serde_json::Value>,
}

impl AnthropicClient {
    pub fn new(api_key: String, model: String, base_url: String) -> Self {
        AnthropicClient {
            api_key,
            model,
            base_url,
            http: reqwest::Client::new(),
        }
    }
}

impl AnthropicClient {
    pub async fn call(
        &self,
        state: &GameState,
        system_prompt: &str,
        tools: Vec<Tool>,
    ) -> anyhow::Result<Vec<ToolCall>> {
        let state_json = serde_json::to_string(state)?;
        let anthropic_tools: Vec<AnthropicTool> = tools
            .into_iter()
            .map(|t| AnthropicTool {
                name: t.name,
                description: t.description,
                input_schema: t.parameters,
            })
            .collect();

        let request = AnthropicRequest {
            model: self.model.clone(),
            messages: vec![
                AnthropicMessage {
                    role: "system",
                    content: system_prompt.to_string(),
                    tool_use: None,
                },
                AnthropicMessage {
                    role: "user",
                    content: state_json,
                    tool_use: None,
                },
            ],
            tools: Some(anthropic_tools),
            max_tokens: 4096,
        };

        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let client = &self.http;
        let response = client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await?
            .json::<AnthropicResponse>()
            .await?;

        let mut calls = Vec::new();
        for block in &response.content {
            if block.block_type == "tool_use" {
                if let (Some(id), Some(name), Some(input)) =
                    (&block.tool_use_id, &block.name, &block.input)
                {
                    calls.push(ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: input.clone(),
                    });
                }
            }
        }
        Ok(calls)
    }

    pub async fn query(
        &self,
        state: &GameState,
        system_prompt: &str,
    ) -> anyhow::Result<Vec<Action>> {
        let calls = self.call(state, system_prompt, Vec::new()).await?;
        Ok(calls
            .into_iter()
            .map(|tc| {
                let reason = tc
                    .arguments
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                Action {
                    action_type: tc.name,
                    params: tc.arguments,
                    reason,
                }
            })
            .collect())
    }
}
