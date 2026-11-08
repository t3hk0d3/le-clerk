//! Le Clerk: a local resource-lease broker for AI coding agents.

/// Crate version, as declared in `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod admin;
pub mod client;
pub mod clock;
pub mod config;
pub mod daemon;
pub mod dedup;
pub mod engine;
pub mod error;
pub mod health;
pub mod inventory;
pub mod model;
pub mod scheduler;
pub mod scope;
pub mod store;
