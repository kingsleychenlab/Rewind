//! Central error type for Rewind.
//!
//! Library code returns [`Result`] using [`RewindError`]. Command-line entry
//! points convert to `anyhow::Error` at the boundary so user-facing messages
//! stay readable. Errors are never silently swallowed.

use std::path::PathBuf;

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, RewindError>;

/// The set of failures Rewind can produce.
#[derive(Debug, thiserror::Error)]
pub enum RewindError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("failed to parse TOML: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("not inside a Git repository (run `git init` or `rewind init`)")]
    NotAGitRepo,

    #[error("git command failed: {0}")]
    Git(String),

    #[error("path {path} escapes the repository root {root}")]
    PathEscapesRoot { path: PathBuf, root: PathBuf },

    #[error("object {0} not found in the content store")]
    ObjectMissing(String),

    #[error("snapshot {0} not found")]
    SnapshotMissing(String),

    #[error("data corruption: {0}")]
    Corrupt(String),

    #[error("operation cancelled")]
    Cancelled,

    #[error("{0}")]
    Other(String),
}

impl RewindError {
    /// Build an ad-hoc error from any displayable value.
    pub fn other(msg: impl Into<String>) -> Self {
        RewindError::Other(msg.into())
    }
}
