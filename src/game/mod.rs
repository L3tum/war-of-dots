//! Game module — ECS-based game engine using hecs.

pub mod components;
pub mod constants;
pub mod debug_stats;
#[allow(clippy::module_inception)]
pub mod game;
pub mod systems;
pub mod world_ext;

// Re-export old modules that are still needed for types
pub mod economy;
pub mod player;
pub mod unit;
