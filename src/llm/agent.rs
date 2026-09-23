//! LLM agent interface with structured tool calling

use serde::{Deserialize, Serialize};

/// LLM agent configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmAgentConfig {
    pub provider: String,
    pub model: String,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
}

/// Tool definition following JSON Schema format (OpenAI-compatible)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Tool call returned by the LLM
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// Legacy action type kept for backward compatibility and advisor
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Action {
    pub action_type: String,
    pub params: serde_json::Value,
    pub reason: Option<String>,
}

/// Use the enum-based client from clients module
pub use super::clients::LlmClient;

/// Game state exposed to LLM (filtered per player's vision)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameState {
    pub vision_grid: Vec<Vec<bool>>,
    pub border_grid: Vec<Vec<u32>>,
    pub units: Vec<UnitState>,
    pub cities: Vec<CityState>,
    pub funds: u32,
    pub player_id: String,
}

/// Unit state for LLM visibility
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnitState {
    pub id: String,
    pub owner: String,
    pub position: (f32, f32),
    pub men: f32,
    pub max_men: f32,
    pub organisation: f32,
    pub equipment: Vec<EquipmentState>,
    pub unit_type: String,
    pub symbol: String,
    pub supply_percent: f32,
    pub resupply_enabled: bool,
}

/// Equipment state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EquipmentState {
    pub name: String,
    pub count: u32,
    pub max_count: u32,
}

/// City state for LLM
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CityState {
    pub id: String,
    pub position: (f32, f32),
    pub manpower: u32,
    pub owner: Option<String>,
    pub level: u32,
}

/// LLM Agent
#[allow(dead_code)]
pub struct LlmAgent {
    pub id: String,
    pub name: String,
    pub provider: LlmProvider,
    pub thinking_delay: std::time::Duration,
    pub reason_log: Vec<ReasonEntry>,
    pub last_call_time: std::time::Instant,
}

/// LLM Provider configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmProvider {
    pub name: String,
    pub model: String,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
}

/// Reason log entry for spectator display
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ReasonEntry {
    pub timestamp: f64,
    pub action: String,
    pub reason: String,
}

impl LlmAgent {
    pub fn new(id: &str, name: &str, provider: LlmProvider, thinking_delay: f64) -> Self {
        LlmAgent {
            id: id.to_string(),
            name: name.to_string(),
            provider,
            thinking_delay: std::time::Duration::from_secs_f64(thinking_delay),
            reason_log: Vec::new(),
            last_call_time: std::time::Instant::now(),
        }
    }

    /// Build a system prompt describing the game and the player's goal
    pub fn build_system_prompt(&self) -> String {
        format!(
            "You are an LLM agent controlling player '{}' in a real-time strategy game called War of Dots. \
            You command troops to move, recruit units, and heal. \
            Your goal is to capture cities and expand your territory. \
            In your state, `border_grid` is a territory-ownership map: each cell holds a player \
            index (0..n) or 255 (unclaimed). Borders form where two different players' cells meet. \
            Use the provided tools to make decisions. Be strategic and consider your opponent's movements. \
            Always provide a brief reason for each action.",
            self.name
        )
    }

    /// Execute one step: call the LLM and return commands to execute
    pub async fn step(
        &mut self,
        client: &LlmClient,
        state: &GameState,
    ) -> anyhow::Result<Vec<crate::server::room::Command>> {
        // Use time-based delay instead of turn count
        let now = std::time::Instant::now();
        if now.duration_since(self.last_call_time) < self.thinking_delay {
            return Ok(Vec::new());
        }
        self.last_call_time = now;

        let system_prompt = self.build_system_prompt();
        let tools = crate::llm::tools::get_game_tools();

        let tool_calls = client.call(state, &system_prompt, tools).await?;
        if tool_calls.is_empty() {
            return Ok(Vec::new());
        }

        // Log reasons and handle malformed tool calls
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);

        let mut commands = Vec::new();
        for tc in &tool_calls {
            match crate::llm::tools::tool_call_to_command(tc, &self.id) {
                Ok((action_str, reason)) => {
                    commands.push(action_str);
                    if !reason.is_empty() {
                        self.reason_log.push(ReasonEntry {
                            timestamp,
                            action: format!("{}({:?})", tc.name, tc.arguments),
                            reason,
                        });
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "LLM agent {}: skipping malformed tool call '{}': {}",
                        self.id,
                        tc.name,
                        e
                    );
                    self.reason_log.push(ReasonEntry {
                        timestamp,
                        action: format!("Invalid tool call: {}", tc.name),
                        reason: format!("Error: {}", e),
                    });
                }
            }
        }

        Ok(commands)
    }
}
