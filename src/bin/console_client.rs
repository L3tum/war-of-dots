//! Console client for War of Dots — headless testing.
//!
//! Speaks the wire protocol directly: u32 BE length-prefixed CBOR frames
//! over raw TCP, starting with the WP3 Hello handshake (Hello must be the
//! first frame; the client waits for HelloAck before sending anything else).

use std::io::Write;

use tokio::io::AsyncBufReadExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use war_of_dots::server::framing::{read_frame, write_frame};
use war_of_dots::server::protocol::{
    ClientMessage, LobbyMessage, LobbySettings, ServerMessage, PROTOCOL_VERSION,
};

const SERVER_URL: &str = "127.0.0.1:8081";

fn main() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let addr = std::env::var("WOZ_SERVER").unwrap_or_else(|_| SERVER_URL.to_string());
        println!("Connecting to {}", addr);
        let stream = match TcpStream::connect(addr).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Failed to connect: {e}");
                return;
            }
        };
        let (mut reader, mut writer) = stream.into_split();

        // ── WP3 handshake: Hello MUST be the first frame ────────────────────────
        let hello = ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            features: 0, // no feature bits requested in v1
        };
        if write_frame(&mut writer, &cbor(&hello)).await.is_err() {
            eprintln!("Failed to send Hello");
            return;
        }
        match read_frame(&mut reader).await {
            Ok(ack) => match ciborium::from_reader::<ServerMessage, _>(&ack[..]) {
                Ok(ServerMessage::HelloAck {
                    protocol_version,
                    features,
                }) => {
                    println!("HelloAck (protocol v{protocol_version}, features {features:#b})");
                }
                Ok(other) => {
                    eprintln!("Expected HelloAck, got {other:?}");
                    return;
                }
                Err(e) => {
                    eprintln!("Failed to decode HelloAck: {e}");
                    return;
                }
            },
            Err(e) => {
                eprintln!("Handshake failed: {e}");
                return;
            }
        }

        // ── Reader task: framed ServerMessage stream ──────────────────────────
        let reader_task = tokio::spawn(async move {
            while let Ok(buf) = read_frame(&mut reader).await {
                match ciborium::from_reader::<ServerMessage, _>(&buf[..]) {
                    Ok(msg) => {
                        let _ = writeln!(std::io::stdout(), "{}", describe(&msg));
                    }
                    Err(e) => println!("(undecoded {e} frame, {} bytes)", buf.len()),
                }
            }
        });

        // ── Stdin → framed ClientMessage stream ─────────────────────────────
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<ClientMessage>(100);
        let stdin_task = tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(tokio::io::stdin());
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break, // EOF
                    Ok(_) => {
                        let cmd = line.trim();
                        if cmd.is_empty() {
                            continue;
                        }
                        match parse_command(cmd) {
                            Some(msg) => {
                                if cmd_tx.send(msg).await.is_err() {
                                    break;
                                }
                            }
                            None => break, // quit/exit
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        while let Some(msg) = cmd_rx.recv().await {
            if write_frame(&mut writer, &cbor(&msg)).await.is_err() {
                break;
            }
        }
        reader_task.await.ok();
        stdin_task.await.ok();
        println!("Disconnected");
    });
}

fn cbor<T: serde::Serialize>(msg: &T) -> Vec<u8> {
    let mut buf = Vec::new();
    ciborium::into_writer(msg, &mut buf).expect("CBOR encode");
    buf
}

/// Compact one-line rendering of a server message (FullState/DeltaState
/// would flood the terminal with grid bytes).
fn describe(msg: &ServerMessage) -> String {
    match msg {
        ServerMessage::HelloAck { .. } => "HelloAck (duplicate)".to_string(),
        ServerMessage::FullState(s) => {
            format!(
                "FullState (terrain {}x{}, units {}, cities {})",
                s.terrain.len(),
                s.terrain.first().map(|r| r.len()).unwrap_or(0),
                s.units.len(),
                s.cities.len()
            )
        }
        ServerMessage::DeltaState(d) => {
            format!(
                "DeltaState (units +{}/~{}, cities ~{}, vision grid {} cells)",
                d.units_added.len(),
                d.unit_updates.len(),
                d.city_updates.len(),
                d.vision_grid_diff.as_ref().map(|g| g.len()).unwrap_or(0)
            )
        }
        ServerMessage::LobbyCreated { lobby_code } => format!("LobbyCreated {lobby_code}"),
        ServerMessage::LobbyJoined { lobby_code } => format!("LobbyJoined {lobby_code}"),
        ServerMessage::LobbyUpdate { players, settings } => {
            let names: Vec<&str> = players.iter().map(|p| p.name.as_str()).collect();
            format!(
                "LobbyUpdate ({}: {} | map {})",
                players.len(),
                names.join(", "),
                settings.map
            )
        }
        ServerMessage::Error { message } => format!("Error: {message}"),
        ServerMessage::LlmReasonLog {
            player_name,
            action,
            reason,
        } => format!("LlmReasonLog {player_name} {action}: {reason}"),
        ServerMessage::Advice { text } => format!("Advice: {text}"),
        ServerMessage::ClearSpectator => "ClearSpectator".to_string(),
        ServerMessage::Leaderboard { entries } => {
            let top: Vec<String> = entries
                .iter()
                .take(3)
                .map(|e| format!("{} ({}W/{}G)", e.name, e.games_won, e.games_played))
                .collect();
            format!("Leaderboard ({})", top.join(", "))
        }
        ServerMessage::GameOver {
            winner,
            game_id,
            message,
        } => format!("GameOver winner={winner} game={game_id} {message}"),
    }
}

fn parse_command(input: &str) -> Option<ClientMessage> {
    let parts: Vec<&str> = input.split_whitespace().collect();
    match parts[0] {
        "quit" | "exit" => None,
        "help" => {
            println!("Commands: create_lobby [name] | join <code> [name] | start_game |");
            println!("  recruit <city_id> <type> | move <unit_id> <x>,<y> | heal <unit_id> |");
            println!("  leaderboard | quit");
            Some(ClientMessage::GetLeaderboard)
        }
        "create_lobby" => {
            let name = parts.get(1).map(|s| s.to_string());
            // Everything not specified takes the serde default — the same
            // baseline a UI user gets with the standard ruleset.
            let settings: LobbySettings = toml::from_str(
                r#"
                map = "default"
                use_standard_ruleset = true
                llm_thinking_delay = 0.0
                "#,
            )
            .expect("default lobby settings");
            Some(ClientMessage::LobbyMessage(LobbyMessage::CreateLobby {
                settings,
                password: None,
                player_name: name,
            }))
        }
        "join" if parts.len() >= 2 => {
            let name = parts.get(2).map(|s| s.to_string());
            Some(ClientMessage::LobbyMessage(LobbyMessage::JoinLobby {
                lobby_code: parts[1].to_string(),
                password: None,
                player_name: name,
            }))
        }
        "start_game" => Some(ClientMessage::LobbyMessage(LobbyMessage::StartGame)),
        "recruit" if parts.len() >= 3 => Some(ClientMessage::RecruitUnit {
            city_id: parts[1].to_string(),
            unit_type: parts[2].to_string(),
        }),
        "move" if parts.len() >= 3 => {
            let (x, y) = parts[2]
                .split_once(',')
                .and_then(|(a, b)| a.parse::<f32>().ok().zip(b.parse::<f32>().ok()))?;
            Some(ClientMessage::MoveUnit {
                unit_id: parts[1].to_string(),
                path: vec![(x, y)],
            })
        }
        "heal" if parts.len() >= 2 => Some(ClientMessage::ToggleHealing {
            unit_id: parts[1].to_string(),
            enabled: true,
        }),
        "leaderboard" => Some(ClientMessage::GetLeaderboard),
        other => {
            println!("Unknown command: {other:?} (try 'help')");
            Some(ClientMessage::GetLeaderboard)
        }
    }
}
