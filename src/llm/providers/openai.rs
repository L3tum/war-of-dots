//! OpenAI LLM provider with structured tool calling

use serde::{Deserialize, Serialize};

use crate::llm::agent::{Action, GameState, Tool, ToolCall};

#[derive(Clone)]
pub struct OpenAiClient {
    api_key: String,
    model: String,
    base_url: String,
}

/// Request body for OpenAI chat completions
#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Tool>>,
}

#[derive(Debug, Serialize)]
struct ChatMessage {
    role: &'static str,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ToolCall>>,
}

/// Response from OpenAI
#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ResponseMessage,
}

#[derive(Debug, Deserialize)]
struct ResponseMessage {
    #[allow(dead_code)]
    content: Option<String>,
    tool_calls: Option<Vec<OpenAiToolCall>>,
}

#[derive(Debug, Deserialize)]
struct OpenAiToolCall {
    id: String,
    #[serde(rename = "type")]
    #[allow(dead_code)]
    call_type: String,
    function: FunctionCall,
}

#[derive(Debug, Deserialize)]
struct FunctionCall {
    name: String,
    arguments: String,
}

impl OpenAiClient {
    pub fn new(api_key: String, model: String, base_url: String) -> Self {
        OpenAiClient {
            api_key,
            model,
            base_url,
        }
    }
}

impl OpenAiClient {
    pub async fn call(
        &self,
        state: &GameState,
        system_prompt: &str,
        tools: Vec<Tool>,
    ) -> anyhow::Result<Vec<ToolCall>> {
        let state_json = serde_json::to_string(state)?;
        let request = ChatRequest {
            model: self.model.clone(),
            messages: vec![
                ChatMessage {
                    role: "system",
                    content: system_prompt.to_string(),
                    tool_calls: None,
                },
                ChatMessage {
                    role: "user",
                    content: state_json,
                    tool_calls: None,
                },
            ],
            tools: Some(tools),
        };

        let url = format!(
            "{}/v1/chat/completions",
            self.base_url.trim_end_matches('/')
        );
        let client = reqwest::Client::new();
        let response = client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await?
            .json::<ChatResponse>()
            .await?;

        let mut calls = Vec::new();
        if let Some(choice) = response.choices.first() {
            if let Some(tool_calls) = &choice.message.tool_calls {
                for tc in tool_calls {
                    let arguments = serde_json::from_str(&tc.function.arguments)?;
                    calls.push(ToolCall {
                        id: tc.id.clone(),
                        name: tc.function.name.clone(),
                        arguments,
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
