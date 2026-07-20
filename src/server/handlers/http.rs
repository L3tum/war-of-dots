//! HTTP handlers for the game server.
//!
//! Serves the frontend, static assets, map list, and replay list.
//!
//! The actual implementation lives in game_server.rs and is exposed
//! as pub(crate) for use by the Axum router.

#[expect(unused_imports)]
use super::super::game_server::{
    index_handler, list_maps_handler, list_replays, serve_static, watch_handler,
};
