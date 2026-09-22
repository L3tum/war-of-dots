//! Ollama LLM provider with structured tool calling

use serde::{Deserialize, Serialize};

use super::build_http_client;
use crate::llm::agent::{GameState, Tool, ToolCall};

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
    /// Create a client with the default 30 s request timeout.
    pub fn new(base_url: String, model: String) -> Self {
        Self::with_timeout(base_url, model, None)
    }

    /// Create a client with an explicit total request timeout.
    /// Pass `None` for the default (30 s); the connect timeout is always 10 s.
    pub fn with_timeout(
        base_url: String,
        model: String,
        timeout: Option<std::time::Duration>,
    ) -> Self {
        OllamaClient {
            base_url,
            model,
            http: build_http_client(timeout),
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
            "/api/chat",
            axum::routing::post(|| async {
                axum::Json(serde_json::json!({
                    "message": {
                        "content": "",
                        "tool_calls": [{
                            "id": "tc_1",
                            "type": "function",
                            "function": {
                                "name": "recruit_unit",
                                "arguments": {
                                    "city_id": "c1",
                                    "unit_type": "soldier",
                                    "reason": "replace losses"
                                }
                            }
                        }]
                    }
                }))
            }),
        );
        let base = serve(app).await;
        let client = OllamaClient::new(base, "llama-test".into());
        let calls = client
            .call(&test_state(), "system prompt", Vec::new())
            .await
            .expect("call should succeed against mock server");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "tc_1");
        assert_eq!(calls[0].name, "recruit_unit");
        assert_eq!(calls[0].arguments["city_id"], "c1");
        assert_eq!(calls[0].arguments["unit_type"], "soldier");
    }

    #[tokio::test]
    async fn call_times_out_on_hanging_endpoint() {
        // Accepts the request and then never responds.
        let app = axum::Router::new().route(
            "/api/chat",
            axum::routing::post(|| async {
                std::future::pending::<()>().await;
                axum::Json(serde_json::json!({}))
            }),
        );
        let base = serve(app).await;
        let client = OllamaClient::with_timeout(
            base,
            "llama-test".into(),
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
