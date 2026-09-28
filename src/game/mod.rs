//! Game module — ECS-based game engine using hecs.

pub mod baselines;
pub mod components;
pub mod constants;
pub mod debug_stats;
pub mod entities;
#[allow(clippy::module_inception)]
pub mod game;
pub mod spatial;
pub mod systems;
pub mod work_state;
pub mod world_ext;

// Re-export old modules that are still needed for types
pub mod economy;
pub mod player;
pub mod unit;

// H5: engine-neutral network baselines (plain engine types, single commit
// step) — the server's delta path diffs the current snapshot against them.
pub use baselines::NetworkBaselines;
