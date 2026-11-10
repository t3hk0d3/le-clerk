//! Le Clerk: a local resource-lease broker for AI coding agents.

/// Crate version, as declared in `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The daemon protocol: what clients and the daemon must agree on beyond
/// the version (routes, MCP endpoint, hook methods). `GET /v1/version`
/// reports it; every client refuses a daemon reporting another, or none.
pub const PROTOCOL: u32 = 3;

pub mod admin;
pub mod client;
pub mod clock;
pub mod config;
pub mod daemon;
pub mod dedup;
pub mod duration;
pub mod engine;
pub mod error;
pub mod health;
pub mod inventory;
pub mod mcp;
pub mod model;
pub mod scheduler;
pub mod scope;
pub mod store;
