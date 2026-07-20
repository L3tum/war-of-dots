//! Replay WebSocket handlers.
//!
//! Handles replay connections that let clients watch recorded games.
//!
//! The actual implementation lives in game_server.rs and is exposed
//! as pub(crate) for use by the Axum router.

#[expect(unused_imports)]
use super::super::game_server::{handle_replay_ws, replay_ws_handler};
