//! HTTP handlers for the game server.
//!
//! Serves the frontend, static assets and the map list, plus the game-end
//! metadata helpers shared with the room's tick loop
//! (`game_metadata_settings_json`, `persist_game_end`).

use axum::response::Html;
use include_dir::{include_dir, Dir};

use crate::game::game::Game;
use crate::map::map_registry;

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

/// Spectator watch page — auto-connects to the current game as a spectator
pub(crate) async fn watch_handler() -> Html<String> {
    Html(r#"
<!DOCTYPE html>
<html>
<head>
  <meta charset="UTF-8">
  <title>War of Dots — Spectator</title>
  <style>
    body { background: #1a1a2e; font-family: sans-serif; color: #fff; text-align: center; }
    .panel { background: rgba(0,0,0,0.8); padding: 32px; border-radius: 8px; margin: 50px auto; max-width: 500px; }
    button { padding: 12px 24px; font-size: 16px; margin: 8px; cursor: pointer; }
  </style>
</head>
<body>
  <div class="panel">
    <h1>📼 Spectator Mode</h1>
    <p>Connect to watch the current game as a spectator.</p>
    <p>Requires the game to have <strong>spectator_vision</strong> enabled.</p>
    <button id="connect-btn">Connect as Spectator</button>
    <br><br>
    <a href="/">Return to Lobby</a>
  </div>
  <script>
    const connectBtn = document.getElementById('connect-btn');
    connectBtn.addEventListener('click', () => {
      const protocol = location.protocol === 'https:' ? 'wss:' : 'ws:';
      const ws = new WebSocket(`${protocol}//${location.host}/ws/spectator`);
      ws.onopen = () => {
        // Redirect to replay viewer with WebSocket connection (complex)
        // For simplicity, just show a message
        alert('Connected as spectator. The game stream is being sent.');
        // In a full implementation, you'd display the game here using the renderer.
        // For now, we just redirect to the game page and let the spectator overlay show.
      };
      ws.onclose = () => alert('Disconnected.');
    });
  </script>
</body>
</html>
"#.to_string())
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

/// Persist the game-end record (wall-clock start/end + the in-play settings
/// JSON) when a game terminates. All four end paths — draw, last player
/// standing, abandoned (no humans left), and room empty — share this.
pub(crate) fn persist_game_end(
    storage: &crate::storage::Storage,
    room_id: &str,
    game_start_time: u64,
    settings_json: &str,
    winners: &[&str],
    player_ids: &[&str],
    game: &Game,
) {
    let end_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    storage.record_game_end(
        room_id,
        game_start_time,
        end_time,
        settings_json,
        winners,
        player_ids,
        game.player_stats(),
        game.settings().use_standard_ruleset,
    );
}
