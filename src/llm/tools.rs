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

/// Extract a required string field from a tool call's LLM-supplied arguments.
/// Errors when the field is missing or not a string.
fn required_str(args: &serde_json::Value, tool: &str, field: &str) -> anyhow::Result<String> {
    args.get(field)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "tool '{}': required field '{}' is missing or not a string",
                tool,
                field
            )
        })
}

/// Extract a required boolean field from a tool call's LLM-supplied arguments.
/// Errors when the field is missing or not a boolean.
fn required_bool(args: &serde_json::Value, tool: &str, field: &str) -> anyhow::Result<bool> {
    args.get(field).and_then(|v| v.as_bool()).ok_or_else(|| {
        anyhow::anyhow!(
            "tool '{}': required field '{}' is missing or not a boolean",
            tool,
            field
        )
    })
}

/// Extract the optional `reason` field (empty string when absent or not a string).
fn optional_reason(args: &serde_json::Value) -> String {
    args.get("reason")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// Parse the `move_troop` `path` field: an array of 2-element numeric coordinate pairs.
fn parse_move_path(args: &serde_json::Value) -> anyhow::Result<Vec<(f32, f32)>> {
    let coords = args.get("path").and_then(|v| v.as_array()).ok_or_else(|| {
        anyhow::anyhow!("tool 'move_troop': required field 'path' is missing or not an array")
    })?;
    let mut path = Vec::with_capacity(coords.len());
    for (i, coord) in coords.iter().enumerate() {
        let pair = coord
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("tool 'move_troop': path[{}] is not an array", i))?;
        if pair.len() < 2 {
            anyhow::bail!(
                "tool 'move_troop': path[{}] must contain at least 2 coordinates",
                i
            );
        }
        let x = pair[0]
            .as_f64()
            .ok_or_else(|| anyhow::anyhow!("tool 'move_troop': path[{}][0] is not a number", i))?;
        let y = pair[1]
            .as_f64()
            .ok_or_else(|| anyhow::anyhow!("tool 'move_troop': path[{}][1] is not a number", i))?;
        path.push((x as f32, y as f32));
    }
    Ok(path)
}

/// Convert a ToolCall into a Command, extracting the reason string.
/// Returns (Command, reason). `player_id` is used for actions that need the owner.
///
/// All LLM-supplied fields are validated: a missing or mistyped field yields an
/// `Err` (the caller logs and skips the action) instead of panicking.
pub fn tool_call_to_command(tc: &ToolCall, player_id: &str) -> anyhow::Result<(Command, String)> {
    let args = &tc.arguments;
    match tc.name.as_str() {
        "move_troop" => {
            let unit_id = required_str(args, tc.name.as_str(), "troop_id")?;
            let path = parse_move_path(args)?;
            Ok((
                Command::MoveTroop {
                    unit_id,
                    owner_id: player_id.to_string(),
                    path,
                },
                optional_reason(args),
            ))
        }
        "recruit_unit" => {
            let city_id = required_str(args, tc.name.as_str(), "city_id")?;
            let unit_type = required_str(args, tc.name.as_str(), "unit_type")?;
            Ok((
                Command::RecruitUnit(city_id, unit_type, player_id.to_string()),
                optional_reason(args),
            ))
        }
        "toggle_healing" => {
            let unit_id = required_str(args, tc.name.as_str(), "unit_id")?;
            let enabled = required_bool(args, tc.name.as_str(), "enabled")?;
            Ok((
                Command::ToggleHealing {
                    unit_id,
                    owner_id: player_id.to_string(),
                    enabled,
                },
                optional_reason(args),
            ))
        }
        "toggle_resupply" => {
            let unit_id = required_str(args, tc.name.as_str(), "unit_id")?;
            let enabled = required_bool(args, tc.name.as_str(), "enabled")?;
            Ok((
                Command::ToggleResupply {
                    unit_id,
                    owner_id: player_id.to_string(),
                    enabled,
                },
                optional_reason(args),
            ))
        }
        "upgrade_city" => {
            let city_id = required_str(args, tc.name.as_str(), "city_id")?;
            Ok((
                Command::UpgradeCity(city_id, player_id.to_string()),
                optional_reason(args),
            ))
        }
        _ => anyhow::bail!("Unknown tool: {}", tc.name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a ToolCall with a given name and raw JSON arguments.
    fn tc(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "test".into(),
            name: name.into(),
            arguments: args,
        }
    }

    // ---- move_troop ----

    #[test]
    fn move_troop_valid() {
        let c = tc(
            "move_troop",
            serde_json::json!({"troop_id": "t1", "path": [[0, 1], [2, 3]], "reason": "push"}),
        );
        let (cmd, reason) = tool_call_to_command(&c, "p1").expect("valid move_troop");
        match cmd {
            Command::MoveTroop {
                unit_id,
                owner_id,
                path,
            } => {
                assert_eq!(unit_id, "t1");
                assert_eq!(owner_id, "p1");
                assert_eq!(path, vec![(0.0, 1.0), (2.0, 3.0)]);
            }
            other => panic!("expected MoveTroop, got {other:?}"),
        }
        assert_eq!(reason, "push");
    }

    #[test]
    fn move_troop_missing_field_errs() {
        let c = tc("move_troop", serde_json::json!({"path": [[0, 1]]}));
        let err = tool_call_to_command(&c, "p1").expect_err("missing troop_id must err");
        assert!(err.to_string().contains("troop_id"), "got: {err}");
    }

    #[test]
    fn move_troop_mistyped_field_errs() {
        let c = tc(
            "move_troop",
            serde_json::json!({"troop_id": 42, "path": "not-an-array"}),
        );
        let err = tool_call_to_command(&c, "p1").expect_err("mistyped fields must err");
        assert!(err.to_string().contains("troop_id"), "got: {err}");

        let c2 = tc(
            "move_troop",
            serde_json::json!({"troop_id": "t1", "path": "nope"}),
        );
        let err2 = tool_call_to_command(&c2, "p1").expect_err("non-array path must err");
        assert!(err2.to_string().contains("path"), "got: {err2}");

        // coordinate pair with a non-number
        let c3 = tc(
            "move_troop",
            serde_json::json!({"troop_id": "t1", "path": [[0, "x"]]}),
        );
        let err3 = tool_call_to_command(&c3, "p1").expect_err("non-number coord must err");
        assert!(err3.to_string().contains("path[0][1]"), "got: {err3}");
    }

    // ---- recruit_unit ----

    #[test]
    fn recruit_unit_valid() {
        let c = tc(
            "recruit_unit",
            serde_json::json!({"city_id": "c9", "unit_type": "infantry", "reason": "replace"}),
        );
        let (cmd, reason) = tool_call_to_command(&c, "p1").expect("valid recruit_unit");
        match cmd {
            Command::RecruitUnit(city, unit_type, owner) => {
                assert_eq!(city, "c9");
                assert_eq!(unit_type, "infantry");
                assert_eq!(owner, "p1");
            }
            other => panic!("expected RecruitUnit, got {other:?}"),
        }
        assert_eq!(reason, "replace");
    }

    #[test]
    fn recruit_unit_missing_field_errs() {
        let c = tc("recruit_unit", serde_json::json!({"unit_type": "infantry"}));
        let err = tool_call_to_command(&c, "p1").expect_err("missing city_id must err");
        assert!(err.to_string().contains("city_id"), "got: {err}");

        let c2 = tc("recruit_unit", serde_json::json!({"city_id": "c9"}));
        let err2 = tool_call_to_command(&c2, "p1").expect_err("missing unit_type must err");
        assert!(err2.to_string().contains("unit_type"), "got: {err2}");
    }

    #[test]
    fn recruit_unit_mistyped_field_errs() {
        let c = tc(
            "recruit_unit",
            serde_json::json!({"city_id": 7, "unit_type": ["infantry"]}),
        );
        let err = tool_call_to_command(&c, "p1").expect_err("mistyped fields must err");
        assert!(err.to_string().contains("city_id"), "got: {err}");
    }

    // ---- toggle_healing ----

    #[test]
    fn toggle_healing_valid() {
        let c = tc(
            "toggle_healing",
            serde_json::json!({"unit_id": "u2", "enabled": true, "reason": "rest"}),
        );
        let (cmd, reason) = tool_call_to_command(&c, "p1").expect("valid toggle_healing");
        match cmd {
            Command::ToggleHealing {
                unit_id,
                owner_id,
                enabled,
            } => {
                assert_eq!(unit_id, "u2");
                assert_eq!(owner_id, "p1");
                assert!(enabled);
            }
            other => panic!("expected ToggleHealing, got {other:?}"),
        }
        assert_eq!(reason, "rest");
    }

    #[test]
    fn toggle_healing_missing_field_errs() {
        let c = tc("toggle_healing", serde_json::json!({"enabled": true}));
        let err = tool_call_to_command(&c, "p1").expect_err("missing unit_id must err");
        assert!(err.to_string().contains("unit_id"), "got: {err}");

        let c2 = tc("toggle_healing", serde_json::json!({"unit_id": "u2"}));
        let err2 = tool_call_to_command(&c2, "p1").expect_err("missing enabled must err");
        assert!(err2.to_string().contains("enabled"), "got: {err2}");
    }

    #[test]
    fn toggle_healing_mistyped_field_errs() {
        let c = tc(
            "toggle_healing",
            serde_json::json!({"unit_id": 5, "enabled": "yes"}),
        );
        let err = tool_call_to_command(&c, "p1").expect_err("mistyped fields must err");
        assert!(err.to_string().contains("unit_id"), "got: {err}");
    }

    // ---- toggle_resupply ----

    #[test]
    fn toggle_resupply_valid() {
        let c = tc(
            "toggle_resupply",
            serde_json::json!({"unit_id": "u3", "enabled": false}),
        );
        let (cmd, reason) = tool_call_to_command(&c, "p1").expect("valid toggle_resupply");
        match cmd {
            Command::ToggleResupply {
                unit_id,
                owner_id,
                enabled,
            } => {
                assert_eq!(unit_id, "u3");
                assert_eq!(owner_id, "p1");
                assert!(!enabled);
            }
            other => panic!("expected ToggleResupply, got {other:?}"),
        }
        assert_eq!(reason, "");
    }

    #[test]
    fn toggle_resupply_missing_field_errs() {
        let c = tc("toggle_resupply", serde_json::json!({"enabled": false}));
        let err = tool_call_to_command(&c, "p1").expect_err("missing unit_id must err");
        assert!(err.to_string().contains("unit_id"), "got: {err}");

        let c2 = tc("toggle_resupply", serde_json::json!({"unit_id": "u3"}));
        let err2 = tool_call_to_command(&c2, "p1").expect_err("missing enabled must err");
        assert!(err2.to_string().contains("enabled"), "got: {err2}");
    }

    #[test]
    fn toggle_resupply_mistyped_field_errs() {
        let c = tc(
            "toggle_resupply",
            serde_json::json!({"unit_id": null, "enabled": 1}),
        );
        let err = tool_call_to_command(&c, "p1").expect_err("mistyped fields must err");
        assert!(err.to_string().contains("unit_id"), "got: {err}");
    }

    // ---- upgrade_city ----

    #[test]
    fn upgrade_city_valid() {
        let c = tc(
            "upgrade_city",
            serde_json::json!({"city_id": "c4", "reason": "income"}),
        );
        let (cmd, reason) = tool_call_to_command(&c, "p1").expect("valid upgrade_city");
        match cmd {
            Command::UpgradeCity(city, owner) => {
                assert_eq!(city, "c4");
                assert_eq!(owner, "p1");
            }
            other => panic!("expected UpgradeCity, got {other:?}"),
        }
        assert_eq!(reason, "income");
    }

    #[test]
    fn upgrade_city_missing_field_errs() {
        let c = tc("upgrade_city", serde_json::json!({}));
        let err = tool_call_to_command(&c, "p1").expect_err("missing city_id must err");
        assert!(err.to_string().contains("city_id"), "got: {err}");
    }

    #[test]
    fn upgrade_city_mistyped_field_errs() {
        let c = tc("upgrade_city", serde_json::json!({"city_id": ["c4"]}));
        let err = tool_call_to_command(&c, "p1").expect_err("mistyped city_id must err");
        assert!(err.to_string().contains("city_id"), "got: {err}");
    }

    // ---- unknown tool ----

    #[test]
    fn unknown_tool_errs() {
        let c = tc("teleport", serde_json::json!({}));
        let err = tool_call_to_command(&c, "p1").expect_err("unknown tool must err");
        assert!(err.to_string().contains("Unknown tool"), "got: {err}");
    }
}
