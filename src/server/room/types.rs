//! Game room types.
//!
//! Contains the types shared between game_server.rs and the handlers:
//! Command, PlayerConnection, PlayerSender, LlmPlayerConfig, GameRoom.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tokio::sync::mpsc;

#[cfg(test)]
use crate::server::handlers::tcp::{configure_game_tcp_stream, tcp_egress_loop, ws_egress_loop};
#[cfg(test)]
use crate::server::protocol::{encode_message, ServerMessage};
#[cfg(test)]
use axum::extract::ws::{Message, WebSocketUpgrade};
#[cfg(test)]
use axum::extract::State;
#[cfg(test)]
use axum::routing::get;
#[cfg(test)]
use futures::StreamExt;

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
    Reliable(Bytes),
    /// M3/W7: explicit close requested by the tick task (a player's delta
    /// encode failed — a half frame is unrecoverable, so the egress loop
    /// ends the connection instead of leaving the peer desynced).
    Close,
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

/// Resolve one egress envelope into wire bytes for the raw-TCP drain.
/// `None` = a stale delta whose ring entry was evicted by a newer delta — skip
/// it. Returns `Bytes`: the TCP path writes the slice directly and the
/// `Reliable` arm hands out the shared broadcast allocation without copying.
pub fn egress_bytes(msg: EgressMessage, ring: &DeltaRing) -> Option<Bytes> {
    match msg {
        EgressMessage::Delta(seq) => ring.take(seq).map(Bytes::from),
        EgressMessage::Reliable(data) => Some(data),
        // The egress loops handle `Close` before resolution (it carries no
        // payload); this arm only keeps the match exhaustive.
        EgressMessage::Close => None,
    }
}

/// WS variant of [`egress_bytes`]: axum 0.7's `Message::Binary` takes
/// `Vec<u8>`, and the delta path recovers the ring's owned `Vec` without a
/// copy (same behavior as pre-`Bytes`). Only `Reliable` broadcasts pay one
/// `to_vec` here — the per-player clones earlier in the tick loop are
/// refcount bumps, so the copies leave the tick task and happen at most once
/// per delivered frame, right before WS framing copies into the socket
/// buffer anyway.
pub fn egress_vec(msg: EgressMessage, ring: &DeltaRing) -> Option<Vec<u8>> {
    match msg {
        EgressMessage::Delta(seq) => ring.take(seq),
        EgressMessage::Reliable(data) => Some(data.to_vec()),
        // The egress loops handle `Close` before resolution (it carries no
        // payload); this arm only keeps the match exhaustive.
        EgressMessage::Close => None,
    }
}

/// Commands that can be sent to a game room.
///
/// In-memory only (no serde derive — nothing wire-facing depends on the
/// variant shapes): player handlers, the LLM task mapper
/// (`llm_tasks::action_to_command`) and spectator joins construct them;
/// the tick loop consumes them. Multi-field variants use named fields;
/// `AddSpectator` keeps its single-field tuple shape.
#[derive(Debug)]
pub enum Command {
    MoveTroop {
        unit_id: String,
        owner_id: String,
        path: Vec<(f32, f32)>,
    },
    RecruitUnit {
        city_id: String,
        unit_type: String,
        owner: String,
    },
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
    UpgradeCity {
        city_id: String,
        owner: String,
    },
    ToggleAdvisor {
        player_id: String,
        enabled: bool,
    },
    EmbarkUnit {
        unit_id: String,
        player_id: String,
    },
    DisembarkUnit {
        unit_id: String,
        player_id: String,
    },
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
}

/// Unified sender that can send to either mpsc channel or WebSocket.
///
/// The TCP path is I7-typed: deltas go through the bounded keep-latest ring,
/// reliable messages are queued unboundedly and never dropped.
pub enum PlayerSender {
    Tcp(TcpEgress),
    /// No connection (LLM-only player)
    None,
}

impl std::fmt::Debug for PlayerSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlayerSender::Tcp(_) => f.debug_tuple("PlayerSender::Tcp").finish(),
            PlayerSender::None => f.debug_tuple("PlayerSender::None").finish(),
        }
    }
}

impl Clone for PlayerSender {
    fn clone(&self) -> Self {
        match self {
            PlayerSender::Tcp(e) => PlayerSender::Tcp(e.clone()),
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
            PlayerSender::None => Ok(()), // No connection, silently ignore
        }
    }

    /// I7: reliable egress (FullState, GameOver, lobby, errors) — never dropped.
    fn push_reliable(&self, data: Bytes) -> Result<(), ()> {
        match self {
            PlayerSender::Tcp(e) => e.tx.send(EgressMessage::Reliable(data)).map_err(|_| ()),
            PlayerSender::None => Ok(()), // No connection, silently ignore
        }
    }

    pub async fn send_delta(&self, data: Vec<u8>) -> Result<(), ()> {
        self.push_delta(data)
    }

    pub async fn send_reliable(&self, data: Bytes) -> Result<(), ()> {
        self.push_reliable(data)
    }

    #[allow(clippy::result_unit_err)]
    pub fn blocking_send_reliable(&self, data: Bytes) -> Result<(), ()> {
        self.push_reliable(data)
    }

    /// M3/W7: request a connection close from the egress side (a delta
    /// encode failed — a half frame is unrecoverable, so end the stream
    /// instead of leaving the peer desynced). Best-effort: if the channel
    /// is already closed (peer gone), there is nothing to do.
    pub fn close(&self) {
        if let PlayerSender::Tcp(e) = self {
            let _ = e.tx.send(EgressMessage::Close);
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

// ---------------------------------------------------------------------------
// I7: typed bounded keep-latest egress — unit + slow-client soak tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod i7_egress_tests {
    use super::*;
    use crate::server::room::{DeltaRing, DELTA_RING_DEPTH};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const DELTA_SIZE: usize = 64 * 1024;
    const N_DELTAS: u64 = 512;

    type PendingEgress =
        Arc<std::sync::Mutex<Option<(mpsc::UnboundedReceiver<EgressMessage>, Arc<DeltaRing>)>>>;

    /// Wait for the test's hand-off of (channel, ring), then run the exact
    /// drain loop the server spawns for the path under test.
    async fn wait_release(
        pending: &PendingEgress,
    ) -> (mpsc::UnboundedReceiver<EgressMessage>, Arc<DeltaRing>) {
        loop {
            if let Some(v) = pending.lock().unwrap().take() {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    async fn gated_tcp_egress_loop(
        pending: PendingEgress,
        writer: tokio::io::WriteHalf<tokio::net::TcpStream>,
    ) {
        let (rx, ring) = wait_release(&pending).await;
        tcp_egress_loop(writer, rx, ring).await;
    }

    async fn gated_ws_egress_loop<S>(pending: PendingEgress, sink: Arc<tokio::sync::Mutex<S>>)
    where
        S: futures::Sink<Message> + Unpin,
    {
        let (rx, ring) = wait_release(&pending).await;
        ws_egress_loop(sink, rx, ring).await;
    }

    /// One tick's delta payload: zeroed 64 KB with a unique marker up front
    /// (the egress path is byte-transparent).
    fn delta_bytes(i: u64) -> Vec<u8> {
        let mut buf = vec![0u8; DELTA_SIZE];
        let marker = format!("I7DELTA-{i:05}").into_bytes();
        buf[..marker.len()].copy_from_slice(&marker);
        buf
    }

    pub(crate) fn find(bytes: &[u8], needle: &[u8]) -> Option<usize> {
        bytes.windows(needle.len()).position(|w| w == needle)
    }

    /// Indexes of the deltas whose markers appear in `received`, in wire order.
    fn collect_delivered(received: &[u8]) -> Vec<u64> {
        const PREFIX: &[u8] = b"I7DELTA-";
        received
            .windows(PREFIX.len() + 5)
            .filter(|w| &w[..PREFIX.len()] == PREFIX)
            .filter_map(|w| std::str::from_utf8(&w[PREFIX.len()..]).ok())
            .filter_map(|s| s.parse::<u64>().ok())
            .collect()
    }

    /// Reassemble server→client frame payloads (unmasked; continuation
    /// frames appended, ping/pong skipped, close ends the stream).
    pub(crate) fn parse_ws_frames(data: &[u8]) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 2 <= data.len() {
            let opcode = data[i] & 0x0f;
            let mut len = (data[i + 1] & 0x7f) as usize;
            let mut off = i + 2;
            if len == 126 {
                if off + 2 > data.len() {
                    return None;
                }
                len = u16::from_be_bytes([data[off], data[off + 1]]) as usize;
                off += 2;
            } else if len == 127 {
                if off + 8 > data.len() {
                    return None;
                }
                let bytes: [u8; 8] = data[off..off + 8].try_into().ok()?;
                len = u64::from_be_bytes(bytes) as usize;
                off += 8;
            }
            if data[i + 1] & 0x80 != 0 {
                off += 4; // masking key (client→server only; tolerate it)
            }
            if off + len > data.len() {
                return None;
            }
            match opcode {
                0x0..=0x2 => out.extend_from_slice(&data[off..off + len]),
                0x8 => return Some(out),
                _ => {} // ping/pong/reserved: skip
            }
            i = off + len;
        }
        Some(out)
    }

    /// Push a stall's worth of traffic: N tick deltas, then two reliable
    /// messages (Error, GameOver). Returns (last delta, error, game over).
    /// The drain task must be gated (not yet started): every pushed delta
    /// then lands in the ring and evicts the oldest past depth, so the
    /// bounded-memory invariant is assertable deterministically.
    async fn push_soak_traffic(
        sender: &PlayerSender,
        ring: &Arc<DeltaRing>,
    ) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let mut last_delta = Vec::new();
        for i in 0..N_DELTAS {
            let bytes = delta_bytes(i);
            if i + 1 == N_DELTAS {
                last_delta = bytes.clone();
            }
            sender.send_delta(bytes).await.unwrap();
        }
        // The drain task is gated and the client reader is stalled: the ring
        // holds exactly its depth — N_DELTAS deltas were pushed, but only the
        // latest DELTA_RING_DEPTH payloads remain in memory.
        assert_eq!(ring.len(), DELTA_RING_DEPTH);
        assert_eq!(ring.latest_seq(), Some(N_DELTAS - 1));
        assert!(ring.byte_size() <= DELTA_RING_DEPTH * DELTA_SIZE);

        let error_msg = encode_message(&ServerMessage::Error {
            message: "i7-soak-error".to_string(),
        });
        let game_over = encode_message(&ServerMessage::GameOver {
            winner: "i7".to_string(),
            game_id: "soak".to_string(),
            message: "i7 soak game over".to_string(),
        });
        // Reliable: always queued, never dropped.
        sender
            .send_reliable(error_msg.clone().into())
            .await
            .unwrap();
        sender
            .send_reliable(game_over.clone().into())
            .await
            .unwrap();
        (last_delta, error_msg, game_over)
    }

    /// Read the client until the GameOver payload is in the buffer (no EOF
    /// assumption — the drain task is what we are testing).
    async fn read_until(client: &mut tokio::net::TcpStream, target: &[u8], into: &mut Vec<u8>) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let mut buf = [0u8; 64 * 1024];
        let mut last_checked = 0usize;
        loop {
            if find(&into[last_checked..], target).is_some() {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "target not delivered in time"
            );
            let n = client.read(&mut buf).await.expect("client read failed");
            assert!(n > 0, "connection closed before target delivered");
            into.extend_from_slice(&buf[..n]);
            last_checked = into.len().saturating_sub(target.len());
        }
    }

    fn assert_resumed(received: &[u8], last_delta: &[u8], error_msg: &[u8], game_over: &[u8]) {
        let delivered = collect_delivered(received);
        // The stall outgrew the ring: stale deltas were dropped (that is what
        // bounds memory), so strictly fewer than N_DELTAS arrived.
        assert!(
            delivered.len() < N_DELTAS as usize,
            "stall must drop stale deltas (got {} of {})",
            delivered.len(),
            N_DELTAS
        );
        // The client resumes with the LATEST delta, in order.
        assert_eq!(
            delivered.last().copied(),
            Some(N_DELTAS - 1),
            "client must resume with the latest delta"
        );
        assert!(
            delivered.windows(2).all(|w| w[0] < w[1]),
            "in-order delivery"
        );

        // Reliable messages were never dropped and arrived after the stall
        // cleared, in order, after the latest delta.
        let err_pos = find(received, error_msg).expect("reliable Error must be delivered");
        let over_pos = find(received, game_over).expect("reliable GameOver must be delivered");
        assert!(over_pos > err_pos, "reliable preserves order");
        // The reliable error starts at or after the end of the latest delta's
        // bytes (they are adjacent in the stall-clear drain).
        let last_end =
            find(received, last_delta).expect("latest delta must be delivered") + last_delta.len();
        assert!(
            err_pos >= last_end,
            "reliable delivered after the stall clears (after the latest delta)"
        );
    }

    #[test]
    fn delta_ring_keep_latest_bounded() {
        let ring = DeltaRing::new(3);
        assert_eq!(ring.len(), 0);
        assert!(ring.is_empty());
        let a = ring.push(vec![1u8; 10]);
        let b = ring.push(vec![2u8; 10]);
        let c = ring.push(vec![3u8; 10]);
        let d = ring.push(vec![4u8; 10]);
        assert_eq!((a, b, c, d), (0u64, 1, 2, 3));
        // Keep-latest: once past depth, the oldest entry is evicted.
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.byte_size(), 30);
        assert_eq!(ring.latest_seq(), Some(d));
        assert!(ring.take(a).is_none(), "evicted seq must be gone");
        assert_eq!(ring.take(b), Some(vec![2u8; 10]));
        assert_eq!(ring.take(c), Some(vec![3u8; 10]));
        assert_eq!(ring.take(d), Some(vec![4u8; 10]));
        assert!(ring.is_empty());
    }

    #[test]
    fn egress_bytes_skips_evicted_deltas() {
        let ring = DeltaRing::new(DELTA_RING_DEPTH);
        let first = ring.push(vec![9u8; 8]);
        let last = (0..N_DELTAS).fold(first, |_, _| ring.push(vec![9u8; 8]));
        assert!(egress_bytes(EgressMessage::Delta(first), &ring).is_none());
        assert_eq!(
            egress_bytes(EgressMessage::Delta(last), &ring),
            Some(vec![9u8; 8].into())
        );
        assert_eq!(
            egress_bytes(EgressMessage::Reliable(vec![7u8].into()), &ring),
            Some(vec![7u8].into())
        );
    }

    /// Nagle must be off on the raw-TCP game transport: small delta frames at
    /// up to 10 Hz would otherwise eat the kernel's ~40 ms coalescing delay.
    /// (Invisible to the headless benchmark, which models no socket — this is
    /// the acceptance gate.)
    #[tokio::test]
    async fn tcp_game_stream_disables_nagle() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        configure_game_tcp_stream(&stream);
        assert!(
            stream.nodelay().unwrap(),
            "game TCP stream must disable Nagle"
        );
    }

    /// A broadcast frame must reach every player as a REF of one allocation,
    /// not a per-player copy: `send_reliable(Bytes)` clones are refcount
    /// bumps, so every queued envelope shares the same underlying bytes.
    #[tokio::test]
    async fn reliable_broadcast_shares_one_allocation() {
        let frame = Bytes::from(encode_message(&ServerMessage::ClearSpectator));
        let mut chunk_ptrs = Vec::new();
        for _ in 0..3 {
            let (tx, mut rx) = mpsc::unbounded_channel::<EgressMessage>();
            let sender = PlayerSender::Tcp(TcpEgress::new(tx));
            sender.send_reliable(frame.clone()).await.unwrap();
            let EgressMessage::Reliable(received) = rx.try_recv().unwrap() else {
                panic!("expected a Reliable envelope");
            };
            chunk_ptrs.push(received.as_ptr());
        }
        assert!(
            chunk_ptrs.iter().all(|p| *p == chunk_ptrs[0]),
            "broadcast clones must share the frame's allocation, got {chunk_ptrs:?}"
        );
    }

    /// Slow-client soak over the TCP path: with a stalled reader, delta
    /// memory stays bounded at the ring depth, reliable GameOver/Error
    /// messages are delivered after the stall clears, and the client resumes
    /// with the latest delta. The drain task (the exact `tcp_egress_loop`
    /// `handle_tcp_client` spawns) is gated behind a hand-off so the stall is
    /// deterministic: all 512 deltas are pushed while the drain is idle, so
    /// ring eviction runs to completion before the invariant is checked.
    /// Invisible to the headless stress harness (send = 0 ms there), so this
    /// test is the acceptance gate.
    #[tokio::test]
    async fn tcp_slow_client_soak_bounded_and_recovers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        let (_stalled_reader, writer) = tokio::io::split(stream);

        let (tx, rx) = mpsc::unbounded_channel::<EgressMessage>();
        let egress = TcpEgress::new(tx);
        let ring = Arc::clone(&egress.ring);
        let sender = PlayerSender::Tcp(egress);

        // Stalled: the client never reads, and the drain task waits for the
        // hand-off, so the ring takes the full stall.
        let pending: PendingEgress = Arc::new(std::sync::Mutex::new(None));
        let pending_drain = Arc::clone(&pending);
        tokio::spawn(async move {
            gated_tcp_egress_loop(pending_drain, writer).await;
        });

        let (last_delta, error_msg, game_over) = push_soak_traffic(&sender, &ring).await;
        drop(sender);

        // Stall clears: hand the channel + ring to the drain, then read until
        // the reliable GameOver arrives.
        *pending.lock().unwrap() = Some((rx, ring.clone()));
        let mut received = Vec::new();
        read_until(&mut client, &game_over, &mut received).await;
        assert_resumed(&received, &last_delta, &error_msg, &game_over);
    }

    /// Slow-client soak over the WS bridge path — the same drain loop
    /// handle_ws_game spawns, against a real axum WebSocket upgrade. The
    /// drain is gated behind the test's hand-off, mirroring the TCP path.
    #[tokio::test]
    async fn ws_slow_client_soak_bounded_and_recovers() {
        let (tx, rx) = mpsc::unbounded_channel::<EgressMessage>();
        let egress = TcpEgress::new(tx);
        let ring = Arc::clone(&egress.ring);
        let sender = PlayerSender::Tcp(egress);

        // Starts empty: the gated drain waits for the test's hand-off.
        let pending: PendingEgress = Arc::new(std::sync::Mutex::new(None));
        let app = axum::Router::new()
            .route("/ws", get(test_ws_upgrade))
            .with_state(pending.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client
            .write_all(
                b"GET /ws HTTP/1.1\r\n\
                  Host: localhost\r\n\
                  Upgrade: websocket\r\n\
                  Connection: Upgrade\r\n\
                  Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                  Sec-WebSocket-Version: 13\r\n\
                  \r\n",
            )
            .await
            .unwrap();
        // Stalled: the client never reads. Push a stall's worth of traffic.
        let (last_delta, error_msg, game_over) = push_soak_traffic(&sender, &ring).await;
        drop(sender);

        // Stall clears: hand the channel + ring to the gated drain, then read
        // until the reliable GameOver arrives.
        *pending.lock().unwrap() = Some((rx, ring.clone()));
        let mut raw = Vec::new();
        read_until(&mut client, &game_over, &mut raw).await;
        let header_end = find(&raw, b"\r\n\r\n").expect("101 upgrade header") + 4;
        let payload = parse_ws_frames(&raw[header_end..]).expect("ws frames parse");
        assert_resumed(&payload, &last_delta, &error_msg, &game_over);
    }

    async fn test_ws_upgrade(
        ws: WebSocketUpgrade,
        State(pending): State<PendingEgress>,
    ) -> impl axum::response::IntoResponse {
        ws.on_upgrade(move |socket| async move {
            let (ws_sink, _ws_stream) = socket.split();
            let sink = Arc::new(tokio::sync::Mutex::new(ws_sink));
            // Gated: waits for the test's hand-off, then runs exactly the
            // drain task handle_ws_game spawns.
            tokio::spawn(gated_ws_egress_loop(pending, sink));
        })
    }
}
