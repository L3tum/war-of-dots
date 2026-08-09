//! Typed error types for public-facing APIs.
//!
//! These typed errors are used by:
//! - Configuration loading/validation (ConfigError)
//! - Storage/database operations (StorageError)
//! - Map operations (MapError)

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

    #[error("failed to execute statement: {0}")]
    Execute(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// Errors from configuration loading/validation.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("configuration file not found: {path}")]
    NotFound { path: String },

    #[error("failed to parse configuration: {source}")]
    Parse {
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// Errors from map operations.
#[derive(Debug, Error)]
pub enum MapError {
    #[error("map file not found: {path}")]
    NotFound { path: String },

    #[error("invalid map format: {detail}")]
    InvalidFormat { detail: String },
}
