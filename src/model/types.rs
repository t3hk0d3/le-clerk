//! Domain value types.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::error::ErrorCode;

/// The kind of thing a resource leases out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceType {
    /// A device node.
    Device,
    /// A network service.
    Service,
    /// An opaque thing identified by a key.
    Generic,
}

impl ResourceType {
    /// The lowercase name used in messages.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Device => "device",
            Self::Service => "service",
            Self::Generic => "generic",
        }
    }
}

/// Request priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    /// Lowest.
    Low,
    /// Default.
    Normal,
    /// Highest.
    High,
}

/// Canonical identity of a project (its common git dir or cwd).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ProjectKey(pub PathBuf);

/// Who may use a resource.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Everything of this user.
    User,
    /// One project only.
    Project(ProjectKey),
}

/// Whether a service unit is something to connect to or to listen as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceMode {
    /// A server already runs; the holder connects.
    Connect,
    /// The holder runs the server on a reserved port.
    Listen,
}

/// Per-unit settings handed to the holder. Unknown keys are refused (the
/// `type` tag excepted), so a misspelt field is an error, not a silent drop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum UnitSettings {
    /// A device node.
    Device {
        /// Filesystem path of the node.
        path: PathBuf,
    },
    /// A network service.
    Service {
        /// Connect or listen.
        mode: ServiceMode,
        /// URL scheme, if any.
        scheme: Option<String>,
        /// Host name or address.
        host: String,
        /// TCP port, never 0.
        port: u16,
        /// Service-specific identifier, if any.
        service_id: Option<String>,
    },
    /// An opaque resource.
    Generic {
        /// Identity key.
        identity_key: String,
    },
}

/// Why unit settings were rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum InvalidSettings {
    /// The settings variant does not fit the resource type.
    #[error("settings do not match resource type {0:?}")]
    TypeMismatch(ResourceType),
    /// A required text field is empty.
    #[error("{0} must not be empty")]
    Empty(&'static str),
    /// The port is 0.
    #[error("port must not be 0")]
    ZeroPort,
}

impl ErrorCode for InvalidSettings {
    fn code(&self) -> &'static str {
        match self {
            InvalidSettings::TypeMismatch(_) => "settings_type_mismatch",
            InvalidSettings::Empty(_) => "settings_empty_field",
            InvalidSettings::ZeroPort => "settings_zero_port",
        }
    }
}

impl UnitSettings {
    /// Check the settings against the resource type they belong to.
    ///
    /// # Errors
    /// [`InvalidSettings`] on a type mismatch, an empty field or port 0.
    pub fn validate(&self, resource_type: ResourceType) -> Result<(), InvalidSettings> {
        match (self, resource_type) {
            (Self::Device { path }, ResourceType::Device) => {
                if path.as_os_str().is_empty() {
                    return Err(InvalidSettings::Empty("path"));
                }
            }
            (Self::Service { host, port, .. }, ResourceType::Service) => {
                if host.is_empty() {
                    return Err(InvalidSettings::Empty("host"));
                }
                if *port == 0 {
                    return Err(InvalidSettings::ZeroPort);
                }
            }
            (Self::Generic { identity_key }, ResourceType::Generic) => {
                if identity_key.is_empty() {
                    return Err(InvalidSettings::Empty("identity_key"));
                }
            }
            _ => return Err(InvalidSettings::TypeMismatch(resource_type)),
        }
        Ok(())
    }

    /// Environment variables describing the unit to the holder.
    #[must_use]
    pub fn env_vars(&self) -> Vec<(String, String)> {
        let mut vars = Vec::new();
        let mut push = |k: &str, v: String| vars.push((format!("CLERK_UNIT_{k}"), v));
        match self {
            Self::Device { path } => push("PATH", path.display().to_string()),
            Self::Service {
                mode,
                scheme,
                host,
                port,
                service_id,
            } => {
                push(
                    "MODE",
                    match mode {
                        ServiceMode::Connect => "connect",
                        ServiceMode::Listen => "listen",
                    }
                    .to_owned(),
                );
                if let Some(s) = scheme {
                    push("SCHEME", s.clone());
                }
                push("HOST", host.clone());
                push("PORT", port.to_string());
                if let Some(s) = service_id {
                    push("SERVICE_ID", s.clone());
                }
            }
            Self::Generic { identity_key } => push("IDENTITY_KEY", identity_key.clone()),
        }
        vars
    }
}

/// Who holds or requests a lease.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HolderRef {
    /// An agent session.
    AgentSession {
        /// The session id.
        session_id: String,
    },
    /// A CLI process, identified by pid and start time to defeat pid reuse.
    CliProcess {
        /// Process id.
        pid: u32,
        /// Process start time.
        start_time: u64,
    },
    /// A human, by tag.
    User {
        /// Free-form tag.
        tag: String,
    },
}

/// Who is calling the API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Caller {
    /// The calling holder.
    pub holder: HolderRef,
    /// The caller's project, if known.
    pub project: Option<ProjectKey>,
    /// Text of the user request this call serves, if any.
    pub override_reason: Option<String>,
}

/// Lease lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseState {
    /// Offered, awaiting claim.
    Offered,
    /// In use.
    Held,
    /// Paused.
    Paused,
    /// Being cleaned up.
    Cleaning,
    /// Over.
    Ended,
}

/// Why a lease ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndCause {
    /// Holder released it.
    Released,
    /// Duration ran out.
    Expired,
    /// Holder yielded on request.
    Yielded,
    /// Taken back by the broker.
    Reclaimed,
    /// Overridden by a user.
    Override,
    /// Holder disappeared.
    HolderGone,
    /// A command failed.
    CommandFailed,
    /// Forced by an admin.
    AdminForced,
    /// An offer was not claimed within the claim window.
    Unclaimed,
}

/// Why a unit is dirty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirtyReason {
    /// The cleanup window passed.
    WindowPassed,
    /// The lease expired.
    Expired,
    /// The holder vanished.
    HolderGone,
    /// A user override.
    Override,
    /// A command failed.
    CommandFailed,
    /// Forced by an admin.
    AdminForced,
}

/// Why a unit is dirty, when, and who last held it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirtyInfo {
    /// The reason.
    pub reason: DirtyReason,
    /// Unix milliseconds.
    pub at: i64,
    /// The holder at the time.
    pub previous_holder: HolderRef,
}

/// A built-in health check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BuiltinCheck {
    /// The path exists.
    Exists,
    /// The path is readable and writable.
    Accessible,
    /// No process has the path open.
    NotOpen,
    /// A TCP connect succeeds.
    TcpConnect,
    /// The port is free to bind.
    PortFree,
    /// An HTTP request succeeds.
    Http,
    /// Always passes.
    None,
}

impl BuiltinCheck {
    /// The kebab-case name used on the wire and in messages.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exists => "exists",
            Self::Accessible => "accessible",
            Self::NotOpen => "not-open",
            Self::TcpConnect => "tcp-connect",
            Self::PortFree => "port-free",
            Self::Http => "http",
            Self::None => "none",
        }
    }
}

/// One health check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "CheckRepr", into = "CheckRepr")]
pub enum Check {
    /// A built-in check.
    Builtin(BuiltinCheck),
    /// A shell command; exit 0 means healthy.
    Custom {
        /// The command line.
        command: String,
    },
}

/// Wire form of [`Check`]: `{"builtin": "…"}` or `{"custom": "…"}`.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CheckRepr {
    Builtin(BuiltinCheck),
    Custom(String),
}

impl From<CheckRepr> for Check {
    fn from(r: CheckRepr) -> Self {
        match r {
            CheckRepr::Builtin(b) => Check::Builtin(b),
            CheckRepr::Custom(command) => Check::Custom { command },
        }
    }
}

impl From<Check> for CheckRepr {
    fn from(c: Check) -> Self {
        match c {
            Check::Builtin(b) => CheckRepr::Builtin(b),
            Check::Custom { command } => CheckRepr::Custom(command),
        }
    }
}

/// An ordered list of checks; all must pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HealthSpec {
    /// The checks, evaluated in order.
    pub checks: Vec<Check>,
}

/// Why a health spec was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum InvalidHealthSpec {
    /// No checks given.
    #[error("health spec has no checks")]
    Empty,
    /// A custom command is blank.
    #[error("custom check command must not be blank")]
    BlankCommand,
    /// A built-in check does not apply to the resource type.
    #[error(
        "health check #{position} '{}' is not offered by type {}; allowed: {}",
        .check.as_str(),
        .resource_type.as_str(),
        allowed_list(*.resource_type)
    )]
    NotAllowed {
        /// 1-based position of the offending entry in the spec.
        position: usize,
        /// The offending check.
        check: BuiltinCheck,
        /// The resource type.
        resource_type: ResourceType,
    },
}

impl ErrorCode for InvalidHealthSpec {
    fn code(&self) -> &'static str {
        match self {
            InvalidHealthSpec::Empty => "health_empty",
            InvalidHealthSpec::BlankCommand => "health_blank_command",
            InvalidHealthSpec::NotAllowed { .. } => "health_check_not_allowed",
        }
    }
}

/// The built-in checks a resource type offers, in display order.
#[must_use]
pub fn allowed_builtins(resource_type: ResourceType) -> &'static [BuiltinCheck] {
    use BuiltinCheck as B;
    match resource_type {
        ResourceType::Device => &[B::Exists, B::Accessible, B::NotOpen, B::None],
        ResourceType::Service => &[B::TcpConnect, B::PortFree, B::Http, B::None],
        ResourceType::Generic => &[B::None],
    }
}

/// Comma-separated kebab-case names of everything a type offers, `custom`
/// included.
fn allowed_list(resource_type: ResourceType) -> String {
    let mut names: Vec<&str> = allowed_builtins(resource_type)
        .iter()
        .map(|c| c.as_str())
        .collect();
    names.push("custom");
    names.join(", ")
}

impl HealthSpec {
    /// Check the spec against the resource type.
    ///
    /// # Errors
    /// [`InvalidHealthSpec`] when empty, when a custom command is blank, or
    /// when a built-in check does not apply to `resource_type`.
    pub fn validate(&self, resource_type: ResourceType) -> Result<(), InvalidHealthSpec> {
        if self.checks.is_empty() {
            return Err(InvalidHealthSpec::Empty);
        }
        for (i, check) in self.checks.iter().enumerate() {
            match check {
                Check::Custom { command } if command.trim().is_empty() => {
                    return Err(InvalidHealthSpec::BlankCommand);
                }
                Check::Builtin(b) if !allowed_builtins(resource_type).contains(b) => {
                    return Err(InvalidHealthSpec::NotAllowed {
                        position: i + 1,
                        check: *b,
                        resource_type,
                    });
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// The type default. `mode` is `None` when the unit's mode is unknown.
    #[must_use]
    pub fn default_for(resource_type: ResourceType, mode: Option<ServiceMode>) -> Self {
        use BuiltinCheck as B;
        let builtins = match (resource_type, mode) {
            (ResourceType::Device, _) => vec![B::Exists, B::NotOpen],
            (ResourceType::Service, Some(ServiceMode::Listen)) => vec![B::PortFree],
            (ResourceType::Service, _) => vec![B::TcpConnect],
            (ResourceType::Generic, _) => vec![B::None],
        };
        Self {
            checks: builtins.into_iter().map(Check::Builtin).collect(),
        }
    }
}
