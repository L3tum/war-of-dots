//! Console client for War of Dots - headless testing

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

const SERVER_URL: &str = "127.0.0.1:8081";

fn main() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let addr = std::env::var("WOZ_SERVER").unwrap_or_else(|_| SERVER_URL.to_string());
        println!("Connecting to {}", addr);
        match TcpStream::connect(addr).await {
            Ok(stream) => {
                let (mut reader, mut writer) = stream.into_split();

                // Spawn a reader task to receive server messages
                let reader_task = tokio::spawn(async move {
                    let mut buf = Vec::new();
                    loop {
                        match reader.read(&mut buf).await {
                            Ok(0) => break,
                            Ok(_) => {
                                match ciborium::from_reader::<ciborium::Value, _>(buf.as_slice()) {
                                    Ok(msg) => {
                                        // Extract the "type" field from a CBOR map
                                        let type_name = extract_type(&msg);
                                        match type_name {
                                            Some(name) => println!("{}: {:?}", name, msg),
                                            None => println!("{:?}", msg),
                                        }
                                    }
                                    Err(_) => {
                                        println!("Server raw: {}", String::from_utf8_lossy(&buf));
                                    }
                                }
                                buf.clear();
                            }
                            Err(_) => break,
                        }
                    }
                });

                // Spawn a task to read from stdin asynchronously and send commands
                let (cmd_tx, mut cmd_rx) = mpsc::channel::<String>(100);
                let stdin = tokio::io::stdin();
                let stdin_task = tokio::spawn(async move {
                    let mut reader = tokio::io::BufReader::new(stdin);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line).await {
                            Ok(0) => break, // EOF
                            Ok(_) => {
                                let trimmed = line.trim().to_string();
                                if trimmed.is_empty() {
                                    continue;
                                }
                                if cmd_tx.send(trimmed).await.is_err() {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                });

                // Receive commands from stdin task and write to TCP
                while let Some(cmd_line) = cmd_rx.recv().await {
                    if let Ok(msg_bytes) = parse_command(&cmd_line) {
                        if writer.write_all(&msg_bytes).await.is_err() {
                            break;
                        }
                    }
                }

                reader_task.await.ok();
                stdin_task.await.ok();
            }
            Err(e) => {
                eprintln!("Failed to connect: {}", e);
            }
        }
    });
}

/// Extract the "type" field from a CBOR map value
fn extract_type(value: &ciborium::Value) -> Option<String> {
    if let ciborium::Value::Map(entries) = value {
        for (k, v) in entries {
            if let (ciborium::Value::Text(key), ciborium::Value::Text(val)) = (k, v) {
                if key == "type" {
                    return Some(val.clone());
                }
            }
        }
    }
    None
}

fn parse_command(input: &str) -> Result<Vec<u8>, String> {
    let parts: Vec<&str> = input.split_whitespace().collect();
    if parts.is_empty() {
        return Err("Empty command".to_string());
    }
    match parts[0] {
        "quit" | "exit" => Ok(b"quit".to_vec()),
        "help" => {
            println!("Commands:");
            println!("  recruit <city_id> <unit_type>  - Recruit a unit");
            println!("  move <unit_id> <x>,<y>          - Move a unit");
            println!("  heal <unit_id>                  - Toggle healing");
            println!("  create_lobby                    - Create a new lobby");
            println!("  join <lobby_code>               - Join a lobby");
            println!("  start_game                      - Start game in your lobby");
            println!("  quit/exit                       - Quit");
            Ok(b"help".to_vec())
        }
        "recruit" if parts.len() >= 3 => {
            let city_id = parts[1];
            let unit_type = parts[2];
            Ok(format!("recruit_{}_{}", city_id, unit_type).into_bytes())
        }
        "move" if parts.len() >= 3 => {
            let unit_id = parts[1];
            let coords = parts[2];
            Ok(format!("move_{}_{}", unit_id, coords).into_bytes())
        }
        "heal" if parts.len() >= 2 => Ok(format!("heal_{}", parts[1]).into_bytes()),
        "create_lobby" => Ok(b"create_lobby".to_vec()),
        "join" if parts.len() >= 2 => Ok(format!("join_{}", parts[1]).into_bytes()),
        "start_game" => Ok(b"start_game".to_vec()),
        other => {
            println!("Unknown command: {}", other);
            Ok(b"unknown".to_vec())
        }
    }
}
