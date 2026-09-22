//! OpenAI LLM provider with structured tool calling

use serde::{Deserialize, Serialize};

use super::build_http_client;
use crate::llm::agent::{GameState, Tool, ToolCall};

#[derive(Clone)]
pub struct OpenAiClient {
    api_key: String,
    model: String,
    base_url: String,
    /// Shared HTTP client (connection-pooled; reused across calls instead of
    /// constructing a fresh `reqwest::Client` per request).
    http: reqwest::Client,
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
    /// Create a client with the default 30 s request timeout.
    pub fn new(api_key: String, model: String, base_url: String) -> Self {
        Self::with_timeout(api_key, model, base_url, None)
    }

    /// Create a client with an explicit total request timeout.
    /// Pass `None` for the default (30 s); the connect timeout is always 10 s.
    pub fn with_timeout(
        api_key: String,
        model: String,
        base_url: String,
        timeout: Option<std::time::Duration>,
    ) -> Self {
        OpenAiClient {
            api_key,
            model,
            base_url,
            http: build_http_client(timeout),
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
        let client = &self.http;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::agent::GameState;

    fn test_state() -> GameState {
        GameState {
            vision_grid: vec![vec![true, false]],
            border_grid: vec![vec![0, 255]],
            units: Vec::new(),
            cities: Vec::new(),
            funds: 100,
            player_id: "p1".into(),
        }
    }

    /// Spin up an axum router on a random loopback port; returns the base URL.
    async fn serve(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        // Give the accept loop a head start.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        format!("http://127.0.0.1:{port}")
    }

    #[tokio::test]
    async fn call_parses_tool_calls_from_mock_server() {
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(|| async {
                axum::Json(serde_json::json!({
                    "choices": [{
                        "message": {
                            "content": null,
                            "tool_calls": [{
                                "id": "call_1",
                                "type": "function",
                                "function": {
                                    "name": "upgrade_city",
                                    "arguments": "{\"city_id\":\"c1\",\"reason\":\"grow\"}"
                                }
                            }]
                        }
                    }]
                }))
            }),
        );
        let base = serve(app).await;
        let client = OpenAiClient::new("key".into(), "gpt-test".into(), base);
        let calls = client
            .call(&test_state(), "system prompt", Vec::new())
            .await
            .expect("call should succeed against mock server");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "upgrade_city");
        assert_eq!(calls[0].arguments["city_id"], "c1");
        assert_eq!(calls[0].arguments["reason"], "grow");
    }

    #[tokio::test]
    async fn call_times_out_on_hanging_endpoint() {
        // Accepts the request and then never responds.
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(|| async {
                std::future::pending::<()>().await;
                axum::Json(serde_json::json!({}))
            }),
        );
        let base = serve(app).await;
        let client = OpenAiClient::with_timeout(
            "key".into(),
            "gpt-test".into(),
            base,
            Some(std::time::Duration::from_millis(800)),
        );
        let start = std::time::Instant::now();
        let res = client
            .call(&test_state(), "system prompt", Vec::new())
            .await;
        let elapsed = start.elapsed();
        assert!(res.is_err(), "hanging endpoint must error, got {res:?}");
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "call should error quickly, took {elapsed:?}"
        );
    }
}
