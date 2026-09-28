//! `LobbyMessage::StartGame` handler, extracted verbatim from the
//! `LobbyMessage::StartGame` arm of
//! [`crate::server::handlers::lobby::handle_client_message`]: lobby
//! lookup + creator check, custom-settings validation, unit-type
//! conversion, the atomic start CLAIM (lobby consumption at guard time —
//! see the claim comment in `start_game`), player-name storage
//! registration, player_map remap for all lobby connections (BEFORE the
//! `GameRoom::new` spawn, so post-spawn commands always see in-game ids),
//! the room spawn, and the ClearSpectator/LobbyUpdate broadcast.

use bytes::Bytes;
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::config::Config;
use crate::game::game::GameSettings;
use crate::map::map::Map;
use crate::map::map_config::load_default_map;
use crate::map::map_registry;
use crate::server::game_server::GameServer;
use crate::server::protocol::{
    encode_message_result, CborEncodeError, LobbySettings, ServerMessage,
};
use crate::server::room::settings::{
    build_default_unit_types, convert_custom_unit_types, convert_unit_types,
    validate_custom_settings,
};
use crate::server::room::{GameRoom, LlmPlayerConfig, PlayerConnection, PlayerSender};

/// Failure of the atomic claim scope (`start_game`).
enum ClaimFail {
    /// The lobby had no players: legacy silent no-op (the pre-refactor code
    /// just returned without sending any message).
    Empty,
    /// Reject the caller with an `Error` carrying this message.
    Reject(String),
}

/// Everything the atomic claim scope hands out: the built start inputs, the
/// lobby roster/senders captured BEFORE consumption (the post-spawn
/// broadcast needs them after the lobby is gone), and the claim itself
/// (the lobby was removed under the lock).
struct ClaimedStart {
    game_settings: GameSettings,
    map: Map,
    human_players: Vec<PlayerConnection>,
    llm_configs: Vec<LlmPlayerConfig>,
    lobby_code: String,
    settings: LobbySettings,
    config_clone: Config,
    lobby_players: Vec<(String, String)>,
    spectator_vision: bool,
    lobby_player_senders: Vec<PlayerSender>,
    all_players: Vec<crate::server::protocol::LobbyPlayer>,
}

/// Start the game for the caller's lobby (creator only).
///
/// The body is the former `LobbyMessage::StartGame` arm of
/// [`crate::server::handlers::lobby::handle_client_message`] verbatim. The
/// early `return Ok(())` exits here; the call site propagates the result
/// with `.await?`, so observable behavior is unchanged.
pub(crate) async fn start_game(
    conn_id: &Uuid,
    server_state: Arc<Mutex<GameServer>>,
    player_sender: &PlayerSender,
) -> Result<(), CborEncodeError> {
    // Only the lobby creator can start the game
    let lobby_code = {
        let server = server_state.lock().await;
        let player_map = server.player_map.lock().await;
        player_map.get(conn_id).map(|(_, code)| code.clone())
    };
    let Some(lobby_code) = lobby_code else {
        let resp = ServerMessage::Error {
            message: "Not in a lobby".to_string(),
        };
        let encoded = encode_message_result(&resp)?;
        let _ = player_sender.send_reliable(encoded.into()).await;
        return Ok(());
    };
    let is_creator = {
        let server = server_state.lock().await;
        let player_map = server.player_map.lock().await;
        let Some((player_id, _)) = player_map.get(conn_id) else {
            return Ok(());
        };
        let lobby_manager = server.lobby_manager.lock().await;
        lobby_manager
            .get_lobby(&lobby_code)
            .map(|l| l.creator == *player_id)
            .unwrap_or(false)
    };
    if !is_creator {
        let resp = ServerMessage::Error {
            message: "Only the lobby creator can start the game".to_string(),
        };
        let encoded = encode_message_result(&resp)?;
        let _ = player_sender.send_reliable(encoded.into()).await;
        return Ok(());
    }

    // ── Atomic start claim (Bug #3: the double-StartGame race) ─────────
    //
    // The old flow checked `games.contains_key(&lobby_code)` in a SEPARATE
    // lock scope after the data-extraction scope, and the room insertion
    // happened in a fire-and-forget `tokio::spawn` — so two concurrent
    // StartGame messages for the same lobby (a double-clicked "Start"
    // button, or a retransmitted lobby message) could BOTH pass the guard
    // before either spawn ran, and each would insert a room: two tick
    // loops, two replay files, and two sets of player_map entries for one
    // lobby.
    //
    // The claim closes that window: inside ONE server-lock scope we
    // (a) read the lobby, (b) check `games` for a running room, and
    // (c) REMOVE the lobby from the lobby manager. All three run under
    // the `server_state` + `lobby_manager` (+ `games`) locks, and no
    // other start_game can reach (a) without passing through them, so
    // check-and-claim is atomic. A second concurrent StartGame therefore
    // observes either the `games` entry (if the first room was already
    // inserted) or — in the normal race — a missing lobby, and is
    // rejected with a clear Error. The room is only spawned after a
    // successful claim.
    //
    // Failure semantics: the claim is NOT rolled back if a later start
    // step fails (e.g. the DB mutex is poisoned during the player-name
    // registration). The lobby is already consumed, so the starter is
    // rejected and the players create a fresh lobby. That is the simpler
    // safe choice: re-queueing the lobby would require restoring both the
    // lobby entry and every member's player_map entry — re-opening the
    // race window — for a path that only fires on a poisoned DB mutex.
    //
    // (Implemented as an `async` block so the `return Err(...)` exits below
    // leave the block — and drop the server lock — instead of the function.)
    let claimed: Result<ClaimedStart, ClaimFail> = async {
        let server = server_state.lock().await;
        let mut lobby_manager = server.lobby_manager.lock().await;
        let lobby = match lobby_manager.get_lobby(&lobby_code) {
            Some(l) => l,
            None => {
                // Also the loser of a concurrent start race: the winner
                // removed the lobby under these same locks, so a second
                // StartGame for the same lobby finds nothing to start.
                return Err(ClaimFail::Reject("Lobby not found".to_string()));
            }
        };
        if !lobby.players.is_empty() {
            // G-3: Validate custom ruleset settings. Custom-ruleset lobbies
            // (use_standard_ruleset == false) are validated in full (lobby-level
            // fields + unit types). A standard-ruleset lobby is validated too
            // when the client still sent custom_unit_types: those types are
            // honored once the server's standard ruleset is disabled, so
            // validating them "when present" closes the flag-bypass (item 7a).
            if !lobby.settings.use_standard_ruleset || !lobby.settings.custom_unit_types.is_empty()
            {
                if let Some(err) = validate_custom_settings(&lobby.settings) {
                    return Err(ClaimFail::Reject(err));
                }
            }

            let unit_types =
                if lobby.settings.use_standard_ruleset && server.config.standard_ruleset.enabled {
                    convert_unit_types(&server.config.standard_ruleset.unit_types)
                } else if !lobby.custom_unit_types.is_empty() {
                    // Convert custom unit types from lobby
                    convert_custom_unit_types(&lobby.custom_unit_types)
                } else {
                    build_default_unit_types()
                };

            let settings = lobby.settings.clone();
            // CityUpgradeSettings is now a single shared type (config.rs),
            // so no protocol->config conversion is needed.
            // Standard ruleset: pull all defaults from config.toml.
            // Custom ruleset: use client-sent values.
            let std = &server.config.standard_ruleset;
            let game_settings = GameSettings {
                starting_funds: if settings.use_standard_ruleset {
                    std.starting_funds
                } else {
                    settings.starting_funds
                },
                initial_manpower: if settings.use_standard_ruleset {
                    std.initial_manpower
                } else {
                    settings.initial_manpower
                },
                funds_per_city: if settings.use_standard_ruleset {
                    std.funds_per_city
                } else {
                    settings.funds_per_city
                },
                recruitment_rate: if settings.use_standard_ruleset {
                    std.recruitment_rate
                } else {
                    settings.recruitment_rate
                },
                game_speed: if settings.use_standard_ruleset {
                    std.game_speed
                } else {
                    settings.game_speed
                },
                use_standard_ruleset: settings.use_standard_ruleset,
                unit_types,
                city_upgrade_settings: if settings.use_standard_ruleset {
                    server.config.city_upgrade_settings()
                } else {
                    settings.city_upgrade_settings.clone()
                },
                water_speed: if settings.use_standard_ruleset {
                    std.water_speed
                } else {
                    settings.water_speed
                },
                influence: if settings.use_standard_ruleset {
                    std.influence.clone().unwrap_or_default()
                } else {
                    crate::game::game::InfluenceSettings::default()
                },
            };
            let map = map_registry::get(&settings.map).unwrap_or_else(load_default_map);
            let mut human_players: Vec<PlayerConnection> = Vec::new();
            let mut llm_configs: Vec<LlmPlayerConfig> = Vec::new();

            for (i, p) in lobby.players.iter().enumerate() {
                let player_id = format!("player_{}", i);
                if p.is_human {
                    human_players.push(PlayerConnection::Player {
                        id: player_id,
                        tx: p.connection.clone(),
                    });
                } else {
                    let llm_provider = p.llm_provider.clone().unwrap_or_default();
                    let thinking_delay = lobby.settings.llm_thinking_delay;

                    // Use the per-slot config if available, otherwise fall back to server config
                    let (model, api_key, base_url) =
                        if let Some(config) = lobby.llm_agent_configs.get(&(i as u32)) {
                            (
                                config.model.clone(),
                                config.api_key.clone(),
                                config.base_url.clone(),
                            )
                        } else {
                            // Fall back to first matching global config provider
                            let provider_config = server
                                .config
                                .llm
                                .providers
                                .iter()
                                .find(|c| c.provider == llm_provider || c.name == llm_provider);
                            (
                                provider_config
                                    .map(|c| c.model.clone())
                                    .unwrap_or_else(|| "llama3".to_string()),
                                provider_config.and_then(|c| c.api_key.clone()),
                                provider_config.and_then(|c| c.base_url.clone()),
                            )
                        };

                    llm_configs.push(LlmPlayerConfig {
                        player_id,
                        player_name: p.name.clone(),
                        provider: llm_provider,
                        model,
                        api_key,
                        base_url,
                        thinking_delay,
                    });
                }
            }

            let lobby_code = lobby.code.clone();
            let settings = lobby.settings.clone();
            let lobby_players: Vec<_> = lobby
                .players
                .iter()
                .map(|p| (p.id.clone(), p.name.clone()))
                .collect();
            let spectator_vision = settings.spectator_vision;

            // Capture the lobby's player connections and the all-players
            // list BEFORE removing the lobby, so the broadcast after the
            // spawn has targets even though the lobby is consumed.
            let lobby_player_senders: Vec<_> =
                lobby.players.iter().map(|p| p.connection.clone()).collect();
            let all_players: Vec<crate::server::protocol::LobbyPlayer> = lobby
                .players
                .iter()
                .map(|p| crate::server::protocol::LobbyPlayer {
                    id: p.id.clone(),
                    name: p.name.clone(),
                    is_human: p.is_human,
                    llm_provider: p.llm_provider.clone(),
                })
                .collect();

            // ── the claim ──
            // `games` is the source of truth (keyed by the same lobby code
            // used when spawning the GameRoom); check it under the SAME
            // locks as the removal so check-and-claim is atomic. Lock
            // order server → lobby_manager → games matches every other
            // path in this module.
            let games = server.games.lock().await;
            if games.contains_key(&lobby_code) {
                return Err(ClaimFail::Reject(
                    "A game is already running for this lobby".to_string(),
                ));
            }
            // Consume the lobby so no other StartGame can find it — this is
            // what makes a concurrent duplicate fail the claim.
            lobby_manager.remove_lobby(&lobby_code);

            Ok(ClaimedStart {
                game_settings,
                map,
                human_players,
                llm_configs,
                lobby_code,
                settings,
                config_clone: server.config.clone(),
                lobby_players,
                spectator_vision,
                lobby_player_senders,
                all_players,
            })
        } else {
            Err(ClaimFail::Empty)
        }
    }
    .await;
    let claimed = match claimed {
        Ok(claimed) => claimed,
        // Empty lobby: legacy silent no-op (no message, as before).
        Err(ClaimFail::Empty) => return Ok(()),
        Err(ClaimFail::Reject(message)) => {
            let resp = ServerMessage::Error { message };
            let encoded = encode_message_result(&resp)?;
            let _ = player_sender.send_reliable(encoded.into()).await;
            return Ok(());
        }
    };
    // Register player display names in the database for leaderboard
    // display (parameterized — no SQL injection). The sync core runs on
    // the blocking pool (Arch #12): the std DB mutex is held on a worker
    // thread, never by this async task. A poisoned DB mutex aborts the
    // start exactly as before — the lobby is already claimed, so the
    // starter is rejected and creates a fresh lobby (see the claim
    // comment above).
    let mut names: Vec<(String, String)> = Vec::new();
    for player in &claimed.human_players {
        let player_id = player.id().cloned().unwrap_or_default();
        let name = claimed
            .lobby_players
            .iter()
            .find(|(id, _)| id == &player_id)
            .map(|(_, name)| name.clone())
            .unwrap_or_else(|| format!("Player {}", player_id));
        names.push((player_id, name));
    }
    for config in &claimed.llm_configs {
        names.push((config.player_id.clone(), config.player_name.clone()));
    }
    {
        let storage = server_state.lock().await.storage.clone();
        if let Err(e) = storage.upsert_player_names_async(&names).await {
            tracing::error!(%e, "Failed to acquire DB lock for player_names");
            return Ok(());
        }
    }
    // Remap every lobby member's player_map entry to the in-game slot IDs
    // minted above: slot i == lobby.players[i] => "player_{i}" (identical
    // ordering to the mint loop). Each connection keeps its room code; a
    // member whose connection is gone has no entry and is skipped.
    //
    // BUG #5: this remap now happens BEFORE the GameRoom spawn (it used to
    // run after it). The spawn is fire-and-forget, so the old order left a
    // window in which the room's tick loop was already consuming commands
    // while lobby members' player_map entries still held their lobby-era
    // ids (player_<uuid8>) — commands arriving in that window were
    // silently dropped/rejected by the engine's ownership gate. Remapping
    // in a single lock scope before the spawn means every command that
    // reaches the room after the spawn always sees in-game ids. (Previously
    // only the starter was remapped to "player_0", so players 2..N kept
    // their lobby-era IDs and the engine's ownership gate silently dropped
    // every command they sent.)
    {
        let server = server_state.lock().await;
        let mut player_map = server.player_map.lock().await;
        for entry in player_map.values_mut() {
            if let Some(slot) = claimed
                .lobby_players
                .iter()
                .position(|(id, _)| id == &entry.0)
            {
                entry.0 = format!("player_{}", slot);
                entry.1 = claimed.lobby_code.clone();
            }
        }
    }
    // Spawn the game with human and LLM players (only reachable after a
    // successful claim — see the claim comment above).
    let server_state_inner = server_state.clone();
    let lobby_code_clone = claimed.lobby_code.clone();
    let map_clone = claimed.map;
    let map_name_clone = claimed.settings.map.clone();
    let game_settings_clone = claimed.game_settings;
    let human_players_clone = claimed.human_players;
    let llm_configs_clone = claimed.llm_configs;
    let config = claimed.config_clone;
    let spectator_vision_clone = claimed.spectator_vision;
    tokio::spawn(async move {
        let server_inner = server_state_inner.lock().await;
        let mut games = server_inner.games.lock().await;
        let room = GameRoom::new(
            lobby_code_clone.clone(),
            map_clone,
            map_name_clone,
            game_settings_clone,
            human_players_clone,
            llm_configs_clone,
            lobby_code_clone.clone(),
            Arc::clone(&server_state_inner),
            config,
            server_inner.storage.clone(),
            spectator_vision_clone,
        );
        let room_arc = Arc::new(Mutex::new(room));
        games.insert(lobby_code_clone, room_arc);
    });
    // Broadcast ClearSpectator then LobbyUpdate (targets were captured in
    // the claim scope, before the lobby was consumed).
    let clear_resp = ServerMessage::ClearSpectator;
    let clear_encoded = Bytes::from(encode_message_result(&clear_resp)?);
    let lobby_update_resp = ServerMessage::LobbyUpdate {
        players: claimed.all_players.clone(),
        settings: claimed.settings.clone(),
    };
    let lobby_encoded = Bytes::from(encode_message_result(&lobby_update_resp)?);
    for sender in &claimed.lobby_player_senders {
        let _ = sender.send_reliable(clear_encoded.clone()).await;
        let _ = sender.send_reliable(lobby_encoded.clone()).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CityUpgradeSettings, Config};
    use crate::server::game_server::GameServer;
    use crate::server::protocol::{LobbySettings, ServerMessage};
    use crate::server::room::{EgressMessage, PlayerSender, TcpEgress};
    use std::time::Duration;
    use tokio::sync::mpsc;

    /// A server whose DB + replay dir live in a unique temp dir (so the
    /// test never touches `data/`). `GameServer::new` also initializes the
    /// map registry (the "default" map the start flow falls back to).
    async fn make_server() -> (Arc<Mutex<GameServer>>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("wod-start-test-{}", Uuid::new_v4()));
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
        let server = GameServer::new(cfg, 0)
            .await
            .expect("server boots (temp storage)");
        (Arc::new(Mutex::new(server)), dir)
    }

    fn test_lobby_settings() -> LobbySettings {
        LobbySettings {
            map: "default".into(),
            starting_funds: 1000,
            initial_manpower: 1000,
            funds_per_city: 10.0,
            game_speed: 1.0,
            use_standard_ruleset: true,
            llm_thinking_delay: 1.0,
            max_players: 4,
            custom_unit_types: vec![],
            spectator_vision: false,
            recruitment_rate: 10.0,
            water_speed: 1.2,
            city_upgrade_settings: CityUpgradeSettings::default(),
        }
    }

    /// One test connection: a `player_map` entry (keyed by `conn_id`,
    /// pointing at lobby `code` with `player_id`) plus a live egress
    /// channel the start flow can broadcast to.
    struct TestConn {
        conn_id: Uuid,
        sender: PlayerSender,
        rx: mpsc::UnboundedReceiver<EgressMessage>,
    }

    async fn make_conn(state: &Arc<Mutex<GameServer>>, player_id: &str, code: String) -> TestConn {
        let (tx, rx) = mpsc::unbounded_channel::<EgressMessage>();
        let sender = PlayerSender::Tcp(TcpEgress::new(tx));
        let conn_id = Uuid::new_v4();
        state
            .lock()
            .await
            .player_map
            .lock()
            .await
            .insert(conn_id, (player_id.to_string(), code));
        TestConn {
            conn_id,
            sender,
            rx,
        }
    }

    /// Create a creator-only lobby and return its code (members are added
    /// by `join_lobby`).
    async fn make_lobby(state: &Arc<Mutex<GameServer>>) -> String {
        let (creator_tx, _creator_rx) = mpsc::unbounded_channel::<EgressMessage>();
        state.lock().await.lobby_manager.lock().await.create_lobby(
            "creator".into(),
            "Creator".into(),
            test_lobby_settings(),
            None,
            PlayerSender::Tcp(TcpEgress::new(creator_tx)),
            vec![],
        )
    }

    /// Add a member to the lobby (explicit player id, as the join flow does).
    async fn join_lobby(state: &Arc<Mutex<GameServer>>, code: &str, player_id: &str, name: &str) {
        let (tx, _rx) = mpsc::unbounded_channel::<EgressMessage>();
        state
            .lock()
            .await
            .lobby_manager
            .lock()
            .await
            .join_lobby(
                code,
                player_id.into(),
                name.into(),
                None,
                PlayerSender::Tcp(TcpEgress::new(tx)),
            )
            .expect("member joins lobby");
    }

    /// Wait until exactly one room has been inserted into `games` (the room
    /// spawn inside `start_game` is fire-and-forget).
    async fn wait_for_room(state: &Arc<Mutex<GameServer>>) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let server = state.lock().await;
                let games = server.games.lock().await;
                let done = games.len() == 1;
                drop(games);
                drop(server);
                if done {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("a single room must appear within 10s");
    }

    /// Decode every `Error { message }` frame currently queued on an egress
    /// channel.
    fn egress_error_messages(rx: &mut mpsc::UnboundedReceiver<EgressMessage>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(EgressMessage::Reliable(data)) = rx.try_recv() {
            if let Ok(ServerMessage::Error { message }) =
                ciborium::from_reader::<ServerMessage, _>(&data[..])
            {
                out.push(message);
            }
        }
        out
    }

    /// Bug #3 regression: two concurrent StartGame messages for the same
    /// lobby must spawn EXACTLY ONE room. The atomic claim (lobby removal
    /// under the guard) means the loser finds no lobby (or, if the winner
    /// already inserted its room, a running game) and is rejected with a
    /// clear Error — never a second room.
    #[tokio::test]
    async fn concurrent_start_game_spawns_exactly_one_room() {
        let (state, dir) = make_server().await;
        let lobby_code = make_lobby(&state).await;
        join_lobby(&state, &lobby_code, "member1", "Member 1").await;
        join_lobby(&state, &lobby_code, "member2", "Member 2").await;

        // Two connections that both pass the creator check (both map to the
        // creator's player id in player_map) — the claim decides the winner.
        let mut creator_a = make_conn(&state, "creator", lobby_code.clone()).await;
        let mut creator_b = make_conn(&state, "creator", lobby_code.clone()).await;

        let (r_a, r_b) = tokio::join!(
            start_game(&creator_a.conn_id, state.clone(), &creator_a.sender),
            start_game(&creator_b.conn_id, state.clone(), &creator_b.sender),
        );
        r_a.expect("winner start_game Ok");
        r_b.expect("loser start_game Ok (rejects via message, not error)");

        wait_for_room(&state).await;

        // Exactly one room, keyed by the lobby code.
        let server = state.lock().await;
        let games = server.games.lock().await;
        assert_eq!(games.len(), 1, "double StartGame must not double the room");
        assert!(games.contains_key(&lobby_code), "room keyed by lobby code");
        drop(games);
        drop(server);

        // The loser must have received a clear rejection Error. (Any of the
        // three rejection reasons is correct: "Lobby not found" if it lost
        // the claim, "A game is already running..." if the winner's room was
        // already inserted, or "Only the lobby creator..." if the winner's
        // pre-spawn remap rewrote its player_map entry first.)
        let mut err_msgs = Vec::new();
        err_msgs.extend(egress_error_messages(&mut creator_a.rx));
        err_msgs.extend(egress_error_messages(&mut creator_b.rx));
        assert!(
            err_msgs.iter().any(|m| {
                m == "Lobby not found"
                    || m == "A game is already running for this lobby"
                    || m == "Only the lobby creator can start the game"
            }),
            "the losing StartGame must be rejected with a clear Error, got: {err_msgs:?}"
        );

        // Drop the room so the background tick task terminates (channel
        // close).
        state.lock().await.games.lock().await.clear();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Bug #5 regression: every lobby member's player_map entry must be
    /// remapped to the in-game slot id ("player_{i}" by lobby order) BEFORE
    /// the GameRoom spawn — so commands arriving after the spawn always
    /// see in-game ids, not lobby-era ids (which the engine's ownership
    /// gate would silently drop).
    #[tokio::test]
    async fn start_game_remaps_player_map_to_in_game_ids() {
        let (state, dir) = make_server().await;
        let lobby_code = make_lobby(&state).await;
        join_lobby(&state, &lobby_code, "member1", "Member 1").await;
        join_lobby(&state, &lobby_code, "member2", "Member 2").await;

        let creator = make_conn(&state, "creator", lobby_code.clone()).await;
        let member1 = make_conn(&state, "member1", lobby_code.clone()).await;
        let member2 = make_conn(&state, "member2", lobby_code.clone()).await;

        start_game(&creator.conn_id, state.clone(), &creator.sender)
            .await
            .expect("start_game Ok");
        // The remap is synchronous inside start_game (before the spawn),
        // so it is already done when we get here. Wait for the room spawn
        // (fire-and-forget) before inspecting/clearing the game map.
        wait_for_room(&state).await;

        let server = state.lock().await;
        let player_map = server.player_map.lock().await;
        assert_eq!(
            player_map.get(&creator.conn_id),
            Some(&("player_0".to_string(), lobby_code.clone())),
            "starter must be remapped to player_0"
        );
        assert_eq!(
            player_map.get(&member1.conn_id),
            Some(&("player_1".to_string(), lobby_code.clone())),
            "member 1 must be remapped to player_1 (not left on its lobby id)"
        );
        assert_eq!(
            player_map.get(&member2.conn_id),
            Some(&("player_2".to_string(), lobby_code.clone())),
            "member 2 must be remapped to player_2 (not left on its lobby id)"
        );
        drop(player_map);
        drop(server);

        // Drop the room so the background tick task terminates (channel
        // close). (Acquire server_state fresh — the tokio Mutex is
        // non-reentrant, so the previous guard must be dropped first.)
        let server = state.lock().await;
        let mut games = server.games.lock().await;
        games.clear();
        drop(games);
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
