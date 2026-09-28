//! HTTP route handlers — the "dumb" layer of the server: static file
//! serving (embedded frontend), JSON list endpoints (map registry), the
//! spectator watch page, and the game-metadata settings JSON used by the
//! room setup (`game_metadata_settings_json`). Game state mutation lives in
//! `room/lobby_flow.rs` / `room/start.rs` — this module only reads.

use std::sync::Arc;

use axum::extract::State;
use axum::response::Html;
use include_dir::{include_dir, Dir};
use tokio::sync::Mutex;

use crate::game::game::Game;
use crate::map::map_registry;
use crate::server::GameServer;

static FRONTEND: Dir = include_dir!("$CARGO_MANIFEST_DIR/frontend/dist");

/// Serve the index.html page
pub(crate) async fn index_handler() -> Html<String> {
    let index_html = FRONTEND
        .get_file("index.html")
        .unwrap()
        .contents_utf8()
        .unwrap();
    Html(index_html.to_string())
}

/// Spectator watch page — lists ACTIVE games (id + live player count)
/// with a "Watch" hook per game that opens the live spectator WebSocket
/// (`/ws/spectator`, see `handlers/spectator.rs`). The server-side
/// spectator handler joins the first active game room, so the hook
/// connects to the single spectator endpoint (the game id is carried for
/// display and future per-game targeting).
///
/// Replaces the old static stub that showed a lone "Connect" button and
/// never listed a real game.
pub(crate) async fn watch_handler(State(state): State<Arc<Mutex<GameServer>>>) -> Html<String> {
    // Collect (game_id, player_count) pairs — player count is the number
    // of player_map entries whose room code is the game id. Both reads
    // happen under the server lock, so the snapshot is consistent.
    let games: Vec<(String, usize)> = {
        let server = state.lock().await;
        let games_map = server.games.lock().await;
        let player_map = server.player_map.lock().await;
        games_map
            .keys()
            .map(|game_id| {
                let count = player_map
                    .values()
                    .filter(|(_, room)| room == game_id)
                    .count();
                (game_id.clone(), count)
            })
            .collect()
    };

    let rows = if games.is_empty() {
        // Machine-checkable empty-state marker (also the human-facing text).
        "<p class=\"no-games\">No active games — start a lobby to begin.</p>".to_string()
    } else {
        games
            .iter()
            .map(|(game_id, count)| {
                format!(
                    "<tr data-game=\"{}\"><td>{}{}</td><td>{} players</td><td><button class=\"watch-btn\" onclick=\"connectSpectator('{}')\">Watch</button></td></tr>",
                    html_escape(game_id),
                    html_escape(game_id),
                    // data-game is the JS hook; the cell text is the id too.
                    "",
                    count,
                    html_escape(game_id),
                )
            })
            .collect::<Vec<_>>()
            .join("")
    };

    Html(format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>War of Dots — Watch</title>
<style>
  body {{ background:#0d1117; color:#c9d1d9; font-family:ui-monospace,Menlo,Consolas,monospace; margin:0; }}
  .wrap {{ max-width:760px; margin:0 auto; padding:48px 24px; }}
  h1 {{ font-size:20px; letter-spacing:.12em; text-transform:uppercase; color:#58a6ff; }}
  table {{ width:100%; border-collapse:collapse; margin-top:24px; }}
  th, td {{ text-align:left; padding:10px 12px; border-bottom:1px solid #21262d; }}
  th {{ color:#8b949e; font-weight:600; font-size:12px; text-transform:uppercase; letter-spacing:.08em; }}
  .no-games {{ color:#8b949e; margin-top:24px; }}
  .watch-btn {{ background:#238636; border:0; color:#fff; padding:6px 14px; border-radius:6px; cursor:pointer; font-family:inherit; }}
  .watch-btn:hover {{ background:#2ea043; }}
  #status {{ margin-top:20px; color:#8b949e; min-height:1.2em; }}
</style>
</head>
<body>
<div class="wrap">
  <h1>War of Dots — Spectator</h1>
  <table id="games">
    <thead><tr><th>Game</th><th>Players</th><th></th></tr></thead>
    <tbody>
{rows}
    </tbody>
  </table>
  <p id="status"></p>
</div>
<script>
// The server's spectator handler (handlers/spectator.rs) joins the first
// active game room, so one shared endpoint is enough; the game id is
// carried for display and future per-game targeting.
function connectSpectator(gameId) {{
  const status = document.getElementById('status');
  try {{
    const protocol = location.protocol === 'https:' ? 'wss:' : 'ws:';
    const ws = new WebSocket(protocol + '//' + location.host + '/ws/spectator');
    status.textContent = 'Connecting to ' + gameId + '…';
    ws.onopen = () => {{ status.textContent = 'Connected to ' + gameId + ' (spectating).'; }};
    ws.onmessage = (ev) => {{ /* frame consumed; live UI updates live in the main game client */ }};
    ws.onerror = () => {{ status.textContent = 'Connection failed for ' + gameId + '.'; }};
    ws.onclose = () => {{ status.textContent = 'Disconnected from ' + gameId + '.'; }};
  }} catch (e) {{
    status.textContent = 'Could not open spectator connection: ' + e;
  }}
}}
</script>
</body>
</html>
"#,
        rows = rows,
    ))
}

/// Escape a short identifier (game id) for safe embedding in HTML text and
/// double-quoted attributes. Game ids are lobby codes (alphanumeric), but
/// escaping keeps the page robust if that ever changes.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

pub(crate) async fn serve_static(
    req: axum::http::Request<axum::body::Body>,
) -> Result<axum::response::Response, axum::http::StatusCode> {
    let path_str = req.uri().path().trim_start_matches('/').to_string();
    if path_str.is_empty() {
        return Err(axum::http::StatusCode::NOT_FOUND);
    }
    let file = match FRONTEND.get_file(&path_str) {
        Some(file) => file,
        None => return Err(axum::http::StatusCode::NOT_FOUND),
    };
    let content_type = match file.path().extension().and_then(|e| e.to_str()) {
        Some("js") => "application/javascript",
        Some("mjs") => "application/javascript",
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("map") => "application/json",
        Some("wasm") => "application/wasm",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        _ => "application/octet-stream",
    };
    let content = file.contents().to_vec();
    Ok(axum::response::Response::builder()
        .header(axum::http::header::CONTENT_TYPE, content_type)
        .body(axum::body::Body::from(content))
        .unwrap())
}

/// List all available maps
pub(crate) async fn list_maps_handler() -> axum::Json<Vec<String>> {
    axum::Json(map_registry::list_maps())
}

/// Serialize the in-play settings + map name for the `game_metadata.settings`
/// column (persisted by `record_game_end` at game end). `GameSettings` is
/// not `Serialize` (nor is `UnitType`), so project the fields the replay
/// list UI cares about here rather than deriving across modules.
pub(crate) fn game_metadata_settings_json(game: &Game, map_name: &str) -> String {
    let s = game.settings();
    serde_json::json!({
        "map": map_name,
        "settings": {
            "starting_funds": s.starting_funds,
            "initial_manpower": s.initial_manpower,
            "funds_per_city": s.funds_per_city,
            "recruitment_rate": s.recruitment_rate,
            "game_speed": s.game_speed,
            "use_standard_ruleset": s.use_standard_ruleset,
            "unit_types": s.unit_types.iter().map(|u| u.name.clone()).collect::<Vec<_>>(),
            "water_speed": s.water_speed,
        },
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// Game-end metadata persistence
// ---------------------------------------------------------------------------
//
// `persist_game_end` lives in `crate::storage` (Arch #8: it is a storage
// concern called from the tick task). It is no longer defined or re-exported
// here.

#[cfg(test)]
mod tests {
    //! HTTP handler tests: one-shot requests against a REAL `GameServer`
    //! (real storage, real games/player_map state) via
    //! `tower::ServiceExt::oneshot` — no mocked state.

    use super::*;
    use crate::config::Config;
    use crate::game::game::PlayerGameStats;
    use crate::server::game_server::GameServer;
    use crate::server::handlers::replay::{list_replays, serve_replay};
    use crate::server::room::{Command, DebugGameStats, GameRoom};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use std::collections::HashMap;
    use tower::ServiceExt;
    use uuid::Uuid;

    /// A real server whose DB + replay dir live in a unique temp dir, so
    /// the tests never touch `data/`. `GameServer::new` also initializes the
    /// map registry.
    async fn make_server() -> (Arc<Mutex<GameServer>>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("wod-http-test-{}", Uuid::new_v4()));
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
        let config: Config = toml::from_str::<Config>(&toml_str).unwrap().resolve();
        let server = GameServer::new(config, 0)
            .await
            .expect("server boots (temp storage)");
        (Arc::new(Mutex::new(server)), dir)
    }

    /// The same HTTP routes the server registers for these handlers.
    fn app(state: Arc<Mutex<GameServer>>) -> axum::Router {
        axum::Router::new()
            .route("/", get(index_handler))
            .route("/watch", get(watch_handler))
            .route("/maps", get(list_maps_handler))
            .route("/replays", get(list_replays))
            .route("/replays/:name", get(serve_replay))
            .with_state(state)
    }

    /// One-shot GET against the app; returns (status, body-as-text).
    async fn get_body(state: &Arc<Mutex<GameServer>>, uri: &str) -> (StatusCode, String) {
        let req = Request::builder()
            .uri(uri)
            .body(Body::empty())
            .expect("valid request");
        let resp = app(state.clone())
            .oneshot(req)
            .await
            .expect("oneshot request succeeds");
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .expect("body reads");
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Insert a lightweight `GameRoom` (no tick task spawned — the fields
    /// are plain data) so `/watch` has a game to list, and register
    /// `player_count` player_map entries pointing at it.
    async fn add_fake_game(state: &Arc<Mutex<GameServer>>, game_id: &str, player_count: usize) {
        let storage = state.lock().await.storage.clone();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<Command>();
        let room = GameRoom {
            game_id: game_id.to_string(),
            command_tx: tx,
            storage,
            spectator_vision: true,
            stats: Arc::new(std::sync::Mutex::new(DebugGameStats::default())),
        };
        let server = state.lock().await;
        let mut games = server.games.lock().await;
        games.insert(game_id.to_string(), Arc::new(Mutex::new(room)));
        drop(games);
        let mut player_map = server.player_map.lock().await;
        for _ in 0..player_count {
            player_map.insert(
                Uuid::new_v4(),
                (
                    format!("player_{}", Uuid::new_v4().simple()),
                    game_id.to_string(),
                ),
            );
        }
    }

    #[tokio::test]
    async fn watch_page_shows_empty_state_without_games() {
        let (state, dir) = make_server().await;
        let (status, body) = get_body(&state, "/watch").await;
        assert_eq!(status, StatusCode::OK);
        // Machine-checkable empty-state marker.
        assert!(
            body.contains("No active games"),
            "empty /watch page must carry the empty-state marker:\n{body}"
        );
        assert!(
            !body.contains("data-game="),
            "no game rows when there are no games:\n{body}"
        );
        // The JS hook is always present so a listed game is one-click watchable.
        assert!(
            body.contains("connectSpectator"),
            "JS hook missing:\n{body}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn watch_page_lists_active_games_with_player_counts() {
        let (state, dir) = make_server().await;
        add_fake_game(&state, "game-alpha", 3).await;
        add_fake_game(&state, "game-beta", 1).await;

        let (status, body) = get_body(&state, "/watch").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            !body.contains("No active games"),
            "empty-state marker must vanish once games exist:\n{body}"
        );
        // Each game is listed with its id (row hook + JS wiring) and count.
        assert!(
            body.contains(r#"data-game="game-alpha""#),
            "alpha row missing:\n{body}"
        );
        assert!(
            body.contains(r#"connectSpectator('game-alpha')"#),
            "alpha watch hook missing:\n{body}"
        );
        assert!(
            body.contains("3 players"),
            "alpha player count missing:\n{body}"
        );
        assert!(
            body.contains(r#"data-game="game-beta""#),
            "beta row missing"
        );
        assert!(body.contains("1 players"), "beta player count missing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn list_maps_returns_registered_maps() {
        let (state, dir) = make_server().await;
        let (status, body) = get_body(&state, "/maps").await;
        assert_eq!(status, StatusCode::OK);
        // "default" is always registered by map_registry::init (called from
        // GameServer::new).
        assert!(
            body.contains("\"default\""),
            "map list must contain the default map: {body}"
        );
        let maps: Vec<String> = serde_json::from_str(&body).expect("map list is JSON");
        assert!(maps.contains(&"default".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn list_replays_empty_on_fresh_db() {
        let (state, dir) = make_server().await;
        let (status, body) = get_body(&state, "/replays").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "[]", "fresh DB must list no replays: {body}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn list_replays_returns_recorded_games() {
        let (state, dir) = make_server().await;
        let storage = state.lock().await.storage.clone();
        let settings_json = r#"{"map":"default","settings":{}}"#;
        let mut stats = HashMap::new();
        stats.insert(
            "player_0".to_string(),
            PlayerGameStats {
                units_recruited: 3,
                units_lost: 1,
                cities_captured: 1,
                funds_spent: 10,
                funds_earned: 40,
            },
        );
        storage.record_game_end(
            "game-abc",
            1_700_000_000,
            1_700_000_600,
            settings_json,
            &["player_0"],
            &["player_0", "player_1"],
            &stats,
            true,
        );

        let (status, body) = get_body(&state, "/replays").await;
        assert_eq!(status, StatusCode::OK);
        let replays: Vec<serde_json::Value> =
            serde_json::from_str(&body).expect("replay list is JSON");
        assert_eq!(replays.len(), 1, "one recorded game: {body}");
        assert_eq!(replays[0]["game_id"], "game-abc");
        assert_eq!(replays[0]["duration"], 600, "end - start: {body}");
        assert_eq!(
            replays[0]["players"],
            serde_json::json!(["player_0", "player_1"])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn serve_replay_missing_file_returns_404() {
        let (state, dir) = make_server().await;
        // Valid .jsonl name, no such row/file.
        let (status, _) = get_body(&state, "/replays/no-such-game.jsonl").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn serve_replay_serves_recorded_file() {
        let (state, dir) = make_server().await;
        let storage = state.lock().await.storage.clone();
        storage.record_game_end(
            "game-file",
            1_700_000_000,
            1_700_000_120,
            r#"{"map":"default"}"#,
            &[],
            &[],
            &HashMap::new(),
            false,
        );
        // record_game_end registers the expected path; the tick loop's
        // replay recorder writes the actual JSONL during play — simulate
        // that here.
        let replay_dir = dir.join("replays");
        std::fs::create_dir_all(&replay_dir).unwrap();
        let replay_file = replay_dir.join("game-file.jsonl");
        std::fs::write(&replay_file, r#"{"full_state":{}}"#).unwrap();
        assert!(
            replay_file.exists(),
            "recorded replay file must exist: {dir:?}"
        );

        let (status, body) = get_body(&state, "/replays/game-file.jsonl").await;
        assert_eq!(status, StatusCode::OK, "recorded replay must be served");
        assert!(!body.is_empty(), "replay body must not be empty");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn index_page_serves_embedded_frontend() {
        let (state, dir) = make_server().await;
        let (status, body) = get_body(&state, "/").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("<!DOCTYPE html>".to_lowercase().as_str())
                || body.to_lowercase().contains("doctype html"),
            "index must be the embedded frontend HTML:\n{body}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
