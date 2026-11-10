//! The daemon: the HTTP surface over the engine, served on a private Unix
//! socket, one instance per user.

pub mod lifecycle;
pub mod server;

pub use lifecycle::{PROTOCOL_OVERRIDE_ENV, RunError, VERSION_OVERRIDE_ENV, run};
