//! LLM advisor interface with tool calling

use crate::llm::agent::{Action, GameState};
use crate::llm::tools;

pub struct Advisor {
    pub enabled: bool,
    pub last_suggestion: Option<Advice>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Advice {
    pub text: String,
    pub action: Option<Action>,
    pub reason: String,
}

impl Advisor {
    pub fn new() -> Self {
        Advisor {
            enabled: false,
            last_suggestion: None,
        }
    }
}

impl Default for Advisor {
    fn default() -> Self {
        Self::new()
    }
}

impl Advisor {
    /// Enable or disable the advisor
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub async fn get_advice(
        &mut self,
        client: &crate::llm::clients::LlmClient,
        state: &GameState,
    ) -> Option<Advice> {
        if !self.enabled {
            return None;
        }
        let system = "You are an LLM advisor for a War of Dots RTS player. Provide ONE strategic suggestion. Keep it brief and specific. If you suggest an action, include the full parameters.";
        let tool_defs = tools::get_game_tools();
        match client.call(state, system, tool_defs).await {
            Ok(tool_calls) => {
                if let Some(tc) = tool_calls.first() {
                    // Advisor doesn't execute commands, so use dummy player_id
                    let (_command, reason) = match tools::tool_call_to_command(tc, "advisor") {
                        Ok(res) => res,
                        Err(e) => {
                            tracing::warn!(
                                "advisor: skipping malformed tool call '{}': {}",
                                tc.name,
                                e
                            );
                            return None;
                        }
                    };
                    let action = Action {
                        action_type: tc.name.clone(),
                        params: tc.arguments.clone(),
                        reason: Some(reason.clone()),
                    };
                    let advice = Advice {
                        text: format!(
                            "{}: {}",
                            tc.name,
                            serde_json::to_string(&tc.arguments).unwrap_or_default()
                        ),
                        action: Some(action),
                        reason,
                    };
                    self.last_suggestion = Some(advice.clone());
                    Some(advice)
                } else {
                    None
                }
            }
            Err(_) => None,
        }
    }
}
