#![allow(dead_code)]
//! Game replay recording

use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::Write;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GameRecord {
    pub game_id: String,
    pub players: Vec<String>,
    pub settings: serde_json::Value,
    pub start_time: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MoveEvent {
    pub tick: u64,
    pub player: String,
    pub action_type: String,
    pub params: serde_json::Value,
    pub reason: Option<String>,
}

pub struct Recorder {
    pub file: File,
    pub game_id: String,
    pub path: String,
}

impl Recorder {
    pub fn new(path: &str, game_id: &str) -> anyhow::Result<Self> {
        let file = File::create(path)?;
        Ok(Recorder {
            file,
            game_id: game_id.to_string(),
            path: path.to_string(),
        })
    }

    pub fn record_header(&mut self, game_record: &GameRecord) {
        let header = serde_json::to_string_pretty(&game_record).unwrap();
        writeln!(self.file, "{}", header).ok();
    }

    pub fn record_move(&mut self, event: &MoveEvent) {
        let line = serde_json::to_string(event).unwrap();
        writeln!(self.file, "{}", line).ok();
        // Flush deferred to finalize() to reduce disk I/O
    }

    pub fn finalize(&mut self, end_time: u64) {
        // Write end marker
        let end = serde_json::json!({"event": "game_end", "time": end_time});
        writeln!(self.file, "{}", end).ok();
        self.file.flush().ok();
    }
}
