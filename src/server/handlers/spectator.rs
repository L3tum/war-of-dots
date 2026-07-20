//! Spectator WebSocket handlers.
//!
//! Handles spectator connections that watch live games without participating.
//!
//! The actual implementation lives in game_server.rs and is exposed
//! as pub(crate) for use by the Axum router.

#[expect(unused_imports)]
use super::super::game_server::{handle_spectator_ws, spectator_ws_handler};
