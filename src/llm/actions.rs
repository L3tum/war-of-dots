//! Provider-neutral actions the LLM layer can express.
//!
//! The llm layer owns this enum and has no dependency on the server
//! layer: an LLM acts *as* its player, so ownership is not part of the
//! action. The server attaches `owner_id`/`player_id` when it maps an
//! action into its own `Command` at the room boundary
//! (`server::room::llm_tasks::action_to_command`).

/// Provider-neutral actions an LLM agent can express.
///
/// Variants mirror the LLM tool surface (see `tools.rs`) minus
/// ownership. In-memory only, like the server's `Command` — no serde.
#[derive(Debug, Clone)]
pub enum LlmAction {
    MoveTroop {
        unit_id: String,
        path: Vec<(f32, f32)>,
    },
    RecruitUnit {
        city_id: String,
        unit_type: String,
    },
    ToggleHealing {
        unit_id: String,
        enabled: bool,
    },
    ToggleResupply {
        unit_id: String,
        enabled: bool,
    },
    UpgradeCity {
        city_id: String,
    },
}
