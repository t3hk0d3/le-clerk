//! The RPC client: HTTP/1.1 over the daemon's Unix socket, plus starting the
//! daemon on demand.
//!
//! Every request opens its own connection, so one [`Client`] can run a
//! notice long-poll and other calls at the same time.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use bytes::Bytes;
use fs4::fs_std::FileExt;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::net::UnixStream;

use crate::config::{Paths, SocketPathTooLong, check_socket_path};
use crate::error::ErrorCode;
use crate::model::{Caller, HolderRef, Host, LeaseId, RequestId};
use crate::scheduler::liveness;
use crate::store::notices::{Notice, NoticeFilter};

/// How long `ensure_running` waits for a spawned daemon to answer.
pub const START_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest `stop` takes, from asking the daemon to its socket going
/// away.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(15);
/// The longest one version probe may take: a daemon that bound its socket
/// but does not answer must not hold up a caller.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
/// Slack on top of a notice long-poll's own wait.
const LONG_POLL_SLACK: Duration = Duration::from_secs(5);
/// The longest an ordinary RPC may take. Generous: a call may run several
/// health checks, each bounded by the daemon's `health_timeout`.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// Most daemons one `ensure_running` call starts.
const MAX_SPAWNS: u32 = 2;
/// First and longest pause between connection attempts.
const BACKOFF_MIN: Duration = Duration::from_millis(10);
const BACKOFF_MAX: Duration = Duration::from_millis(200);

/// Why no usable daemon could be reached.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DaemonError {
    /// The daemon is not running and could not be started.
    #[error(
        "the clerk daemon is not answering and could not be started; check the daemon log, \
         then retry"
    )]
    Unavailable,
    /// The daemon runs a different version than this client.
    #[error(
        "the clerk daemon runs version {daemon} but this clerk is {client}; run `clerk daemon \
         restart` to switch"
    )]
    VersionMismatch {
        /// This client's version.
        client: String,
        /// The running daemon's version.
        daemon: String,
    },
    /// The daemon runs this version but speaks another protocol, or none
    /// (a daemon built before protocols).
    #[error(
        "the clerk daemon speaks protocol {} but this clerk speaks {client}; run `clerk daemon \
         restart` to switch",
        protocol_text(*.daemon)
    )]
    ProtocolMismatch {
        /// This client's protocol.
        client: u32,
        /// The running daemon's protocol, when it reports one.
        daemon: Option<u32>,
    },
    /// The socket path is too long to bind or connect to.
    #[error(transparent)]
    SocketPathTooLong(#[from] SocketPathTooLong),
    /// A daemon holds the lock but does not answer: it is starting, wedged
    /// or stopped. It is not "not running", and no other daemon can start.
    #[error(
        "the clerk daemon is unresponsive ({}): it holds its lock but does not answer; check the \
         daemon log, or end that process and retry",
        pid_text(*.pid)
    )]
    Unresponsive {
        /// The daemon's pid, from its lock file, when known.
        pid: Option<u32>,
    },
}

fn protocol_text(protocol: Option<u32>) -> String {
    protocol.map_or_else(|| "none (an older build)".to_owned(), |p| p.to_string())
}

fn pid_text(pid: Option<u32>) -> String {
    pid.map_or_else(|| "pid unknown".to_owned(), |p| format!("pid {p}"))
}

impl ErrorCode for DaemonError {
    fn code(&self) -> &'static str {
        match self {
            DaemonError::Unavailable => "daemon_unavailable",
            DaemonError::VersionMismatch { .. } | DaemonError::ProtocolMismatch { .. } => {
                "version_mismatch"
            }
            DaemonError::Unresponsive { .. } => "daemon_unresponsive",
            DaemonError::SocketPathTooLong(_) => "socket_path_too_long",
        }
    }
}

/// An error answer from the daemon, `{"code": "...", "message": "..."}`.
/// Transport failures read `daemon_unavailable`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Error)]
#[error("{code}: {message}")]
pub struct ApiError {
    /// Stable `snake_case` code.
    pub code: String,
    /// Human-readable text.
    pub message: String,
    /// Machine-readable data, for codes that carry it.
    #[serde(default)]
    pub details: Option<Value>,
}

impl ApiError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
            details: None,
        }
    }

    fn unavailable(why: &impl std::fmt::Display) -> Self {
        Self::new(
            "daemon_unavailable",
            format!("the clerk daemon did not answer ({why}); retry, or check the daemon log"),
        )
    }
}

/// What `GET /v1/version` reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonInfo {
    /// The daemon's version.
    pub version: String,
    /// The daemon's protocol; absent from daemons built before protocols.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<u32>,
    /// The daemon's process id.
    pub pid: u32,
}

impl DaemonInfo {
    /// Whether this client can talk to the daemon: same version, same
    /// protocol.
    ///
    /// # Errors
    /// [`DaemonError::VersionMismatch`] or [`DaemonError::ProtocolMismatch`].
    pub fn check(&self) -> Result<(), DaemonError> {
        if self.version != crate::VERSION {
            return Err(DaemonError::VersionMismatch {
                client: crate::VERSION.to_owned(),
                daemon: self.version.clone(),
            });
        }
        if self.protocol != Some(crate::PROTOCOL) {
            return Err(DaemonError::ProtocolMismatch {
                client: crate::PROTOCOL,
                daemon: self.protocol,
            });
        }
        Ok(())
    }
}

/// Environment variables that locate clerk's files.
const PATH_VARS: [&str; 5] = [
    "CLERK_HOME",
    "HOME",
    "XDG_STATE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_RUNTIME_DIR",
];

/// How to start a daemon: `<program> daemon` with `env` added to the
/// inherited environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launcher {
    /// The `clerk` binary.
    pub program: PathBuf,
    /// Extra environment variables.
    pub env: Vec<(OsString, OsString)>,
}

impl Launcher {
    /// This executable, with the inherited environment (so `CLERK_HOME`
    /// passes through).
    ///
    /// # Errors
    /// [`io::Error`] when the running executable cannot be found.
    pub fn current_exe() -> io::Result<Self> {
        Ok(Self {
            program: std::env::current_exe()?,
            env: Vec::new(),
        })
    }

    /// The path variables the child would see as relative, in absolute form
    /// (against this process's working directory): the child runs in `/`
    /// and would otherwise resolve them somewhere else than its clients do.
    /// `env` overrides the inherited value; empty values are left alone.
    fn absolute_path_vars(&self) -> io::Result<Vec<(&'static str, PathBuf)>> {
        let mut fixed = Vec::new();
        for key in PATH_VARS {
            let value = self
                .env
                .iter()
                .rev()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
                .or_else(|| std::env::var_os(key));
            if let Some(value) = value.filter(|v| !v.is_empty()) {
                let path = PathBuf::from(value);
                if path.is_relative() {
                    fixed.push((key, std::path::absolute(&path)?));
                }
            }
        }
        Ok(fixed)
    }

    /// Start `<program> daemon` in its own session, stdin closed and
    /// stdout/stderr appended to the daemon log.
    fn spawn(&self, paths: &Paths) -> io::Result<Child> {
        if let Some(dir) = paths.log.parent() {
            fs::create_dir_all(dir)?;
        }
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&paths.log)?;
        let mut cmd = std::process::Command::new(&self.program);
        cmd.arg("daemon")
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .envs(self.absolute_path_vars()?)
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        // SAFETY: the closure runs in the forked child before exec and only
        // calls setsid, which is async-signal-safe.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        cmd.spawn()
    }
}

/// The caller this process acts as: its own CLI-process holder.
#[must_use]
pub fn cli_caller() -> Caller {
    let holder = match liveness::self_start_time() {
        Some(start_time) => HolderRef::CliProcess {
            pid: std::process::id(),
            start_time,
        },
        None => HolderRef::User {
            tag: "cli".to_owned(),
        },
    };
    Caller {
        holder,
        project: None,
        override_reason: None,
    }
}

/// A connection to a running daemon of this client's version.
#[derive(Debug, Clone)]
pub struct Client {
    socket: PathBuf,
    daemon: DaemonInfo,
    /// The agent session's host, sent with every request (see
    /// [`Client::with_host`]).
    host: Option<Host>,
}

impl Client {
    /// Connect to the running daemon and check its version.
    ///
    /// # Errors
    /// As [`Client::probe`]; [`DaemonError::VersionMismatch`] when the
    /// daemon is another version, [`DaemonError::ProtocolMismatch`] when it
    /// speaks another protocol (or none).
    pub async fn connect(paths: &Paths) -> Result<Self, DaemonError> {
        Self::connect_within(paths, PROBE_TIMEOUT).await
    }

    /// [`Client::connect`], giving up after `limit`. Never starts a
    /// daemon.
    ///
    /// # Errors
    /// As [`Client::connect`].
    pub async fn connect_within(paths: &Paths, limit: Duration) -> Result<Self, DaemonError> {
        let daemon = Self::probe_within(paths, limit).await?;
        daemon.check()?;
        Ok(Self {
            socket: paths.socket.clone(),
            daemon,
            host: None,
        })
    }

    /// Ask the daemon who it is, whatever its version. Never starts one.
    ///
    /// # Errors
    /// When nothing answers within [`PROBE_TIMEOUT`]:
    /// [`DaemonError::Unavailable`] if the daemon lock is free (no daemon
    /// runs), [`DaemonError::Unresponsive`] if a process holds it.
    pub async fn probe(paths: &Paths) -> Result<DaemonInfo, DaemonError> {
        Self::probe_within(paths, PROBE_TIMEOUT).await
    }

    /// [`Client::probe`], giving up after `limit`.
    async fn probe_within(paths: &Paths, limit: Duration) -> Result<DaemonInfo, DaemonError> {
        check_socket_path(&paths.socket)?;
        let (status, body) = send(&paths.socket, Method::GET, "/v1/version", None, limit)
            .await
            .map_err(|e| {
                tracing::debug!(error = %e, "no daemon answers");
                unanswered(paths)
            })?;
        if status != StatusCode::OK {
            tracing::warn!(%status, "the daemon's version endpoint failed");
            return Err(unanswered(paths));
        }
        serde_json::from_slice(&body).map_err(|e| {
            tracing::warn!(error = %e, "the daemon's version answer does not parse");
            unanswered(paths)
        })
    }

    /// Connect, starting the daemon (this executable) when none runs.
    ///
    /// # Errors
    /// As [`Client::ensure_running_with`].
    pub async fn ensure_running(paths: &Paths) -> Result<Self, DaemonError> {
        let launcher = Launcher::current_exe().map_err(|e| {
            tracing::warn!(error = %e, "cannot find the clerk executable");
            DaemonError::Unavailable
        })?;
        Self::ensure_running_with(paths, &launcher).await
    }

    /// Connect, starting the daemon with `launcher` when none runs; the
    /// whole call takes at most about [`START_TIMEOUT`], each probe at most
    /// [`PROBE_TIMEOUT`]. Concurrent callers end up
    /// with one daemon: the daemon's lock lets only one of the started
    /// processes run, the others exit at once.
    ///
    /// A daemon is (re)started only while the lock is free and the one this
    /// call started, if any, has exited, so a daemon that is still shutting
    /// down is waited for rather than raced. At most [`MAX_SPAWNS`] daemons
    /// are started per call.
    ///
    /// # Errors
    /// [`DaemonError::VersionMismatch`] when a daemon of another version
    /// runs (it is not replaced); [`DaemonError::Unresponsive`] when a
    /// daemon holds the lock but never answered; [`DaemonError::Unavailable`]
    /// when no daemon
    /// answered in time.
    pub async fn ensure_running_with(
        paths: &Paths,
        launcher: &Launcher,
    ) -> Result<Self, DaemonError> {
        check_socket_path(&paths.socket)?;
        let deadline = Instant::now() + START_TIMEOUT;
        let left = || {
            deadline
                .saturating_duration_since(Instant::now())
                .min(PROBE_TIMEOUT)
        };
        // An unresponsive daemon may just be starting (lock taken, socket
        // not served yet): wait for it like for a fresh one.
        match Self::connect_within(paths, left()).await {
            Err(DaemonError::Unavailable | DaemonError::Unresponsive { .. }) => {}
            done => return done,
        }
        let mut child: Option<Child> = None;
        let mut spawns = 0;
        let mut backoff = BACKOFF_MIN;
        loop {
            let ours_gone = child
                .as_mut()
                .is_none_or(|c| c.try_wait().map_or(true, |s| s.is_some()));
            if ours_gone && spawns < MAX_SPAWNS && lock_free(&paths.lock) {
                spawns += 1;
                match launcher.spawn(paths) {
                    Ok(c) => child = Some(c),
                    Err(e) => tracing::warn!(error = %e, "cannot start the clerk daemon"),
                }
            }
            tokio::time::sleep(backoff.min(left())).await;
            backoff = (backoff * 2).min(BACKOFF_MAX);
            match Self::connect_within(paths, left()).await {
                Err(DaemonError::Unavailable | DaemonError::Unresponsive { .. })
                    if Instant::now() < deadline => {}
                done => {
                    reap(child);
                    return done;
                }
            }
        }
    }

    /// Stop the running daemon, whatever its version, and wait until its
    /// socket is gone. Returns who was stopped, or `None` when no daemon
    /// was running.
    ///
    /// # Errors
    /// [`DaemonError::Unavailable`] when the daemon refused the stop or its
    /// socket did not go away within [`STOP_TIMEOUT`] (the whole call is
    /// bounded by that, plus one [`PROBE_TIMEOUT`]);
    /// [`DaemonError::Unresponsive`] when a daemon holds the lock but does
    /// not answer. Only a free lock with nothing answering is "not running".
    pub async fn stop(paths: &Paths, caller: &Caller) -> Result<Option<DaemonInfo>, DaemonError> {
        let daemon = match Self::probe(paths).await {
            Ok(d) => d,
            Err(DaemonError::Unavailable) => return Ok(None),
            Err(e) => return Err(e),
        };
        let client = Self {
            socket: paths.socket.clone(),
            daemon,
            host: None,
        };
        let deadline = Instant::now() + STOP_TIMEOUT;
        let asked = client
            .request(
                "daemon_stop",
                caller,
                json!({}),
                deadline.saturating_duration_since(Instant::now()),
            )
            .await;
        if let Err(e) = asked {
            tracing::warn!(error = %e, "daemon_stop failed");
            return Err(DaemonError::Unavailable);
        }
        let mut backoff = BACKOFF_MIN;
        while paths.socket.exists() {
            if Instant::now() >= deadline {
                return Err(DaemonError::Unavailable);
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
        Ok(Some(client.daemon))
    }

    /// Stop the running daemon (if any), then start a fresh one.
    ///
    /// # Errors
    /// As [`Client::stop`] and [`Client::ensure_running`].
    pub async fn restart(paths: &Paths, caller: &Caller) -> Result<Self, DaemonError> {
        Self::stop(paths, caller).await?;
        Self::ensure_running(paths).await
    }

    /// The daemon this client talks to.
    #[must_use]
    pub fn daemon(&self) -> &DaemonInfo {
        &self.daemon
    }

    /// This client with every request also reporting `host`, the process
    /// running the calling agent session (`CLAUDE_PID`).
    #[must_use]
    pub fn with_host(mut self, host: Option<Host>) -> Self {
        self.host = host;
        self
    }

    /// Call RPC `method` as `caller` with `args`, waiting up to
    /// [`CALL_TIMEOUT`] for the answer.
    ///
    /// # Errors
    /// The daemon's [`ApiError`]; `daemon_unavailable` when it cannot be
    /// reached or does not answer in time; `bad_response` when its answer
    /// does not parse.
    pub async fn call(
        &self,
        method: &str,
        caller: &Caller,
        args: Value,
    ) -> Result<Value, ApiError> {
        self.request(method, caller, args, CALL_TIMEOUT).await
    }

    /// [`Client::call`], giving up after `limit`.
    ///
    /// # Errors
    /// As [`Client::call`].
    pub async fn call_within(
        &self,
        method: &str,
        caller: &Caller,
        args: Value,
        limit: Duration,
    ) -> Result<Value, ApiError> {
        self.request(method, caller, args, limit).await
    }

    async fn request(
        &self,
        method: &str,
        caller: &Caller,
        args: Value,
        limit: Duration,
    ) -> Result<Value, ApiError> {
        let mut body = json!({ "caller": caller, "args": args });
        if let Some(host) = self.host {
            body["host"] = json!(host);
        }
        let body = serde_json::to_vec(&body)
            .map_err(|e| ApiError::new("invalid_request", e.to_string()))?;
        let path = format!("/v1/rpc/{method}");
        let (status, body) = send(&self.socket, Method::POST, &path, Some(body), limit)
            .await
            .map_err(|e| ApiError::unavailable(&e))?;
        decode(status, &body)
    }

    /// Deliver `holder`'s pending notices, oldest first, waiting up to
    /// `secs` seconds (the daemon caps it at 60) for one to arrive. The
    /// request itself gives up a few seconds after that wait.
    ///
    /// # Errors
    /// As [`Client::call`].
    pub async fn wait_notices(
        &self,
        holder: &HolderRef,
        secs: u64,
    ) -> Result<Vec<Notice>, ApiError> {
        self.wait_notices_matching(holder, None, secs).await
    }

    /// [`Client::wait_notices`], but with `filter` only the notices naming
    /// one of its leases or requests are delivered (and waited for); the
    /// holder's other notices stay pending.
    ///
    /// # Errors
    /// As [`Client::call`].
    pub async fn wait_notices_matching(
        &self,
        holder: &HolderRef,
        filter: Option<&NoticeFilter>,
        secs: u64,
    ) -> Result<Vec<Notice>, ApiError> {
        let holder = serde_json::to_string(holder)
            .map_err(|e| ApiError::new("invalid_request", e.to_string()))?;
        let mut params = vec![("holder", holder), ("wait", secs.to_string())];
        if let Some(host) = self.host {
            params.push(("host", json!(host).to_string()));
        }
        if let Some(f) = filter {
            let join = |ids: Vec<&str>| ids.join(",");
            params.push((
                "leases",
                join(f.leases.iter().map(LeaseId::as_str).collect()),
            ));
            params.push((
                "requests",
                join(f.requests.iter().map(RequestId::as_str).collect()),
            ));
        }
        let query = serde_urlencoded::to_string(params)
            .map_err(|e| ApiError::new("invalid_request", e.to_string()))?;
        let path = format!("/v1/notices?{query}");
        let wait = Duration::from_secs(secs).min(crate::engine::MAX_WAIT);
        let (status, body) = send(
            &self.socket,
            Method::GET,
            &path,
            None,
            wait + LONG_POLL_SLACK,
        )
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
        let result = decode(status, &body)?;
        serde_json::from_value(result).map_err(|e| ApiError::new("bad_response", e.to_string()))
    }

    /// One step of agent session `session`'s notice wait from `host`, up to
    /// `secs` seconds (the daemon caps it at 60): the daemon's
    /// `{"status": ...}` answer (see `GET /v1/agent/wait`).
    ///
    /// # Errors
    /// As [`Client::call`].
    pub async fn agent_wait(
        &self,
        session: &str,
        host: Host,
        secs: u64,
        first: bool,
    ) -> Result<Value, ApiError> {
        let host = serde_json::to_string(&host)
            .map_err(|e| ApiError::new("invalid_request", e.to_string()))?;
        let query = serde_urlencoded::to_string([
            ("session", session.to_owned()),
            ("host", host),
            ("wait", secs.to_string()),
            ("first", first.to_string()),
        ])
        .map_err(|e| ApiError::new("invalid_request", e.to_string()))?;
        let path = format!("/v1/agent/wait?{query}");
        let wait = Duration::from_secs(secs).min(crate::engine::MAX_WAIT);
        let (status, body) = send(
            &self.socket,
            Method::GET,
            &path,
            None,
            wait + LONG_POLL_SLACK,
        )
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
        decode(status, &body)
    }

    /// Mark agent session `session`'s notices `ids` delivered.
    ///
    /// # Errors
    /// As [`Client::call`].
    pub async fn agent_ack(&self, session: &str, ids: &[String]) -> Result<Value, ApiError> {
        let body = serde_json::to_vec(&json!({ "session": session, "ids": ids }))
            .map_err(|e| ApiError::new("invalid_request", e.to_string()))?;
        let (status, body) = send(
            &self.socket,
            Method::POST,
            "/v1/agent/ack",
            Some(body),
            CALL_TIMEOUT,
        )
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
        decode(status, &body)
    }
}

/// Why a daemon did not answer: [`DaemonError::Unavailable`] when nothing
/// holds the daemon lock (see [`lock_free`]), else
/// [`DaemonError::Unresponsive`] with the pid the lock file names.
fn unanswered(paths: &Paths) -> DaemonError {
    if lock_free(&paths.lock) {
        DaemonError::Unavailable
    } else {
        DaemonError::Unresponsive {
            pid: fs::read_to_string(&paths.lock)
                .ok()
                .and_then(|text| text.trim().parse().ok()),
        }
    }
}

/// True unless another process holds the daemon lock. A lock file that
/// cannot be opened counts as free: the daemon itself decides.
fn lock_free(path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return true;
    };
    // Dropping `file` releases the probe's own lock at once.
    FileExt::try_lock_exclusive(&file).unwrap_or(true)
}

/// Collect a started daemon process once it exits, without blocking. In a
/// short-lived CLI the thread simply dies with the process and the daemon
/// is re-parented.
fn reap(child: Option<Child>) {
    if let Some(mut child) = child {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        let spawned = std::thread::Builder::new()
            .name("clerk-reaper".to_owned())
            .spawn(move || {
                let _ = child.wait();
            });
        if let Err(e) = spawned {
            tracing::debug!(error = %e, "cannot start a reaper thread");
        }
    }
}

/// Turn an answer into the `result` value or the daemon's error.
fn decode(status: StatusCode, body: &[u8]) -> Result<Value, ApiError> {
    #[derive(Deserialize)]
    struct Answer {
        result: Option<Value>,
        error: Option<ApiError>,
    }
    let answer: Answer = serde_json::from_slice(body).map_err(|e| {
        ApiError::new(
            "bad_response",
            format!("the daemon answered {status} with an unreadable body: {e}"),
        )
    })?;
    match (status.is_success(), answer.error) {
        (_, Some(error)) => Err(error),
        (true, None) => Ok(answer.result.unwrap_or(Value::Null)),
        (false, None) => Err(ApiError::new(
            "bad_response",
            format!("the daemon answered {status} without an error"),
        )),
    }
}

/// A transport failure: the socket did not answer HTTP.
#[derive(Debug, Error)]
enum TransportError {
    #[error("cannot connect: {0}")]
    Connect(#[from] io::Error),
    #[error("http: {0}")]
    Http(#[from] hyper::Error),
    #[error("bad request: {0}")]
    Request(#[from] hyper::http::Error),
    #[error("no answer within {0:?}")]
    Timeout(Duration),
}

/// One request on a fresh connection to `socket`, connect to last byte
/// bounded by `limit`.
async fn send(
    socket: &Path,
    method: Method,
    path: &str,
    body: Option<Vec<u8>>,
    limit: Duration,
) -> Result<(StatusCode, Bytes), TransportError> {
    tokio::time::timeout(limit, exchange(socket, method, path, body))
        .await
        .map_err(|_| TransportError::Timeout(limit))?
}

async fn exchange(
    socket: &Path,
    method: Method,
    path: &str,
    body: Option<Vec<u8>>,
) -> Result<(StatusCode, Bytes), TransportError> {
    let stream = UnixStream::connect(socket).await?;
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            tracing::debug!(error = %e, "daemon connection ended with an error");
        }
    });
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(hyper::header::HOST, "clerk");
    if body.is_some() {
        request = request.header(hyper::header::CONTENT_TYPE, "application/json");
    }
    let request = request.body(Full::new(Bytes::from(body.unwrap_or_default())))?;
    let response = sender.send_request(request).await?;
    let status = response.status();
    let body = response.into_body().collect().await?.to_bytes();
    Ok((status, body))
}
