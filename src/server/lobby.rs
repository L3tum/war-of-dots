//! Lobby management

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;
use uuid::Uuid;

/// M2: lobbies with no join activity for this long are reaped by the
/// periodic sweep in `GameServer::run`. A disconnected creator's lobby is
/// removed immediately (see the disconnect cleanup in game_server.rs) — the
/// TTL is the backstop for lobbies whose members simply never come back.
pub const LOBBY_IDLE_TTL: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lobby {
    pub code: String,
    pub creator: String,
    pub players: Vec<LobbyPlayer>,
    pub settings: LobbySettings,
    pub password: Option<String>,
    pub llm_agents: Vec<LlmAgentConfig>, // legacy field, still present for backward compat
    /// Per-slot LLM configuration (new system)
    pub llm_agent_configs: std::collections::HashMap<u32, LlmAgentConfig>,
    /// Custom unit types when use_standard_ruleset is false (from lobby settings)
    pub custom_unit_types: Vec<super::protocol::CustomUnitType>,
    /// M2: last join activity (creation counts as activity). Updated on
    /// every successful `join_lobby`/`add_llm_player`; the TTL sweep reaps
    /// lobbies idle longer than `LOBBY_IDLE_TTL`.
    #[serde(skip, default = "lobby_last_activity_default")]
    pub last_activity: Instant,
}

#[derive(Debug, Clone)]
pub struct LobbyPlayer {
    pub id: String,
    pub name: String,
    pub is_human: bool,
    pub llm_provider: Option<String>,
    pub connected: bool,
    pub connection: PlayerSender,
}

// For serialization/deserialization, we need custom impl
impl serde::Serialize for LobbyPlayer {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("LobbyPlayer", 5)?;
        s.serialize_field("id", &self.id)?;
        s.serialize_field("name", &self.name)?;
        s.serialize_field("is_human", &self.is_human)?;
        s.serialize_field("llm_provider", &self.llm_provider)?;
        s.serialize_field("connected", &self.connected)?;
        s.end()
    }
}

impl<'de> serde::Deserialize<'de> for LobbyPlayer {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Helper {
            id: String,
            name: String,
            is_human: bool,
            llm_provider: Option<String>,
            connected: bool,
        }
        let h = Helper::deserialize(deserializer)?;
        // Default to TCP sender (shouldn't happen in practice)
        let (_tx, _rx) = tokio::sync::mpsc::unbounded_channel::<super::room::EgressMessage>();
        Ok(LobbyPlayer {
            id: h.id,
            name: h.name,
            is_human: h.is_human,
            llm_provider: h.llm_provider,
            connected: h.connected,
            connection: PlayerSender::Tcp(super::room::TcpEgress::new(_tx)),
        })
    }
}

fn lobby_last_activity_default() -> Instant {
    Instant::now()
}

use super::game_server::PlayerSender;
use super::protocol::LobbySettings;
use crate::llm::agent::LlmAgentConfig;

pub struct LobbyManager {
    pub lobbies: HashMap<String, Lobby>,
}

impl LobbyManager {
    pub fn new() -> Self {
        LobbyManager {
            lobbies: HashMap::new(),
        }
    }
}

impl Default for LobbyManager {
    fn default() -> Self {
        Self::new()
    }
}

impl LobbyManager {
    pub fn create_lobby(
        &mut self,
        creator_id: String,
        creator_name: String,
        settings: LobbySettings,
        password: Option<String>,
        connection: PlayerSender,
        custom_unit_types: Vec<super::protocol::CustomUnitType>,
    ) -> String {
        // Normalize the player cap. The hard server cap is 16 (see
        // validate_custom_settings); anything outside 1..=16 is treated as
        // "unspecified" and defaulted to 4. The 0 case is the real one:
        // standard-ruleset clients omit `max_players` entirely (their form
        // section is hidden) and serde fills in 0, which used to make the
        // lobby join-proof — the full check `players.len() >= max_players`
        // was immediately true (1 >= 0) and every join failed with "Lobby
        // full". Four is the natural default (the default map puts city
        // clusters at the four corners), and clamping also drops hostile
        // out-of-range values (e.g. u32::MAX) that previously slipped through
        // unvalidated on the standard-ruleset path.
        let mut settings = settings;
        if !(1..=16).contains(&settings.max_players) {
            settings.max_players = 4;
        }
        let code = Uuid::new_v4().to_string()[..12].to_string();
        let lobby = Lobby {
            code: code.clone(),
            creator: creator_id.clone(),
            players: vec![LobbyPlayer {
                id: creator_id,
                name: creator_name,
                is_human: true,
                llm_provider: None,
                connected: true,
                connection,
            }],
            settings,
            password,
            llm_agents: Vec::new(),
            llm_agent_configs: std::collections::HashMap::new(),
            custom_unit_types,
            last_activity: Instant::now(),
        };
        self.lobbies.insert(code.clone(), lobby);
        code
    }

    pub fn join_lobby(
        &mut self,
        code: &str,
        player_id: String,
        player_name: String,
        password: Option<String>,
        connection: PlayerSender,
    ) -> Result<&mut Lobby, String> {
        let lobby = self.lobbies.get_mut(code).ok_or("Lobby not found")?;
        if let Some(pass) = &lobby.password {
            // Constant-time comparison to avoid leaking password prefix info via timing.
            let provided = password.as_deref().unwrap_or("");
            if !bool::from(provided.as_bytes().ct_eq(pass.as_bytes())) {
                return Err("Incorrect password".to_string());
            }
        }
        if lobby.players.len() >= lobby.settings.max_players as usize {
            return Err("Lobby full".to_string());
        }
        // M2: a successful join is activity — reset the TTL clock.
        lobby.last_activity = Instant::now();
        lobby.players.push(LobbyPlayer {
            id: player_id,
            name: player_name,
            is_human: true,
            llm_provider: None,
            connected: true,
            connection,
        });
        Ok(lobby)
    }

    pub fn set_slot_llm(&mut self, code: &str, slot: u32, provider: String) -> Result<(), String> {
        let lobby = self.lobbies.get_mut(code).ok_or("Lobby not found")?;
        let player = lobby
            .players
            .get_mut(slot as usize)
            .ok_or("Slot not found")?;
        if provider.is_empty() {
            // Empty provider means revert to human
            player.is_human = true;
            player.llm_provider = None;
        } else {
            player.is_human = false;
            player.llm_provider = Some(provider);
        }
        Ok(())
    }

    pub fn set_slot_llm_config(
        &mut self,
        code: &str,
        slot: u32,
        config: LlmAgentConfig,
    ) -> Result<(), String> {
        let lobby = self.lobbies.get_mut(code).ok_or("Lobby not found")?;
        lobby.llm_agent_configs.insert(slot, config);
        Ok(())
    }

    /// Add an LLM-only player to the lobby (no WebSocket connection)
    pub fn add_llm_player(
        &mut self,
        code: &str,
        name: String,
        llm_provider: String,
    ) -> Result<(), String> {
        let lobby = self.lobbies.get_mut(code).ok_or("Lobby not found")?;
        if lobby.players.len() >= lobby.settings.max_players as usize {
            return Err("Lobby full".to_string());
        }
        let slot = lobby.players.len() as u32;
        let id = format!("llm-{}", slot);
        // M2: an LLM slot fill is join activity too.
        lobby.last_activity = Instant::now();
        lobby.players.push(LobbyPlayer {
            id: id.clone(),
            name,
            is_human: false,
            llm_provider: Some(llm_provider),
            connected: false,
            connection: PlayerSender::None,
        });
        Ok(())
    }

    pub fn get_lobby(&self, code: &str) -> Option<&Lobby> {
        self.lobbies.get(code)
    }
}
impl LobbyManager {
    #[allow(dead_code)]
    pub fn get_lobby_mut(&mut self, code: &str) -> Option<&mut Lobby> {
        self.lobbies.get_mut(code)
    }
    #[allow(dead_code)]
    pub fn get_first_lobby(&self) -> Option<(&String, &Lobby)> {
        self.lobbies.iter().next()
    }

    /// Remove a lobby from the manager. Used to consume a lobby once its game
    /// has started, so it cannot be re-found by `get_first_lobby` for a
    /// duplicate StartGame.
    pub fn remove_lobby(&mut self, code: &str) -> Option<Lobby> {
        self.lobbies.remove(code)
    }

    /// M2: remove every lobby idle (no join activity) for at least `ttl`.
    /// Returns the removed lobby codes so the caller can log them.
    pub fn reap_idle_lobbies(&mut self, ttl: Duration) -> Vec<String> {
        let now = Instant::now();
        let idle: Vec<String> = self
            .lobbies
            .values()
            .filter(|l| now.duration_since(l.last_activity) >= ttl)
            .map(|l| l.code.clone())
            .collect();
        for code in &idle {
            self.lobbies.remove(code);
        }
        idle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lobby settings with the required fields and a chosen player cap. `0`
    /// is the "unspecified" shape standard-ruleset clients actually send
    /// (they omit the field, serde fills in 0).
    fn settings_with_max_players(max: u32) -> LobbySettings {
        let mut s: LobbySettings = toml::from_str(
            r#"
            map = "default"
            use_standard_ruleset = true
            llm_thinking_delay = 0.0
            "#,
        )
        .expect("lobby settings parse");
        s.max_players = max;
        s
    }

    /// Regression (the original E2E-caught bug): an unspecified (0) cap must
    /// normalize to a playable default rather than leave the lobby join-proof.
    #[test]
    fn create_lobby_normalizes_unspecified_cap() {
        let mut mgr = LobbyManager::new();
        let code = mgr.create_lobby(
            "host".into(),
            "Host".into(),
            settings_with_max_players(0),
            None,
            PlayerSender::None,
            vec![],
        );
        let lobby = mgr.get_lobby(&code).expect("lobby exists");
        assert_eq!(
            lobby.settings.max_players, 4,
            "an unspecified (0) cap must become the default"
        );
    }

    /// A hostile out-of-range cap (e.g. u32::MAX) is clamped to the default
    /// instead of being stored verbatim and passing unvalidated.
    #[test]
    fn create_lobby_clamps_out_of_range_cap() {
        let mut mgr = LobbyManager::new();
        let code = mgr.create_lobby(
            "host".into(),
            "Host".into(),
            settings_with_max_players(u32::MAX),
            None,
            PlayerSender::None,
            vec![],
        );
        let lobby = mgr.get_lobby(&code).expect("lobby exists");
        assert_eq!(
            lobby.settings.max_players, 4,
            "u32::MAX must clamp to the default cap, not be stored"
        );
    }

    /// The mirror of the original bug: the normalized cap must actually bite.
    /// A cap-4 lobby fills at four players and rejects the fifth — proving
    /// the fix enforces a real ceiling, not just a permissive one.
    #[test]
    fn normalized_cap_bites_on_the_fifth_join() {
        let mut mgr = LobbyManager::new();
        let code = mgr.create_lobby(
            "host".into(),
            "Host".into(),
            settings_with_max_players(0),
            None,
            PlayerSender::None,
            vec![],
        );
        // Host (already in the lobby) + 3 guests = 4 players = the cap.
        for i in 0..3 {
            mgr.join_lobby(
                &code,
                format!("p{i}"),
                format!("p{i}"),
                None,
                PlayerSender::None,
            )
            .expect("join within the cap must succeed");
        }
        assert_eq!(mgr.get_lobby(&code).unwrap().players.len(), 4);

        // The 5th join is now full.
        let err = mgr
            .join_lobby(&code, "p4".into(), "p4".into(), None, PlayerSender::None)
            .expect_err("fifth join must fail on a normalized cap-4 lobby");
        assert_eq!(err, "Lobby full");
    }

    /// M2: an idle lobby is reaped once it exceeds the TTL, while a lobby
    /// with recent activity survives. The TTL is a parameter, so the test
    /// uses a tiny budget instead of sleeping 30 minutes.
    #[test]
    fn reap_idle_lobbies_drops_only_idle_lobbies() {
        let mut mgr = LobbyManager::new();
        let idle_code = mgr.create_lobby(
            "host1".into(),
            "Host1".into(),
            settings_with_max_players(4),
            None,
            PlayerSender::None,
            vec![],
        );
        let live_code = mgr.create_lobby(
            "host2".into(),
            "Host2".into(),
            settings_with_max_players(4),
            None,
            PlayerSender::None,
            vec![],
        );
        // Backdate the idle lobby past a 10 ms TTL.
        {
            let lobby = mgr.lobbies.get_mut(&idle_code).unwrap();
            lobby.last_activity = Instant::now() - Duration::from_millis(50);
        }
        // A join on the live lobby must reset its activity clock.
        mgr.join_lobby(
            &live_code,
            "guest".into(),
            "Guest".into(),
            None,
            PlayerSender::None,
        )
        .expect("join must succeed");
        assert!(mgr.get_lobby(&live_code).unwrap().last_activity > lobby_now_minus_50ms());

        let reaped = mgr.reap_idle_lobbies(Duration::from_millis(10));
        assert_eq!(reaped, vec![idle_code.clone()]);
        assert!(
            mgr.get_lobby(&idle_code).is_none(),
            "idle lobby must be gone"
        );
        assert!(
            mgr.get_lobby(&live_code).is_some(),
            "active lobby must survive"
        );
    }

    /// M2: the join-activity update is what keeps a lobby alive — without a
    /// join, even a just-created lobby is reaped at a zero TTL.
    #[test]
    fn created_lobby_is_reapable_when_idle() {
        let mut mgr = LobbyManager::new();
        let code = mgr.create_lobby(
            "host".into(),
            "Host".into(),
            settings_with_max_players(4),
            None,
            PlayerSender::None,
            vec![],
        );
        std::thread::sleep(Duration::from_millis(5));
        let reaped = mgr.reap_idle_lobbies(Duration::from_millis(1));
        assert_eq!(reaped, vec![code]);
    }

    fn lobby_now_minus_50ms() -> Instant {
        Instant::now() - Duration::from_millis(50)
    }
}
