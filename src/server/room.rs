//! Game room types.
//!
//! Contains the types shared between game_server.rs and the handlers:
//! Command, PlayerConnection, PlayerSender, LlmPlayerConfig, GameRoom.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

/// I7: keep-latest ring depth for per-tick delta payloads. A stale delta is
/// worthless once a newer one exists, so depth 2–4 is plenty of slack between
/// the tick loop pushing and the sender task draining.
pub const DELTA_RING_DEPTH: usize = 3;

/// I7: tagged egress envelope for player connections.
///
/// `Delta` carries only the sequence number of the payload in the
/// per-connection keep-latest ring (`DeltaRing`) — the encoded bytes are held
/// by the ring and evicted once `DELTA_RING_DEPTH` newer deltas exist, so a
/// stalled reader cannot accumulate unbounded delta payloads. `Reliable`
/// carries its payload inline: `FullState`, `GameOver`, lobby and error
/// messages are always queued and never dropped.
pub enum EgressMessage {
    Delta(u64),
    Reliable(Vec<u8>),
}

/// I7: bounded keep-latest ring of encoded delta payloads.
#[derive(Debug)]
pub struct DeltaRing {
    depth: usize,
    slots: Mutex<VecDeque<(u64, Arc<Vec<u8>>)>>,
}

impl DeltaRing {
    pub fn new(depth: usize) -> Self {
        let depth = depth.max(1);
        Self {
            depth,
            slots: Mutex::new(VecDeque::with_capacity(depth)),
        }
    }

    /// Append a delta, evicting the oldest once past depth. Returns its sequence number.
    pub fn push(&self, bytes: Vec<u8>) -> u64 {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let seq = slots.back().map(|(s, _)| s + 1).unwrap_or(0);
        slots.push_back((seq, Arc::new(bytes)));
        while slots.len() > self.depth {
            slots.pop_front();
        }
        seq
    }

    /// Take ownership of a delta by sequence (drain side). `None` when the
    /// entry was already evicted — a newer delta superseded it, so the drain
    /// loop drops the envelope and moves on.
    pub fn take(&self, seq: u64) -> Option<Vec<u8>> {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let pos = slots.iter().position(|(s, _)| *s == seq)?;
        let (_, arc) = slots.remove(pos)?;
        // The drain task is the sole consumer, so the ring was the last owner;
        // fall back to a copy in the (impossible) case of a live extra ref.
        Some(Arc::try_unwrap(arc).unwrap_or_else(|arc| arc.to_vec()))
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.slots.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }

    /// Total payload bytes currently held — bounded by `depth` pushes.
    #[cfg(test)]
    pub fn byte_size(&self) -> usize {
        self.slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(_, b)| b.len())
            .sum()
    }

    #[cfg(test)]
    pub fn latest_seq(&self) -> Option<u64> {
        self.slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .back()
            .map(|(s, _)| *s)
    }
}

/// I7: channel + keep-latest ring for one connection's egress.
#[derive(Clone)]
pub struct TcpEgress {
    pub tx: mpsc::UnboundedSender<EgressMessage>,
    pub ring: Arc<DeltaRing>,
}

impl TcpEgress {
    pub fn new(tx: mpsc::UnboundedSender<EgressMessage>) -> Self {
        Self {
            tx,
            ring: Arc::new(DeltaRing::new(DELTA_RING_DEPTH)),
        }
    }
}

/// Resolve one egress envelope into wire bytes. `None` = a stale delta whose
/// ring entry was evicted by a newer delta — skip it.
pub fn egress_bytes(msg: EgressMessage, ring: &DeltaRing) -> Option<Vec<u8>> {
    match msg {
        EgressMessage::Delta(seq) => ring.take(seq),
        EgressMessage::Reliable(data) => Some(data),
    }
}

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
///
/// The TCP path is I7-typed: deltas go through the bounded keep-latest ring,
/// reliable messages are queued unboundedly and never dropped.
pub enum PlayerSender {
    Tcp(TcpEgress),
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
            PlayerSender::Tcp(e) => PlayerSender::Tcp(e.clone()),
            PlayerSender::Websocket(f) => PlayerSender::Websocket(Arc::clone(f)),
            PlayerSender::None => PlayerSender::None,
        }
    }
}

impl PlayerSender {
    /// I7: per-tick delta — routed through the bounded keep-latest ring.
    fn push_delta(&self, data: Vec<u8>) -> Result<(), ()> {
        match self {
            PlayerSender::Tcp(e) => {
                let seq = e.ring.push(data);
                e.tx.send(EgressMessage::Delta(seq)).map_err(|_| ())
            }
            PlayerSender::Websocket(f) => f(data),
            PlayerSender::None => Ok(()), // No connection, silently ignore
        }
    }

    /// I7: reliable egress (FullState, GameOver, lobby, errors) — never dropped.
    fn push_reliable(&self, data: Vec<u8>) -> Result<(), ()> {
        match self {
            PlayerSender::Tcp(e) => e.tx.send(EgressMessage::Reliable(data)).map_err(|_| ()),
            PlayerSender::Websocket(f) => f(data),
            PlayerSender::None => Ok(()), // No connection, silently ignore
        }
    }

    pub async fn send_delta(&self, data: Vec<u8>) -> Result<(), ()> {
        self.push_delta(data)
    }

    pub async fn send_reliable(&self, data: Vec<u8>) -> Result<(), ()> {
        self.push_reliable(data)
    }

    #[allow(clippy::result_unit_err)]
    pub fn blocking_send_reliable(&self, data: Vec<u8>) -> Result<(), ()> {
        self.push_reliable(data)
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
