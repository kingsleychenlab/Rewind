//! Rewind — time-travel debugging for any local Git repository.
//!
//! This crate is organized as a library with a thin binary front-end
//! (`src/main.rs`) so that integration tests can drive the same code paths the
//! CLI uses. Modules are layered roughly in the order Rewind was built:
//!
//! * [`error`] / [`util`] — shared primitives.
//! * [`paths`] / [`config`] / [`repo`] — where data lives and which repo we act on.
//! * [`db`] / [`objects`] — SQLite metadata and content-addressed blobs.
//! * [`tracking`] / [`snapshot`] / [`diff`] — the working-tree model over time.
//! * [`exec`] / [`investigate`] / [`restore`] — running things and reasoning about them.
//! * [`ci`] — non-interactive reporting.
//! * [`tui`] — the interactive terminal application.

pub mod config;
pub mod error;
pub mod paths;
pub mod repo;
pub mod util;

// Layers added as they are implemented:
pub mod ci;
pub mod cli;
pub mod db;
pub mod diff;
pub mod exec;
pub mod investigate;
pub mod objects;
pub mod restore;
pub mod session;
pub mod snapshot;
pub mod tracking;
pub mod tui;

pub use error::{Result, RewindError};

/// The crate version, surfaced by `rewind --version` and reports.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
