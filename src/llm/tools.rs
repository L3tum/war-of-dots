//! Tool definitions and command conversion for LLM agents

use super::agent::{Tool, ToolCall};
use crate::server::game_server::Command;

/// Get all available game tools for LLM
pub fn get_game_tools() -> Vec<Tool> {
    vec![
        move_troop_tool(),
        recruit_unit_tool(),
        toggle_healing_tool(),
        toggle_resupply_tool(),
        upgrade_city_tool(),
    ]
}

/// Move a single troop tool
fn move_troop_tool() -> Tool {
    Tool {
        name: "move_troop".into(),
        description: "Move a single troop along a path of grid coordinates. The `reason` field is a brief explanation of why you're making this move.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "troop_id": {"type": "string"},
                "path": {"type": "array", "items": {"type": "array", "items": {"type": "number"}}},
                "reason": {"type": "string"}
            },
            "required": ["troop_id", "path"]
        }),
    }
}

/// Recruit unit tool
fn recruit_unit_tool() -> Tool {
    Tool {
        name: "recruit_unit".into(),
        description:
            "Recruit a new unit at one of your cities. Provide the city ID and unit type name."
                .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "city_id": {"type": "string"},
                "unit_type": {"type": "string"},
                "reason": {"type": "string"}
            },
            "required": ["city_id", "unit_type"]
        }),
    }
}

/// Toggle healing tool
fn toggle_healing_tool() -> Tool {
    Tool {
        name: "toggle_healing".into(),
        description: "Toggle healing on or off for a unit. Healing allows a unit to regenerate men but it cannot move while healing.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "unit_id": {"type": "string"},
                "enabled": {"type": "boolean"},
                "reason": {"type": "string"}
            },
            "required": ["unit_id", "enabled"]
        }),
    }
}

/// Toggle equipment resupply tool
fn toggle_resupply_tool() -> Tool {
    Tool {
        name: "toggle_resupply".into(),
        description: "Toggle equipment resupply on or off for a unit. When enabled, the unit receives equipment from friendly cities within supply range. Resupply costs funds per item delivered.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "unit_id": {"type": "string"},
                "enabled": {"type": "boolean"},
                "reason": {"type": "string"}
            },
            "required": ["unit_id", "enabled"]
        }),
    }
}

/// Upgrade city tool
fn upgrade_city_tool() -> Tool {
    Tool {
        name: "upgrade_city".into(),
        description: "Upgrade a city to increase its income multiplier, supply output, and supply range. Costs increase with each level. Max level is 3.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "city_id": {"type": "string"},
                "reason": {"type": "string"}
            },
            "required": ["city_id"]
        }),
    }
}

/// Convert a ToolCall into a Command, extracting the reason string.
/// Returns (Command, reason). `player_id` is used for actions that need the owner.
pub fn tool_call_to_command(tc: &ToolCall, player_id: &str) -> anyhow::Result<(Command, String)> {
    match tc.name.as_str() {
        "move_troop" => {
            let troop_id = tc
                .arguments
                .get("troop_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let reason = tc
                .arguments
                .get("reason")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default();
            let path = tc
                .arguments
                .get("path")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|coord| {
                            coord.as_array().and_then(|c| {
                                if c.len() >= 2 {
                                    Some((
                                        c[0].as_f64().unwrap_or(0.0) as f32,
                                        c[1].as_f64().unwrap_or(0.0) as f32,
                                    ))
                                } else {
                                    None
                                }
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok((
                Command::MoveTroop {
                    unit_id: troop_id,
                    owner_id: player_id.to_string(),
                    path,
                },
                reason,
            ))
        }
        "recruit_unit" => {
            let city_id = tc
                .arguments
                .get("city_id")
                .and_then(|v| v.as_str())
                .unwrap()
                .to_string();
            let unit_type = tc
                .arguments
                .get("unit_type")
                .and_then(|v| v.as_str())
                .unwrap()
                .to_string();
            let reason = tc
                .arguments
                .get("reason")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default();
            Ok((
                Command::RecruitUnit(city_id, unit_type, player_id.to_string()),
                reason,
            ))
        }
        "toggle_healing" => {
            let unit_id = tc
                .arguments
                .get("unit_id")
                .and_then(|v| v.as_str())
                .unwrap()
                .to_string();
            let enabled = tc
                .arguments
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let reason = tc
                .arguments
                .get("reason")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default();
            Ok((
                Command::ToggleHealing {
                    unit_id,
                    owner_id: player_id.to_string(),
                    enabled,
                },
                reason,
            ))
        }
        "toggle_resupply" => {
            let unit_id = tc
                .arguments
                .get("unit_id")
                .and_then(|v| v.as_str())
                .unwrap()
                .to_string();
            let enabled = tc
                .arguments
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let reason = tc
                .arguments
                .get("reason")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default();
            Ok((
                Command::ToggleResupply {
                    unit_id,
                    owner_id: player_id.to_string(),
                    enabled,
                },
                reason,
            ))
        }
        "upgrade_city" => {
            let city_id = tc
                .arguments
                .get("city_id")
                .and_then(|v| v.as_str())
                .unwrap()
                .to_string();
            let reason = tc
                .arguments
                .get("reason")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default();
            Ok((Command::UpgradeCity(city_id, player_id.to_string()), reason))
        }
        _ => anyhow::bail!("Unknown tool: {}", tc.name),
    }
}
