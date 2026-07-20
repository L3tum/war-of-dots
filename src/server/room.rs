//! Game room types.
//!
//! Contains the types shared between game_server.rs and the handlers:
//! Command, PlayerConnection, PlayerSender, LlmPlayerConfig, GameRoom.

use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

/// Commands that can be sent to a game room.
#[derive(Debug)]
pub enum Command {
    MoveTroop {
        unit_id: String,
        owner_id: String,
        path: Vec<(f32, f32)>,
    },
    RecruitUnit(String, String, String), // city_id, unit_type_name, owner
    ToggleHealing {
        unit_id: String,
        owner_id: String,
        enabled: bool,
    },
    ToggleResupply {
        unit_id: String,
        owner_id: String,
        enabled: bool,
    },
    UpgradeCity(String, String),   // city_id, owner
    ToggleAdvisor(String, bool),   // player_id, enabled
    EmbarkUnit(String, String),    // unit_id, player_id
    DisembarkUnit(String, String), // unit_id, player_id
    AddSpectator(PlayerSender),
}

/// A human player connection or a spectator.
#[derive(Clone)]
pub enum PlayerConnection {
    Player { id: String, tx: PlayerSender },
    Spectator { tx: PlayerSender },
}

impl PlayerConnection {
    pub fn id(&self) -> Option<&String> {
        match self {
            PlayerConnection::Player { id, .. } => Some(id),
            PlayerConnection::Spectator { .. } => None,
        }
    }

    pub fn tx(&self) -> &PlayerSender {
        match self {
            PlayerConnection::Player { tx, .. } => tx,
            PlayerConnection::Spectator { tx } => tx,
        }
    }

    #[allow(dead_code)]
    pub fn tx_mut(&mut self) -> &mut PlayerSender {
        match self {
            PlayerConnection::Player { tx, .. } => tx,
            PlayerConnection::Spectator { tx } => tx,
        }
    }
}

/// Unified sender that can send to either mpsc channel or WebSocket.
pub enum PlayerSender {
    Tcp(mpsc::UnboundedSender<Vec<u8>>),
    #[allow(dead_code)]
    Websocket(Arc<dyn Fn(Vec<u8>) -> Result<(), ()> + Send + Sync>),
    /// No connection (LLM-only player)
    None,
}

impl std::fmt::Debug for PlayerSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlayerSender::Tcp(_) => f.debug_tuple("PlayerSender::Tcp").finish(),
            PlayerSender::Websocket(_) => f.debug_tuple("PlayerSender::Websocket").finish(),
            PlayerSender::None => f.debug_tuple("PlayerSender::None").finish(),
        }
    }
}

impl Clone for PlayerSender {
    fn clone(&self) -> Self {
        match self {
            PlayerSender::Tcp(tx) => PlayerSender::Tcp(tx.clone()),
            PlayerSender::Websocket(f) => PlayerSender::Websocket(Arc::clone(f)),
            PlayerSender::None => PlayerSender::None,
        }
    }
}

impl PlayerSender {
    pub async fn send(&self, data: Vec<u8>) -> Result<(), ()> {
        match self {
            PlayerSender::Tcp(tx) => tx.send(data).map_err(|_| ()),
            PlayerSender::Websocket(f) => f(data),
            PlayerSender::None => Ok(()), // No connection, silently ignore
        }
    }

    #[allow(clippy::result_unit_err)]
    pub fn blocking_send(&self, data: Vec<u8>) -> Result<(), ()> {
        match self {
            PlayerSender::Tcp(tx) => tx.send(data).map_err(|_| ()),
            PlayerSender::Websocket(f) => f(data),
            PlayerSender::None => Ok(()), // No connection, silently ignore
        }
    }
}

/// LLM Agent configuration from lobby.
#[derive(Clone)]
pub struct LlmPlayerConfig {
    pub player_id: String,
    pub player_name: String,
    pub provider: String,
    pub model: String,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub thinking_delay: f64,
}

/// A game room that manages a single running game instance.
#[allow(dead_code)]
pub struct GameRoom {
    pub game_id: String,
    /// Command channel to send game actions to the running tick loop
    pub command_tx: mpsc::UnboundedSender<Command>,
    /// Shared storage for recording statistics
    pub storage: crate::storage::Storage,
    /// Spectator vision enabled (from lobby settings)
    pub spectator_vision: bool,
    /// Live per-game debug stats, updated by the tick loop and read by /debug/stats.
    pub stats: Arc<Mutex<DebugGameStats>>,
}

/// Live performance stats for a single running game, updated each tick.
#[derive(Clone, Default)]
pub struct DebugGameStats {
    pub tick_count: u64,
    pub last_tick_ms: f64,
    pub entity_count: usize,
}
