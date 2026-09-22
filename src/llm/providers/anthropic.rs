//! Anthropic LLM provider with tool use

use serde::{Deserialize, Serialize};

use super::build_http_client;
use crate::llm::agent::{GameState, Tool, ToolCall};

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
        AnthropicClient {
            api_key,
            model,
            base_url,
            http: build_http_client(timeout),
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
    async fn call_parses_tool_use_blocks_from_mock_server() {
        let app = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post(|| async {
                axum::Json(serde_json::json!({
                    "content": [
                        {"type": "text", "text": "I suggest moving."},
                        {
                            "type": "tool_use",
                            "id": "tu_1",
                            "name": "move_troop",
                            "input": {
                                "troop_id": "t1",
                                "path": [[0, 0], [2, 1]],
                                "reason": "flank"
                            }
                        }
                    ]
                }))
            }),
        );
        let base = serve(app).await;
        let client = AnthropicClient::new("key".into(), "claude-test".into(), base);
        let calls = client
            .call(&test_state(), "system prompt", Vec::new())
            .await
            .expect("call should succeed against mock server");
        assert_eq!(calls.len(), 1, "text block must be ignored, tool_use kept");
        assert_eq!(calls[0].id, "tu_1");
        assert_eq!(calls[0].name, "move_troop");
        assert_eq!(calls[0].arguments["troop_id"], "t1");
        assert_eq!(
            calls[0].arguments["path"],
            serde_json::json!([[0, 0], [2, 1]])
        );
    }

    #[tokio::test]
    async fn call_times_out_on_hanging_endpoint() {
        // Accepts the request and then never responds.
        let app = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post(|| async {
                std::future::pending::<()>().await;
                axum::Json(serde_json::json!({}))
            }),
        );
        let base = serve(app).await;
        let client = AnthropicClient::with_timeout(
            "key".into(),
            "claude-test".into(),
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
