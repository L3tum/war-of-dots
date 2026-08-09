//! Ollama LLM provider with structured tool calling

use serde::{Deserialize, Serialize};

use crate::llm::agent::{Action, GameState, Tool, ToolCall};

#[derive(Clone)]
pub struct OllamaClient {
    base_url: String,
    model: String,
    /// Shared HTTP client (connection-pooled; reused across calls instead of
    /// constructing a fresh `reqwest::Client` per request).
    http: reqwest::Client,
}

/// Request body for Ollama /api/chat
#[derive(Debug, Serialize)]
struct OllamaRequest {
    model: String,
    messages: Vec<OllamaMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Tool>>,
}

#[derive(Debug, Serialize)]
struct OllamaMessage {
    role: &'static str,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ToolCall>>,
}

/// Response from Ollama
#[derive(Debug, Deserialize)]
struct OllamaResponse {
    message: OllamaResponseMessage,
}

#[derive(Debug, Deserialize)]
struct OllamaResponseMessage {
    #[expect(dead_code)]
    content: Option<String>,
    tool_calls: Option<Vec<OllamaToolCall>>,
}

#[derive(Debug, Deserialize)]
struct OllamaToolCall {
    id: String,
    #[serde(rename = "type")]
    #[expect(dead_code)]
    call_type: String,
    function: OllamaFunctionCall,
}

#[derive(Debug, Deserialize)]
struct OllamaFunctionCall {
    name: String,
    arguments: serde_json::Value,
}

impl OllamaClient {
    pub fn new(base_url: String, model: String) -> Self {
        OllamaClient {
            base_url,
            model,
            http: reqwest::Client::new(),
        }
    }
}

impl OllamaClient {
    pub async fn call(
        &self,
        state: &GameState,
        system_prompt: &str,
        tools: Vec<Tool>,
    ) -> anyhow::Result<Vec<ToolCall>> {
        let state_json = serde_json::to_string(state)?;
        let request = OllamaRequest {
            model: self.model.clone(),
            messages: vec![
                OllamaMessage {
                    role: "system",
                    content: system_prompt.to_string(),
                    tool_calls: None,
                },
                OllamaMessage {
                    role: "user",
                    content: state_json,
                    tool_calls: None,
                },
            ],
            tools: Some(tools),
        };

        let url = format!("{}/api/chat", self.base_url.trim_end_matches('/'));
        let client = &self.http;
        let response = client
            .post(&url)
            .json(&request)
            .send()
            .await?
            .json::<OllamaResponse>()
            .await?;

        let mut calls = Vec::new();
        if let Some(tool_calls) = &response.message.tool_calls {
            for tc in tool_calls {
                calls.push(ToolCall {
                    id: tc.id.clone(),
                    name: tc.function.name.clone(),
                    arguments: tc.function.arguments.clone(),
                });
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
