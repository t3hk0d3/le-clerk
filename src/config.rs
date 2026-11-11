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
/// Reconnection grace of an agent session whose last liveness stream
/// closed, seconds.
pub const RECONNECT_GRACE_SECS: u64 = 60;
/// How long ended leases and audit entries are kept, days.
pub const RETENTION_DAYS: u64 = 90;
/// How long delivered notices, and notices nobody will take, are kept, days.
pub const NOTICE_RETENTION_DAYS: u64 = 7;

/// Timing configuration; every value is in seconds except the ratio and the
/// retention periods (days).
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
    /// Days an ended lease and an audit entry are kept.
    pub retention_days: u64,
    /// Days a delivered notice, and a notice for a holder that is gone, is
    /// kept.
    pub notice_retention_days: u64,
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
            retention_days: RETENTION_DAYS,
            notice_retention_days: NOTICE_RETENTION_DAYS,
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
        "invalid config value for {key}: windows, timeouts and retention periods must be \
         positive, warning_lead_ratio between 0 and 1"
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
    /// the cleanup, claim and yield windows, the health timeout and the
    /// retention periods are positive.
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
            ("retention_days", self.retention_days),
            ("notice_retention_days", self.notice_retention_days),
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

/// True for the environment variables a child process of the clerk (the
/// daemon, a custom health check) inherits: `HOME`, `PATH`, `XDG_*`,
/// `CLERK_HOME`, `LANG` and `RUST_LOG`. Everything else (an agent's
/// `CLAUDE_*` variables, tokens in the caller's shell) stays behind.
#[must_use]
pub fn inherited_env_var(key: &std::ffi::OsStr) -> bool {
    let Some(key) = key.to_str() else {
        return false;
    };
    matches!(key, "HOME" | "PATH" | "CLERK_HOME" | "LANG" | "RUST_LOG") || key.starts_with("XDG_")
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

    /// The marker a deliberate stop leaves in the state directory: while
    /// it exists, waiting clients do not start a daemon.
    #[must_use]
    pub fn stop_marker(&self) -> PathBuf {
        self.db.with_file_name("stopped")
    }

    /// The lock a daemon holds for its whole life, whatever run directory
    /// it uses: one daemon per store.
    #[must_use]
    pub fn store_lock(&self) -> PathBuf {
        self.db.with_file_name("clerk.db.lock")
    }

    /// Where the running daemon records its socket path, so clients with
    /// another run directory find it.
    #[must_use]
    pub fn addr_file(&self) -> PathBuf {
        self.db.with_file_name("daemon.addr")
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
