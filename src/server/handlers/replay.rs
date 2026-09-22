//! Replay WebSocket handlers.
//!
//! `/ws/replay?game_id=...` streams the RECORDED game — the JSONL file the
//! game loop writes to `{replay_path}/{game_id}.jsonl` — to the viewer.
//! The recording is the viewpoint player's actual wire stream (the same
//! `full_state` / per-tick `delta` payloads their client received live), so
//! this endpoint re-sends those payloads VERBATIM, re-wrapped in the same
//! binary CBOR `ServerMessage` frames the live `/ws` connection speaks. The
//! frontend's `connectReplayGame` (main.ts) consumes this endpoint through
//! the normal read-only `Connection`, so the replay path gets full parity
//! with live play: enum terrain, camera centering, delta handling, fog/
//! border flags.
//!
//! The legacy Rust resimulation (ReplayPlayer + Recorder, src/replay/) is
//! gone: it parsed a move-list format no live writer produced, so the
//! list-UI path always replayed an empty game on a hardcoded default map.
//! Streaming the recording fixes that and makes custom maps work for free —
//! the map lives inside the recorded `full_state` line.
//!
//! Framing (server → client; the viewer never sends):
//!   - BINARY CBOR `ServerMessage` frames, in file order:
//!     1. `FullState` — from the first `"full_state"` line,
//!     2. `DeltaState` — one per `"delta"` line (idle ticks recorded
//!        nothing, exactly as on the wire); the server closes after the
//!        `"game_end"` marker (or EOF).
//!   - on error: a single BINARY `Error { message }` frame, then close —
//!     the frontend treats a pre-handshake `Error` as fatal (no reconnect
//!     hammering) and shows the disconnected overlay.
//!
//! Pacing: one `DeltaState` frame per ~17 ms (60 Hz), matching the old
//! resimulation endpoint's watch speed — the recorded deltas carry no
//! timestamps, so the original tick cadence cannot be restored. The file
//! is read with `tokio::fs`; no blocking thread is pinned for the duration
//! of a replay (reading small JSONL line-by-line is fine async).
//!
//! Note the difference vs `?replay=<file>`: that legacy URL mode fetches
//! the same JSONL over HTTP (`GET /replays/:name`) and feeds the standalone
//! `ReplayPlayer`, which paces client-side (one event per rAF). This WS
//! mode feeds the live `Connection`, which only speaks binary `ServerMessage`
//! frames — hence the JSONL line → `ServerMessage` re-wrap here.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::sync::Mutex;

use crate::server::game_server::GameServer;
use crate::server::protocol::{encode_message, GameDeltaMessage, GameStateMessage, ServerMessage};

/// Basename charset rule for any client-supplied replay identifier
/// (the `game_id` query param on `/ws/replay`, the `:name` path param on
/// `/replays/:name`). Mirrors the frontend's `?replay=` guard: a plain
/// basename — no path separators (forward or back), no null bytes, no `..`
/// sequences (so `..` itself, `../x` and `a/../b` all fail), alphanumeric
/// plus `.`/`_`/`-`, ≤ 128 bytes.
pub(crate) fn is_safe_replay_basename(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Filename rule for the `/replays/:name` route: the basename rule plus a
/// `.jsonl` suffix with a non-empty stem (`.jsonl` alone is rejected).
pub(crate) fn is_safe_replay_file_name(name: &str) -> bool {
    is_safe_replay_basename(name) && name.ends_with(".jsonl") && name.len() > ".jsonl".len()
}

/// Require that `stored_path` (as persisted in `game_metadata.replay_path`)
/// resolves to a file whose parent directory is the canonical replay dir.
/// Returns the path to open, or `None` if it escapes the replay directory
/// (traversal, dangling `..`, or a missing parent). Defense-in-depth on top
/// of the basename rule: the DB is server-written, but a tampered or legacy
/// row must not be able to point the viewer at an arbitrary file.
pub(crate) fn resolve_replay_file(replay_dir: &str, stored_path: &str) -> Option<PathBuf> {
    let dir = Path::new(replay_dir).canonicalize().ok()?;
    let candidate = Path::new(stored_path);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        dir.join(candidate)
    };
    // The file itself may not exist yet; the traversal boundary is its
    // parent. `canonicalize` on the parent collapses any `..` segments.
    let parent = joined.parent()?.canonicalize().ok()?;
    (parent == dir).then_some(joined)
}

/// WebSocket upgrade handler for replay viewers.
pub(crate) async fn replay_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<Mutex<GameServer>>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let game_id = params.get("game_id").cloned();
    let server_state = state.clone();
    ws.on_upgrade(move |socket| handle_replay_ws(socket, server_state, game_id))
}

/// Stream the recorded JSONL for `game_id` to the viewer (see module docs
/// for the frame contract). Entirely async — no `spawn_blocking` for the
/// lifetime of the replay.
pub(crate) async fn handle_replay_ws(
    socket: WebSocket,
    server_state: Arc<Mutex<GameServer>>,
    game_id: Option<String>,
) {
    let (mut ws_sink, _ws_stream) = socket.split();
    // The viewer never sends on this channel — the read half is dropped;
    // the stream loop below exits when a send fails (viewer disconnected).
    drop(_ws_stream);

    // Errors go out as the same binary `ServerMessage` the live path uses —
    // the frontend's Connection decodes CBOR frames and treats a pre-
    // handshake `Error` as fatal (marks the connection as not reconnectable
    // and shows the disconnected overlay).
    async fn send_error(sink: &mut futures::stream::SplitSink<WebSocket, Message>, message: &str) {
        let _ = sink
            .send(Message::Binary(encode_message(&ServerMessage::Error {
                message: message.to_string(),
            })))
            .await;
    }

    // 1) The client-supplied identifier must be a plain basename. This
    //    rejects `..`, absolute paths, null bytes and any separator before
    //    anything touches the DB or the filesystem.
    let game_id = match game_id {
        Some(id) if !id.is_empty() => id,
        _ => {
            send_error(&mut ws_sink, "No game_id provided").await;
            return;
        }
    };
    if !is_safe_replay_basename(&game_id) {
        send_error(&mut ws_sink, "Invalid game_id").await;
        return;
    }

    // 2) Resolve the recorded file through the DB (parameterized query),
    //    then verify the stored path stays inside the replay directory.
    //    Clone `Storage` out of the server guard first so the guard is
    //    dropped before we hold the inner (std) DB mutex.
    let storage = server_state.lock().await.storage.clone();
    let stored: Option<String> = match storage.db.lock() {
        Ok(db) => match db.prepare("SELECT replay_path FROM game_metadata WHERE id = ?") {
            Ok(mut stmt) => {
                use sqlite::Value;
                let params = [Value::String(game_id.clone())];
                if stmt.bind(params.as_slice()).is_err() {
                    tracing::error!(game_id = %game_id, "Failed to bind game_id");
                    None
                } else if matches!(stmt.next(), Ok(sqlite::State::Row)) {
                    stmt.read(0).ok().filter(|p: &String| !p.is_empty())
                } else {
                    None
                }
            }
            Err(e) => {
                tracing::error!(%e, game_id = %game_id, "Failed to prepare replay lookup");
                None
            }
        },
        Err(e) => {
            tracing::error!(%e, "Failed to acquire DB lock for replay lookup");
            None
        }
    };

    let file_path = {
        let server = server_state.lock().await;
        let dir = server.config.storage.replay_path.clone();
        match stored {
            None => {
                send_error(&mut ws_sink, "Replay not found").await;
                return;
            }
            Some(stored) => match resolve_replay_file(&dir, &stored) {
                Some(p) => p,
                None => {
                    tracing::warn!(
                        game_id = %game_id,
                        "Rejected replay path outside the replay dir"
                    );
                    send_error(&mut ws_sink, "Replay not found").await;
                    return;
                }
            },
        }
    };

    // 3) Stream the file: each recorded line is the payload the viewpoint
    //    client actually received on the wire — re-wrap it in the same
    //    binary `ServerMessage` frame type and send it. `full_state` lines
    //    become `FullState`, `delta` lines become `DeltaState`, and the
    //    terminal `game_end` marker ends the stream. Unknown line shapes
    //    are skipped (forward compatibility with future recorder events).
    let file = match tokio::fs::File::open(&file_path).await {
        Ok(f) => f,
        Err(_) => {
            send_error(&mut ws_sink, "Replay not found").await;
            return;
        }
    };
    let mut reader = tokio::io::BufReader::new(file);
    let mut line = String::new();
    let mut sent_state = false;
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let value: serde_json::Value = match serde_json::from_str(trimmed) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(
                            game_id = %game_id,
                            %e,
                            "Skipping malformed replay line"
                        );
                        continue;
                    }
                };
                let frame: Option<ServerMessage> = if let Some(gs) = value.get("full_state") {
                    match serde_json::from_value::<GameStateMessage>(gs.clone()) {
                        Ok(state) => {
                            sent_state = true;
                            Some(ServerMessage::FullState(state))
                        }
                        Err(e) => {
                            tracing::warn!(game_id = %game_id, %e, "Bad full_state line");
                            None
                        }
                    }
                } else if let Some(d) = value.get("delta") {
                    match serde_json::from_value::<GameDeltaMessage>(d.clone()) {
                        Ok(delta) => Some(ServerMessage::DeltaState(delta)),
                        Err(e) => {
                            tracing::warn!(game_id = %game_id, %e, "Bad delta line");
                            None
                        }
                    }
                } else if value.get("event").and_then(|e| e.as_str()) == Some("game_end") {
                    // Terminal marker — the client already holds the final
                    // state after the last recorded delta.
                    break;
                } else {
                    None // unknown event type — skip
                };
                if let Some(msg) = frame {
                    if ws_sink
                        .send(Message::Binary(encode_message(&msg)))
                        .await
                        .is_err()
                    {
                        return; // viewer disconnected
                    }
                    // DeltaState frames are paced (the recorded stream has no
                    // timestamps); the initial FullState goes out immediately.
                    if let ServerMessage::DeltaState(_) = &msg {
                        tokio::time::sleep(Duration::from_millis(17)).await;
                    }
                }
            }
            Err(e) => {
                tracing::error!(%e, game_id = %game_id, "Failed reading replay file");
                send_error(&mut ws_sink, "Failed to read replay").await;
                return;
            }
        }
    }
    if !sent_state {
        // A recording without an initial full state would leave the viewer
        // with an empty board — surface it instead of a blank canvas.
        send_error(&mut ws_sink, "Replay not found").await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basename_rule_accepts_plain_recording_names() {
        assert!(is_safe_replay_basename("abc123"));
        assert!(is_safe_replay_basename("a-b_c.d.jsonl"));
        assert!(is_safe_replay_basename("game-42.jsonl"));
    }

    #[test]
    fn basename_rule_rejects_traversal_and_separators() {
        assert!(!is_safe_replay_basename(""));
        assert!(!is_safe_replay_basename(".."));
        assert!(!is_safe_replay_basename("../config.toml"));
        assert!(!is_safe_replay_basename("a/../b.jsonl"));
        assert!(!is_safe_replay_basename("a/b.jsonl"));
        assert!(!is_safe_replay_basename("a\\b.jsonl"));
        assert!(!is_safe_replay_basename("/abs.jsonl"));
        assert!(!is_safe_replay_basename("bad\0name.jsonl"));
        assert!(!is_safe_replay_basename(&"x".repeat(129)));
    }

    #[test]
    fn file_name_rule_requires_jsonl_suffix_with_stem() {
        assert!(is_safe_replay_file_name("game.jsonl"));
        assert!(!is_safe_replay_file_name(".jsonl"));
        assert!(!is_safe_replay_file_name("game.toml"));
        assert!(!is_safe_replay_file_name("../evil.jsonl"));
    }

    #[test]
    fn resolve_replay_file_keeps_paths_inside_the_replay_dir() {
        let dir = std::env::temp_dir().join(format!("wod-replay-resolve-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let replay_dir = dir.join("replays");
        std::fs::create_dir_all(&replay_dir).unwrap();
        let replay_dir = replay_dir.to_str().unwrap().to_string();

        // Same-dir relative and absolute paths are fine (the file need not
        // exist yet — the boundary is its parent).
        let ok_rel = resolve_replay_file(&replay_dir, "game-42.jsonl");
        assert!(ok_rel.is_some(), "plain name must resolve");
        assert_eq!(
            ok_rel.as_ref().unwrap().parent().unwrap(),
            Path::new(&replay_dir).canonicalize().unwrap()
        );
        let ok_abs = resolve_replay_file(&replay_dir, &format!("{}/game.jsonl", replay_dir));
        assert!(ok_abs.is_some(), "absolute in-dir path must resolve");

        // Traversal variants must be rejected.
        assert_eq!(resolve_replay_file(&replay_dir, "../db.sqlite"), None);
        assert_eq!(resolve_replay_file(&replay_dir, "../../etc/passwd"), None);
        assert_eq!(resolve_replay_file(&replay_dir, "a/../../x.jsonl"), None);
        // Absolute path outside the dir.
        assert_eq!(resolve_replay_file(&replay_dir, "/etc/passwd"), None);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
