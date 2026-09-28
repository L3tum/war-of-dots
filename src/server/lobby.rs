//! Lobby management

use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;

/// M2: lobbies with no join activity for this long are reaped by the
/// periodic sweep in `GameServer::run`. A disconnected creator's lobby is
/// removed immediately (see the disconnect cleanup in game_server.rs) — the
/// TTL is the backstop for lobbies whose members simply never come back.
pub const LOBBY_IDLE_TTL: Duration = Duration::from_secs(30 * 60);

/// M1a: lobby codes are 8 alphanumeric characters — a 36^8 ≈ 2.8×10^12
/// space, out of brute-force reach for a network observer (the previous
/// code was a 12-char UUID prefix: only 8 hex digits of entropy plus a
/// fixed hyphen, and hex is a 4× smaller alphabet than alnum per char).
/// Full alnum keeps the code short enough to type on a LAN while
/// maximizing entropy per character.
pub const LOBBY_CODE_LEN: usize = 8;
const LOBBY_CODE_ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";

/// M1a: mint a fresh lobby code (8 uniformly-drawn alphanumeric chars).
pub fn generate_lobby_code() -> String {
    let mut rng = rand::thread_rng();
    (0..LOBBY_CODE_LEN)
        .map(|_| {
            *LOBBY_CODE_ALPHABET
                .choose(&mut rng)
                .expect("non-empty alphabet") as char
        })
        .collect()
}

/// M2: hard cap on player and LLM agent names. Each name is re-broadcast
/// to every lobby member on every join (LobbyUpdate), so an unbounded
/// name (up to a full 1 MiB wire frame) is a memory/CPU amplification
/// lever. Enforced at the join/add entry points — REJECTED, never
/// silently truncated: truncation would desync the client's display from
/// the server-stored identity without telling the client its name was
/// altered.
pub const MAX_PLAYER_NAME_LEN: usize = 32;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lobby {
    pub code: String,
    pub creator: String,
    pub players: Vec<LobbyPlayer>,
    pub settings: LobbySettings,
    pub password: Option<String>,
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

use super::protocol::{LobbySettings, ServerMessage};
use super::room::PlayerSender;
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
        // M1a: 8 alnum chars (see generate_lobby_code). The retry loop
        // guards against the (negligible at the 100-lobby cap — birthday
        // bound ≈ 10^-10) collision that would otherwise silently
        // overwrite an existing lobby's map entry.
        let mut code = generate_lobby_code();
        while self.lobbies.contains_key(&code) {
            code = generate_lobby_code();
        }
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
        // M2: reject an oversized name (see MAX_PLAYER_NAME_LEN) — the
        // dispatcher-level guard mirrors this; the check here is the
        // domain-level backstop so no future call site can bypass it.
        let name_len = player_name.chars().count();
        if name_len > MAX_PLAYER_NAME_LEN {
            return Err(format!(
                "Player name too long ({name_len} characters; max {MAX_PLAYER_NAME_LEN})"
            ));
        }
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
        // M2: LLM agent names get the same cap as player names (see
        // MAX_PLAYER_NAME_LEN) — they are broadcast in LobbyUpdate too.
        let name_len = name.chars().count();
        if name_len > MAX_PLAYER_NAME_LEN {
            return Err(format!(
                "LLM agent name too long ({name_len} characters; max {MAX_PLAYER_NAME_LEN})"
            ));
        }
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

    /// Arch #6: snapshot a lobby's broadcast payload together with CLONED
    /// member senders, so the (awaiting) transport send can happen AFTER
    /// the LobbyManager lock is dropped. The Lobby domain type owns the
    /// senders; the previous pattern `.await`ed every `send_reliable`
    /// while holding the global manager mutex, serializing every lobby
    /// operation behind one member's network latency. The returned
    /// senders are cheap clones (channel/Arc) and the payload is a
    /// fully-owned `ServerMessage` — nothing borrows from `self`, so the
    /// caller can drop the manager lock and then send. (The call sites in
    /// `room/lobby_flow.rs` still need to switch to this helper — see the
    /// task report; the domain side is ready.)
    pub fn lobby_update_broadcast(&self, code: &str) -> Option<(ServerMessage, Vec<PlayerSender>)> {
        let lobby = self.lobbies.get(code)?;
        let players: Vec<super::protocol::LobbyPlayer> = lobby
            .players
            .iter()
            .map(|p| super::protocol::LobbyPlayer {
                id: p.id.clone(),
                name: p.name.clone(),
                is_human: p.is_human,
                llm_provider: p.llm_provider.clone(),
            })
            .collect();
        let settings = lobby.settings.clone();
        let senders: Vec<PlayerSender> =
            lobby.players.iter().map(|p| p.connection.clone()).collect();
        Some((ServerMessage::LobbyUpdate { players, settings }, senders))
    }
}
impl LobbyManager {
    /// Remove a lobby from the manager. Used to consume a lobby once its game
    /// has started, so a duplicate StartGame cannot re-find it.
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

    // ── M1a: lobby code generation ────────────────────────────────────────

    /// M1a: generated codes are exactly 8 chars drawn from the alnum set —
    /// the brute-force space a network observer would have to search.
    #[test]
    fn generated_lobby_codes_are_eight_alphanumeric_chars() {
        for _ in 0..256 {
            let code = generate_lobby_code();
            assert_eq!(code.len(), LOBBY_CODE_LEN, "code must be 8 chars: {code}");
            assert!(
                code.chars().all(|c| c.is_ascii_alphanumeric()),
                "code must be alnum: {code}"
            );
        }
    }

    /// M1a: no collisions in 1000 mints (birthday bound at 36^8 ≈ 3.6×10^-8
    /// — safe to assert deterministically in CI).
    #[test]
    fn generated_lobby_codes_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            let code = generate_lobby_code();
            assert!(
                seen.insert(code.clone()),
                "duplicate code in 1000 samples: {code}"
            );
        }
    }

    /// M1a: the generated code is the lobby's map key and has the new shape.
    #[test]
    fn create_lobby_uses_generated_code_as_key() {
        let mut mgr = LobbyManager::new();
        let code = mgr.create_lobby(
            "host".into(),
            "Host".into(),
            settings_with_max_players(4),
            None,
            PlayerSender::None,
            vec![],
        );
        assert_eq!(code.len(), LOBBY_CODE_LEN);
        assert!(code.chars().all(|c| c.is_ascii_alphanumeric()));
        assert!(mgr.get_lobby(&code).is_some(), "code must be the map key");
    }

    // ── Password: constant-time compare paths ─────────────────────────────

    fn mgr_with_lobby(password: Option<String>) -> (LobbyManager, String) {
        let mut mgr = LobbyManager::new();
        let code = mgr.create_lobby(
            "host".into(),
            "Host".into(),
            settings_with_max_players(4),
            password,
            PlayerSender::None,
            vec![],
        );
        (mgr, code)
    }

    /// A correct password joins (the ct_eq fast path must not reject a
    /// valid credential).
    #[test]
    fn join_with_correct_password_succeeds() {
        let (mut mgr, code) = mgr_with_lobby(Some("s3cret".into()));
        mgr.join_lobby(
            &code,
            "p1".into(),
            "P1".into(),
            Some("s3cret".into()),
            PlayerSender::None,
        )
        .expect("correct password must join");
        assert_eq!(mgr.get_lobby(&code).unwrap().players.len(), 2);
    }

    /// A wrong password is rejected with the standard error and no seat is
    /// minted.
    #[test]
    fn join_with_wrong_password_rejected() {
        let (mut mgr, code) = mgr_with_lobby(Some("s3cret".into()));
        let err = mgr
            .join_lobby(
                &code,
                "p1".into(),
                "P1".into(),
                Some("wrong".into()),
                PlayerSender::None,
            )
            .expect_err("wrong password must be rejected");
        assert_eq!(err, "Incorrect password");
        assert_eq!(
            mgr.get_lobby(&code).unwrap().players.len(),
            1,
            "no seat on failure"
        );
    }

    /// A lobby with no password accepts a join that provides no password.
    #[test]
    fn join_passwordless_lobby_without_password() {
        let (mut mgr, code) = mgr_with_lobby(None);
        mgr.join_lobby(&code, "p1".into(), "P1".into(), None, PlayerSender::None)
            .expect("no-password lobby accepts a no-password join");
        assert_eq!(mgr.get_lobby(&code).unwrap().players.len(), 2);
    }

    /// A lobby WITH a password rejects a join that provides none (the
    /// provided "" must not ct_eq the stored secret).
    #[test]
    fn join_passworded_lobby_without_password_rejected() {
        let (mut mgr, code) = mgr_with_lobby(Some("s3cret".into()));
        let err = mgr
            .join_lobby(&code, "p1".into(), "P1".into(), None, PlayerSender::None)
            .expect_err("a passworded lobby must reject a passwordless join");
        assert_eq!(err, "Incorrect password");
        assert_eq!(
            mgr.get_lobby(&code).unwrap().players.len(),
            1,
            "no seat on failure"
        );
    }

    // ── M2: name caps (domain-level backstop) ─────────────────────────────

    /// 32 chars is accepted, 33 is rejected — and the cap counts CHARS, not
    /// bytes (a 32-char multibyte name is 96+ bytes but must pass).
    #[test]
    fn join_name_cap_accepts_32_rejects_33_chars() {
        let (mut mgr, code) = mgr_with_lobby(None);
        mgr.join_lobby(
            &code,
            "p1".into(),
            "\u{e9}".repeat(MAX_PLAYER_NAME_LEN),
            None,
            PlayerSender::None,
        )
        .expect("32 multibyte chars must be accepted (cap is per-char, not per-byte)");
        let err = mgr
            .join_lobby(
                &code,
                "p2".into(),
                "n".repeat(MAX_PLAYER_NAME_LEN + 1),
                None,
                PlayerSender::None,
            )
            .expect_err("33-char name must be rejected, not truncated");
        assert!(err.contains("too long"), "{err}");
    }

    #[test]
    fn add_llm_player_name_cap_accepts_32_rejects_33_chars() {
        let (mut mgr, code) = mgr_with_lobby(None);
        mgr.add_llm_player(&code, "a".repeat(MAX_PLAYER_NAME_LEN), "ollama".into())
            .expect("32-char LLM name must be accepted");
        let err = mgr
            .add_llm_player(&code, "a".repeat(MAX_PLAYER_NAME_LEN + 1), "ollama".into())
            .expect_err("33-char LLM name must be rejected, not truncated");
        assert!(err.contains("too long"), "{err}");
    }

    // ── Arch #6: broadcast payload + cloned senders out of the lock ──────

    /// Arch #6: `lobby_update_broadcast` returns an owned payload plus
    /// CLONED senders, so the (awaiting) send can run after the manager
    /// lock is dropped. The test proves the clones stay live after the
    /// manager itself is gone — the property the old send-under-lock
    /// pattern did not have.
    #[test]
    fn lobby_update_broadcast_senders_stay_live_after_manager_drop() {
        let (tx, mut rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::server::room::EgressMessage>();
        let mut mgr = LobbyManager::new();
        // Both members get a real Tcp sender on the SAME channel, so
        // "delivered" can observe both clones after the manager drop
        // (a PlayerSender::None member delivers nothing, by design).
        let host_tx = tx.clone();
        let code = mgr.create_lobby(
            "host".into(),
            "Host".into(),
            settings_with_max_players(4),
            None,
            PlayerSender::Tcp(crate::server::room::TcpEgress::new(host_tx)),
            vec![],
        );
        mgr.join_lobby(
            &code,
            "g1".into(),
            "Guest".into(),
            None,
            PlayerSender::Tcp(crate::server::room::TcpEgress::new(tx)),
        )
        .expect("join");

        // Snapshot under the (implicit) borrow…
        let (update, senders) = mgr.lobby_update_broadcast(&code).expect("lobby exists");
        // …then drop the manager (the stand-in for dropping the lock).
        drop(mgr);

        // The cloned senders must still deliver — no manager state is
        // needed on the send path.
        let data = bytes::Bytes::from(vec![1, 2, 3]);
        for sender in &senders {
            let _ = sender.blocking_send_reliable(data.clone());
        }
        let mut delivered = 0;
        while let Ok(crate::server::room::EgressMessage::Reliable(_)) = rx.try_recv() {
            delivered += 1;
        }
        assert_eq!(
            delivered, 2,
            "both member senders must be live after the manager drop"
        );

        // The payload is the LobbyUpdate snapshot of both members.
        if let ServerMessage::LobbyUpdate { players, .. } = update {
            assert_eq!(players.len(), 2);
            assert!(players.iter().any(|p| p.name == "Host"));
            assert!(players.iter().any(|p| p.name == "Guest"));
        } else {
            panic!("expected a LobbyUpdate payload");
        }
    }
}
