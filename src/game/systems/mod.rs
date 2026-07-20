//! Game systems — pure functions that operate on hecs::World.

pub mod attrition;
pub mod city_capture;
pub mod combat;
pub mod economy;
pub mod encirclement;
pub mod healing;
pub mod influence;
pub mod movement;
pub mod reinforce;
pub mod spatial_hash;
pub mod supply;
pub mod surrender;
pub mod water;

// Legacy systems — replaced by influence.rs (kept for reference, not called)
// pub mod border;
// pub mod vision;
