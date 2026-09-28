//! Lobby + in-game message dispatch.
//!
//! `lobby_bridge_sender` builds the lobby-facing egress bridge;
//! `handle_client_message` is the central message router — a thin
//! dispatcher over `room::lobby_flow` (lobby arms: create/join/LLM-slot/
//! start) and `room::commands` (game-command arms: recruit/move/toggles/
//! upgrade/advisor/leaderboard/embark) — shared by the TCP and WebSocket
//! transports.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use uuid::Uuid;

use crate::server::game_server::GameServer;
use crate::server::lobby::MAX_PLAYER_NAME_LEN;
use crate::server::protocol::{
    encode_message_result, CborEncodeError, ClientMessage, LobbyMessage, ServerMessage,
};
use crate::server::room::{EgressMessage, PlayerSender, TcpEgress};

/// I7: build the lobby-facing sender from a connection sender. TCP senders
/// share the same channel + keep-latest ring; `None` gets a throwaway channel
/// whose receiver is dropped (sends fail and are ignored at every call site).
pub(crate) fn lobby_bridge_sender(player_sender: &PlayerSender) -> PlayerSender {
    match player_sender {
        PlayerSender::Tcp(e) => PlayerSender::Tcp(e.clone()),
        PlayerSender::None => {
            // No connection — create a throwaway channel
            let (tx, _rx) = mpsc::unbounded_channel::<EgressMessage>();
            PlayerSender::Tcp(TcpEgress::new(tx))
        }
    }
}

/// M1b: per-connection join cooldown. A LAN-scale rate limit with no
/// per-IP infrastructure, keyed by connection UUID: it throttles
/// lobby-code guessing and seat-spam from a single connection (every
/// join attempt re-broadcasts a LobbyUpdate to all members, and an
/// unknown code is otherwise guessable at unlimited attempts/second).
/// `tokio::time::Instant` is tokio's monotonic clock (pausable via
/// `time::pause` if the `test-util` feature is ever enabled; the test
/// below uses a real sleep instead). Entries expire after the cooldown,
/// so the map cannot grow with disconnected connections.
const JOIN_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(2);
static JOIN_ATTEMPTS: std::sync::LazyLock<std::sync::Mutex<HashMap<Uuid, tokio::time::Instant>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// M2: reject a name longer than `MAX_PLAYER_NAME_LEN` characters (per
/// char, not per byte — Unicode names are not amplified). Rejected
/// rather than silently truncated: truncation would desync the client's
/// display from the server-stored identity, and the cap exists to bound
/// the per-join LobbyUpdate re-broadcast, which a silent truncate would
/// not communicate to the sender.
fn name_too_long(name: Option<&str>) -> Option<ServerMessage> {
    let name = name?;
    let len = name.chars().count();
    if len > MAX_PLAYER_NAME_LEN {
        return Some(ServerMessage::Error {
            message: format!("Name too long ({len} characters; max {MAX_PLAYER_NAME_LEN})"),
        });
    }
    None
}

/// M1b: `Some(Error)` when this connection attempted a join within the
/// last `JOIN_COOLDOWN`; otherwise stamp the attempt and return `None`
/// (the stamp covers FAILED attempts too — the rate limit is on
/// attempts, not on successes).
fn join_cooldown_rejection(conn_id: &Uuid) -> Option<ServerMessage> {
    let now = tokio::time::Instant::now();
    let mut attempts = JOIN_ATTEMPTS.lock().unwrap_or_else(|e| e.into_inner());
    // Expired entries are dead weight — prune on every access so the map
    // is bounded by connections active within the cooldown window.
    attempts.retain(|_, t| now.duration_since(*t) < JOIN_COOLDOWN);
    if attempts
        .get(conn_id)
        .is_some_and(|t| now.duration_since(*t) < JOIN_COOLDOWN)
    {
        return Some(ServerMessage::Error {
            message: "Join attempts are rate limited — try again shortly".to_string(),
        });
    }
    attempts.insert(*conn_id, now);
    None
}

/// Guards for the lobby ENTRY points (create/join/LLM-add), enforced in
/// this transport-facing dispatcher rather than in `LobbyManager`:
/// the connection identity (`conn_id`) only exists in this layer, the
/// seat record (`player_map`) lives on `GameServer`, and the lobby
/// flow in `room/lobby_flow.rs` is conn-agnostic by design. A
/// rejection is a plain `ServerMessage::Error` the client can display;
/// the connection stays open.
async fn lobby_entry_guard(
    conn_id: &Uuid,
    server_state: &Arc<Mutex<GameServer>>,
    lobby_msg: &LobbyMessage,
) -> Option<ServerMessage> {
    match lobby_msg {
        LobbyMessage::JoinLobby { player_name, .. } => {
            // Bug #2: one lobby seat per connection. `player_map`
            // (conn_id -> (player_id, lobby code)) is the seat record —
            // inserted on create/join, removed on disconnect — the same
            // source of truth the CreateLobby guard uses. Without this,
            // one connection can sit in N lobbies at once, minting ghost
            // seats that consume cities at game start.
            {
                let server = server_state.lock().await;
                if server.player_map.lock().await.contains_key(conn_id) {
                    return Some(ServerMessage::Error {
                        message: "Already in a lobby — start it or leave before joining another"
                            .to_string(),
                    });
                }
            }
            if let Some(err) = name_too_long(player_name.as_deref()) {
                return Some(err);
            }
            join_cooldown_rejection(conn_id)
        }
        LobbyMessage::CreateLobby { player_name, .. } => {
            // M2: cap the creator's display name (see name_too_long).
            name_too_long(player_name.as_deref())
        }
        LobbyMessage::AddLlmPlayer { name, .. } => {
            // M2: cap the LLM agent's display name (see name_too_long).
            name_too_long(Some(name))
        }
        // SetSlotLlm / SetSlotLlmConfig / StartGame: no entry-point guard
        // needed — the flow already requires a seated (and, for the LLM
        // arms, creator-only) connection.
        _ => None,
    }
}

/// M3: fallible — returns `Err` when a server reply cannot be CBOR-encoded.
/// A truncated frame would desync the peer, so callers close the connection
/// on `Err` (the read loops do exactly that).
pub(crate) async fn handle_client_message(
    conn_id: &Uuid,
    server_state: Arc<Mutex<GameServer>>,
    msg: &ClientMessage,
    player_sender: &PlayerSender,
) -> Result<(), CborEncodeError> {
    match msg {
        // WP3: Hello is only valid as the connection's FIRST message (the
        // read loops gate it before dispatch); a repeated Hello is a
        // protocol violation.
        ClientMessage::Hello { .. } => {
            let resp = ServerMessage::Error {
                message: "Protocol error: Hello is only allowed as the first message".to_string(),
            };
            let encoded = encode_message_result(&resp)?;
            let _ = player_sender.send_reliable(encoded.into()).await;
        }
        ClientMessage::LobbyMessage(lobby_msg) => {
            // M1/M2 entry-point guards (see lobby_entry_guard): run before
            // the lobby flow so a rejected attempt never mints a seat, a
            // broadcast, or a cooldown stamp it should not have.
            if let Some(rejection) = lobby_entry_guard(conn_id, &server_state, lobby_msg).await {
                let encoded = encode_message_result(&rejection)?;
                let _ = player_sender.send_reliable(encoded.into()).await;
                return Ok(());
            }
            // Extracted verbatim into `crate::server::room::lobby_flow`.
            crate::server::room::lobby_flow::handle_lobby_message(
                conn_id,
                server_state,
                lobby_msg,
                player_sender,
            )
            .await?;
        }
        msg => {
            // Game-command arms extracted verbatim into
            // `crate::server::room::commands`.
            crate::server::room::commands::handle_game_command(
                conn_id,
                server_state,
                msg,
                player_sender,
            )
            .await?;
        }
    }
    Ok(())
}

// ────────────────────────────────────────────────────────────────────────
// Entry-point guard tests (Bug #2 seat guard, M1b cooldown, M2 name caps)
// ────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::server::protocol::LobbySettings;

    /// A real GameServer with throwaway storage (unique temp dir per test
    /// so parallel tests never share a sqlite file — same pattern as the
    /// Sec L2 tests in game_server.rs).
    async fn make_server() -> Arc<Mutex<GameServer>> {
        let dir = std::env::temp_dir().join(format!("wod-lobby-guard-{}", Uuid::new_v4()));
        let toml_str = format!(
            r#"
[llm]

[standard_ruleset]
enabled = true
unit_types = []

[storage]
database_path = "{db}"
replay_path = "{replay}"
"#,
            db = dir.join("db.sqlite").display(),
            replay = dir.join("replays").display(),
        );
        let cfg: Config = toml::from_str::<Config>(&toml_str).unwrap().resolve();
        Arc::new(Mutex::new(GameServer::new(cfg, 0).await.unwrap()))
    }

    fn lobby_settings() -> LobbySettings {
        toml::from_str(
            r#"
            map = "default"
            use_standard_ruleset = true
            llm_thinking_delay = 0.0
            "#,
        )
        .expect("lobby settings parse")
    }

    /// One client→server round-trip through the real dispatcher; returns
    /// the first decoded server reply on the connection. The lobby flow
    /// only uses reliable egress, so the first Reliable frame is the
    /// answer (later broadcast frames stay in the channel and are dropped
    /// with the receiver).
    async fn dispatch(
        state: &Arc<Mutex<GameServer>>,
        conn_id: &Uuid,
        msg: &ClientMessage,
    ) -> ServerMessage {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<EgressMessage>();
        let sender = PlayerSender::Tcp(TcpEgress::new(tx));
        handle_client_message(conn_id, state.clone(), msg, &sender)
            .await
            .expect("server reply encodes");
        let data = loop {
            match rx.recv().await.expect("egress open") {
                EgressMessage::Reliable(d) => break d,
                EgressMessage::Delta(_) => {}
                EgressMessage::Close => panic!("unexpected Close on the guard test path"),
            }
        };
        ciborium::from_reader(&data[..]).expect("valid cbor")
    }

    async fn create_lobby(
        state: &Arc<Mutex<GameServer>>,
        conn_id: &Uuid,
        name: Option<&str>,
    ) -> String {
        let msg = ClientMessage::LobbyMessage(LobbyMessage::CreateLobby {
            settings: lobby_settings(),
            password: None,
            player_name: name.map(str::to_string),
        });
        match dispatch(state, conn_id, &msg).await {
            ServerMessage::LobbyCreated { lobby_code } => lobby_code,
            other => panic!("expected LobbyCreated, got {other:?}"),
        }
    }

    async fn join_lobby(
        state: &Arc<Mutex<GameServer>>,
        conn_id: &Uuid,
        code: &str,
        name: Option<&str>,
    ) -> ServerMessage {
        let msg = ClientMessage::LobbyMessage(LobbyMessage::JoinLobby {
            lobby_code: code.to_string(),
            password: None,
            player_name: name.map(str::to_string),
        });
        dispatch(state, conn_id, &msg).await
    }

    /// Bug #2: a connection that already holds a lobby seat (creator or
    /// guest) cannot join another lobby — the second join is rejected with
    /// a clear Error, the seat record is untouched, and a DIFFERENT
    /// connection can still join (the guard is per-connection, not a
    /// global lockout).
    #[tokio::test]
    async fn seated_connection_cannot_join_another_lobby() {
        let state = make_server().await;
        let creator = Uuid::new_v4();
        let code = create_lobby(&state, &creator, None).await;

        // The same connection now tries to join (its own lobby — the guard
        // is about holding ANY seat, not about the target lobby).
        match join_lobby(&state, &creator, &code, None).await {
            ServerMessage::Error { message } => {
                assert!(message.contains("Already in a lobby"), "{message}");
            }
            other => panic!("expected an Error rejection, got {other:?}"),
        }

        // The rejected join minted no seat: exactly one entry in the
        // player_map, and it is the creator's original lobby.
        let seats = state.lock().await.player_map.lock().await.clone();
        assert_eq!(seats.len(), 1, "exactly one seated connection");
        assert_eq!(
            seats.get(&creator).map(|(_, c)| c.as_str()),
            Some(code.as_str())
        );

        // A fresh connection can still join the same lobby.
        let guest = Uuid::new_v4();
        match join_lobby(&state, &guest, &code, Some("Guest")).await {
            ServerMessage::LobbyJoined { lobby_code } => assert_eq!(lobby_code, code),
            other => panic!("a fresh connection must join, got {other:?}"),
        }
    }

    /// M1b: a rapid second join attempt from the same connection is
    /// rejected with a clear "try again shortly" Error; after the cooldown
    /// expires the same connection joins successfully. Real-time test
    /// (tokio's `test-util` feature is not enabled, so `time::pause` is
    /// unavailable): the only non-determinism is the sleep margin — the
    /// reject/accept boundary is a clock comparison against
    /// `JOIN_COOLDOWN`, and the retry sleeps past it with headroom.
    #[tokio::test]
    async fn join_cooldown_rejects_rapid_retry_then_expires() {
        let state = make_server().await;
        let creator = Uuid::new_v4();
        let code = create_lobby(&state, &creator, None).await;

        // A fresh connection's first join attempt (failing — no such
        // lobby) stamps the cooldown.
        let guest = Uuid::new_v4();
        match join_lobby(&state, &guest, "NO-SUCH-CODE", None).await {
            ServerMessage::Error { message } => {
                assert!(message.contains("not found"), "{message}");
            }
            other => panic!("expected Lobby-not-found, got {other:?}"),
        }
        // The immediate retry is rate limited — a distinct, clear error
        // (not "not found").
        match join_lobby(&state, &guest, "NO-SUCH-CODE", None).await {
            ServerMessage::Error { message } => {
                assert!(message.contains("rate limited"), "{message}");
            }
            other => panic!("expected the cooldown rejection, got {other:?}"),
        }

        // Sleep past the cooldown: the same connection can try again, and
        // this time the join into the real lobby succeeds.
        tokio::time::sleep(JOIN_COOLDOWN + std::time::Duration::from_millis(250)).await;
        match join_lobby(&state, &guest, &code, None).await {
            ServerMessage::LobbyJoined { lobby_code } => assert_eq!(lobby_code, code),
            other => panic!("join after the cooldown must succeed, got {other:?}"),
        }
    }

    /// M2: 32-char names are accepted and 33-char names are rejected with
    /// a clear Error at every entry point (create, join, LLM-add) —
    /// never silently truncated.
    #[tokio::test]
    async fn name_caps_accept_32_chars_and_reject_33() {
        let state = make_server().await;
        let name32 = "n".repeat(32);
        let name33 = "n".repeat(33);

        // 32-char creator name: accepted.
        let creator = Uuid::new_v4();
        let code = create_lobby(&state, &creator, Some(&name32)).await;

        // 33-char creator name: rejected.
        let c2 = Uuid::new_v4();
        let msg = ClientMessage::LobbyMessage(LobbyMessage::CreateLobby {
            settings: lobby_settings(),
            password: None,
            player_name: Some(name33.clone()),
        });
        match dispatch(&state, &c2, &msg).await {
            ServerMessage::Error { message } => assert!(message.contains("too long"), "{message}"),
            other => panic!("expected the name rejection, got {other:?}"),
        }

        // 33-char guest name: rejected (fresh connection, no seat).
        let g1 = Uuid::new_v4();
        match join_lobby(&state, &g1, &code, Some(&name33)).await {
            ServerMessage::Error { message } => assert!(message.contains("too long"), "{message}"),
            other => panic!("expected the name rejection, got {other:?}"),
        }

        // 33-char LLM name: rejected on the creator-only endpoint.
        let msg = ClientMessage::LobbyMessage(LobbyMessage::AddLlmPlayer {
            name: name33,
            llm_provider: "ollama".into(),
        });
        match dispatch(&state, &creator, &msg).await {
            ServerMessage::Error { message } => assert!(message.contains("too long"), "{message}"),
            other => panic!("expected the name rejection, got {other:?}"),
        }

        // And the 32-char guest name joins end to end.
        let g2 = Uuid::new_v4();
        match join_lobby(&state, &g2, &code, Some(&name32)).await {
            ServerMessage::LobbyJoined { .. } => {}
            other => panic!("32-char name must join, got {other:?}"),
        }
    }
}
