//! Runtime configuration and filesystem paths.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

/// Maximum expiry-warning lead, seconds.
pub const WARNING_LEAD_MAX_SECS: u64 = 300;
/// Expiry-warning lead as a fraction of the declared duration.
pub const WARNING_LEAD_RATIO: f64 = 0.2;
/// Cleanup window, seconds.
pub const CLEANUP_WINDOW_SECS: u64 = 120;
/// One-time cleanup extension, seconds.
pub const CLEANUP_EXTENSION_SECS: u64 = 120;
/// Claim window, seconds.
pub const CLAIM_WINDOW_SECS: u64 = 300;
/// Yield answer window, seconds.
pub const YIELD_WINDOW_SECS: u64 = 120;
/// Health-check timeout, seconds.
pub const HEALTH_TIMEOUT_SECS: u64 = 10;
/// Reconnection grace, seconds (reserved for phase 3).
pub const RECONNECT_GRACE_SECS: u64 = 60;
/// Gap between progress notifications of a blocked `lease_wait`, ms.
pub const MCP_PROGRESS_MS: u64 = 30_000;
/// Longest a `lease_wait` (or a waiting task) blocks before it answers with
/// an empty batch, ms.
pub const MCP_WAIT_CAP_MS: u64 = 20 * 60 * 1000;
/// The `pollIntervalMs` suggested for MCP tasks, ms.
pub const MCP_TASK_POLL_MS: u64 = 5_000;

/// Timing configuration; every value is in seconds except the ratio and the
/// `*_ms` values (milliseconds).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Cap on the expiry warning lead.
    pub warning_lead_max: u64,
    /// Warning lead as a fraction of the duration.
    pub warning_lead_ratio: f64,
    /// Cleanup window.
    pub cleanup_window: u64,
    /// Cleanup extension.
    pub cleanup_extension: u64,
    /// Claim window.
    pub claim_window: u64,
    /// Yield answer window.
    pub yield_window: u64,
    /// Health-check timeout.
    pub health_timeout: u64,
    /// Reconnection grace.
    pub reconnect_grace: u64,
    /// Gap between progress notifications of a blocked `lease_wait`, ms.
    pub mcp_progress_ms: u64,
    /// Longest a `lease_wait` (or a waiting MCP task) blocks, ms.
    pub mcp_wait_cap_ms: u64,
    /// The `pollIntervalMs` suggested for MCP tasks, ms.
    pub mcp_task_poll_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            warning_lead_max: WARNING_LEAD_MAX_SECS,
            warning_lead_ratio: WARNING_LEAD_RATIO,
            cleanup_window: CLEANUP_WINDOW_SECS,
            cleanup_extension: CLEANUP_EXTENSION_SECS,
            claim_window: CLAIM_WINDOW_SECS,
            yield_window: YIELD_WINDOW_SECS,
            health_timeout: HEALTH_TIMEOUT_SECS,
            reconnect_grace: RECONNECT_GRACE_SECS,
            mcp_progress_ms: MCP_PROGRESS_MS,
            mcp_wait_cap_ms: MCP_WAIT_CAP_MS,
            mcp_task_poll_ms: MCP_TASK_POLL_MS,
        }
    }
}

/// Why the config file could not be loaded.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The file exists but could not be read.
    #[error("cannot read {path}: {source}")]
    Read {
        /// The config path.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// The file is not valid config TOML.
    #[error("invalid config {path}: {source}")]
    Parse {
        /// The config path.
        path: PathBuf,
        /// The underlying error.
        source: toml::de::Error,
    },
    /// A value is out of range; `key` names it.
    #[error(
        "invalid config value for {key}: windows and timeouts must be positive, \
         warning_lead_ratio between 0 and 1"
    )]
    Invalid {
        /// The offending key.
        key: &'static str,
    },
    /// Neither `CLERK_HOME` nor `HOME` (nor every XDG directory) is set, so
    /// there is nowhere to keep clerk's files.
    #[error("cannot find a home directory: set HOME or CLERK_HOME")]
    NoHome,
    /// The working directory, needed to make a relative `CLERK_HOME`
    /// absolute, cannot be read.
    #[error("cannot make {path} absolute: {source}")]
    Relative {
        /// The relative path.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
}

impl Config {
    /// Load `paths.config`; a missing file means all defaults.
    ///
    /// # Errors
    /// [`ConfigError`] when the file is unreadable or malformed, or
    /// [`ConfigError::Invalid`] when a value is out of range.
    pub fn load(paths: &Paths) -> Result<Self, ConfigError> {
        let text = match fs::read_to_string(&paths.config) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(ConfigError::Read {
                    path: paths.config.clone(),
                    source,
                });
            }
        };
        let config: Self = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: paths.config.clone(),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Check the values: `warning_lead_ratio` is a number in `[0, 1]`, and
    /// the cleanup, claim and yield windows, the health timeout and the MCP
    /// intervals are positive.
    ///
    /// # Errors
    /// [`ConfigError::Invalid`] naming the first offending key.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !(0.0..=1.0).contains(&self.warning_lead_ratio) {
            return Err(ConfigError::Invalid {
                key: "warning_lead_ratio",
            });
        }
        for (key, value) in [
            ("cleanup_window", self.cleanup_window),
            ("claim_window", self.claim_window),
            ("yield_window", self.yield_window),
            ("health_timeout", self.health_timeout),
            ("mcp_progress_ms", self.mcp_progress_ms),
            ("mcp_wait_cap_ms", self.mcp_wait_cap_ms),
            ("mcp_task_poll_ms", self.mcp_task_poll_ms),
        ] {
            if value == 0 {
                return Err(ConfigError::Invalid { key });
            }
        }
        Ok(())
    }
}

/// The daemon socket's path is too long for a Unix socket address.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error(
    "the daemon socket path is {len} bytes long, over the {max}-byte limit for Unix sockets \
     ({}); set a shorter CLERK_HOME or XDG_RUNTIME_DIR",
    path.display()
)]
pub struct SocketPathTooLong {
    /// The socket path.
    pub path: PathBuf,
    /// Its length in bytes.
    pub len: usize,
    /// The longest path the platform takes, in bytes (without the NUL).
    pub max: usize,
}

/// The longest Unix socket path this platform takes, in bytes, without the
/// terminating NUL (107 on Linux).
#[must_use]
pub fn socket_path_max() -> usize {
    // SAFETY: `sockaddr_un` is plain old data; all zeroes is a valid value.
    let addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_path.len() - 1
}

/// Refuse a socket path longer than [`socket_path_max`].
///
/// # Errors
/// [`SocketPathTooLong`] naming the path and its length.
pub fn check_socket_path(path: &Path) -> Result<(), SocketPathTooLong> {
    let len = path.as_os_str().len();
    let max = socket_path_max();
    if len > max {
        return Err(SocketPathTooLong {
            path: path.to_owned(),
            len,
            max,
        });
    }
    Ok(())
}

/// Where clerk keeps its files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// Daemon socket.
    pub socket: PathBuf,
    /// Daemon lock file.
    pub lock: PathBuf,
    /// `SQLite` database.
    pub db: PathBuf,
    /// Daemon log.
    pub log: PathBuf,
    /// Config file.
    pub config: PathBuf,
}

impl Paths {
    /// Resolve from the process environment, honouring `CLERK_HOME`.
    ///
    /// # Errors
    /// As [`Paths::from_lookup`].
    pub fn resolve() -> Result<Self, ConfigError> {
        Self::from_lookup(|k| std::env::var_os(k))
    }

    /// Resolve using `get` to read environment variables. Every path is
    /// absolute: a relative `CLERK_HOME` is taken against the working
    /// directory now, so the daemon and its clients agree whatever their
    /// working directories.
    ///
    /// # Errors
    /// [`ConfigError::NoHome`] when neither `CLERK_HOME` nor `HOME` is set
    /// and an XDG directory that would replace `HOME` is missing;
    /// [`ConfigError::Relative`] when the working directory cannot be read.
    pub fn from_lookup(get: impl Fn(&str) -> Option<OsString>) -> Result<Self, ConfigError> {
        // Empty values count as unset, as the XDG spec says.
        let var = |k: &str| get(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        if let Some(home) = var("CLERK_HOME") {
            let home = std::path::absolute(&home).map_err(|source| ConfigError::Relative {
                path: home.clone(),
                source,
            })?;
            return Ok(Self::under(
                &home.join("run"),
                &home.join("state"),
                &home.join("config"),
            ));
        }
        let home = var("HOME");
        let under_home = |rel: &str| {
            home.as_ref()
                .map(|h| h.join(rel))
                .ok_or(ConfigError::NoHome)
        };
        let state = var("XDG_STATE_HOME").map_or_else(|| under_home(".local/state"), Ok)?;
        let config = var("XDG_CONFIG_HOME").map_or_else(|| under_home(".config"), Ok)?;
        // Without a runtime dir, the socket and lock live beside the database.
        let run = var("XDG_RUNTIME_DIR").map_or_else(|| state.join("clerk"), |r| r.join("clerk"));
        Ok(Self::under(
            &run,
            &state.join("clerk"),
            &config.join("clerk"),
        ))
    }

    fn under(run: &std::path::Path, state: &std::path::Path, config: &std::path::Path) -> Self {
        Self {
            socket: run.join("clerk.sock"),
            lock: run.join("daemon.lock"),
            db: state.join("clerk.db"),
            log: state.join("daemon.log"),
            config: config.join("config.toml"),
        }
    }
}
