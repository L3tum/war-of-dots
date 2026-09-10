//! Lobby management

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use subtle::ConstantTimeEq;
use uuid::Uuid;

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

use super::game_server::PlayerSender;
use super::protocol::{LobbySettings, ServerMessage};
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

    #[allow(dead_code)]
    pub fn broadcast_lobby_update(&self, code: &str, msg: &ServerMessage) {
        use super::protocol::encode_message;
        let encoded = encode_message(msg);
        if let Some(lobby) = self.lobbies.get(code) {
            for player in &lobby.players {
                let encoded_clone = encoded.clone();
                let sender = player.connection.clone();
                tokio::spawn(async move {
                    let _ = sender.send_reliable(encoded_clone).await;
                });
            }
        }
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
}
