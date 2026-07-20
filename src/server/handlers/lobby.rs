//! Lobby and game action handlers.
//!
//! Handles WebSocket connections for game clients, including:
//! - Lobby management (create, join, leave, start game)
//! - In-game commands (move, recruit, upgrade, etc.)
//! - TCP and WebSocket transport layers
//!
//! The actual implementation lives in game_server.rs and is exposed
//! as pub(crate) for use by the Axum router.

#[expect(unused_imports)]
use super::super::game_server::{
    handle_client_message, handle_tcp_client, handle_ws_game, ws_handler,
};
