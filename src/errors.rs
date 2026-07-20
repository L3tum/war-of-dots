//! Typed error types for public-facing APIs.
//!
//! [RESERVED FOR FUTURE USE] Internal game logic continues to use `anyhow` and
//! `Option`/`Result<(), String>`. These typed errors are defined for:
//! - Storage/database operations (StorageError)
//! - Configuration loading/validation (ConfigError)
//! - Server/lobby operations (ServerError)
//! - Map operations (MapError)
//!
//! They form a public error API that can be adopted gradually as the codebase
//! matures. Marked with `#![allow(dead_code)]` because they are not yet used
//! internally, but they are intentional scaffolding for future refactoring.
#![allow(dead_code)]

use thiserror::Error;

/// Errors from database/storage operations.
#[derive(Debug, Error)]
pub enum StorageError {
    #[error("failed to open database at {path}: {source}")]
    Open {
        path: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("failed to prepare statement: {0}")]
    Prepare(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("failed to bind parameters: {0}")]
    Bind(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("failed to execute statement: {0}")]
    Execute(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("failed to acquire database lock: {0}")]
    Lock(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("database query returned unexpected result")]
    UnexpectedResult,
}

/// Errors from configuration loading/validation.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("configuration file not found: {path}")]
    NotFound { path: String },

    #[error("invalid configuration: {0}")]
    Invalid(String),

    #[error("failed to parse configuration: {source}")]
    Parse {
        #[source]
        source: serde_json::Error,
    },

    #[error("invalid game setting: {detail}")]
    InvalidSetting { detail: String },
}

/// Errors from server operations.
#[derive(Debug, Error)]
pub enum ServerError {
    #[error("room not found: {room_id}")]
    RoomNotFound { room_id: String },

    #[error("player already in room: {player_id}")]
    PlayerAlreadyInRoom { player_id: String },

    #[error("game already in progress")]
    GameInProgress,

    #[error("not enough players to start game")]
    NotEnoughPlayers,

    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}

/// Errors from map operations.
#[derive(Debug, Error)]
pub enum MapError {
    #[error("map file not found: {path}")]
    NotFound { path: String },

    #[error("invalid map format: {detail}")]
    InvalidFormat { detail: String },

    #[error("map dimensions out of range: {width}x{height}")]
    InvalidDimensions { width: u32, height: u32 },
}
