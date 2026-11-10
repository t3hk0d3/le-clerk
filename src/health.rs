//! Health checks: pure I/O against a unit's settings.
//!
//! The checker is synchronous from the caller's view. The HTTP check blocks
//! on the process's one check [`runtime`], so it must not be called from
//! inside a tokio runtime's async context.

use std::ffi::CString;
use std::fs;
use std::io::{self, Read};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::Empty;
use hyper::Request;
use hyper_util::rt::TokioIo;
use tokio::runtime::{Handle, Runtime};
use tokio::task::JoinHandle;

use crate::model::{BuiltinCheck, Check, HealthSpec, ResourceId, UnitId, UnitSettings};

/// Longest stderr excerpt kept from a failing custom command.
const STDERR_LIMIT: usize = 1024;

/// The runtime http checks and the check pool use; built on first use and
/// kept for the life of the process.
static RUNTIME: Mutex<Option<Runtime>> = Mutex::new(None);
/// How many times [`RUNTIME`] was built.
static RUNTIMES_BUILT: AtomicUsize = AtomicUsize::new(0);

/// A handle to the process's one check runtime.
///
/// # Errors
/// [`io::Error`] when the runtime cannot be started.
pub fn runtime() -> io::Result<Handle> {
    // Only ever holds a finished runtime, so a panic leaves nothing to repair.
    let mut slot = RUNTIME.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(runtime) = slot.as_ref() {
        return Ok(runtime.handle().clone());
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("clerk-check")
        .enable_all()
        .build()?;
    RUNTIMES_BUILT.fetch_add(1, Ordering::Relaxed);
    let handle = runtime.handle().clone();
    *slot = Some(runtime);
    Ok(handle)
}

/// How many check runtimes this process has built; at most one.
#[must_use]
pub fn runtimes_built() -> usize {
    RUNTIMES_BUILT.load(Ordering::Relaxed)
}

/// The custom commands running now, by process group.
static RUNNING: Mutex<Running> = Mutex::new(Running {
    groups: Vec::new(),
    stopped: false,
});

struct Running {
    groups: Vec<libc::pid_t>,
    /// Set by [`stop_checks`]: no command starts any more.
    stopped: bool,
}

fn running() -> MutexGuard<'static, Running> {
    // A list of ids and a flag; a panic leaves nothing to repair.
    RUNNING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The process is going away: kill the process group of every custom
/// command still running, and start no more.
pub fn stop_checks() {
    let mut running = running();
    running.stopped = true;
    for group in &running.groups {
        // SAFETY: `kill` has no memory-safety preconditions. The negative
        // pid addresses a group created via `process_group(0)`, listed
        // only until its command has been waited for.
        unsafe {
            libc::kill(-group, libc::SIGKILL);
        }
    }
}

/// A custom command's place in [`RUNNING`], until dropped.
struct Tracked(libc::pid_t);

impl Drop for Tracked {
    fn drop(&mut self) {
        running().groups.retain(|g| *g != self.0);
    }
}

/// Spawn `cmd` (in its own process group) and list it in [`RUNNING`], under
/// one lock, so that [`stop_checks`] misses no command.
fn spawn_tracked(cmd: &mut Command) -> io::Result<(Child, Tracked)> {
    let mut running = running();
    if running.stopped {
        return Err(io::Error::other("the daemon is stopping"));
    }
    let child = cmd.spawn()?;
    let group = libc::pid_t::try_from(child.id()).map_err(io::Error::other)?;
    running.groups.push(group);
    Ok((child, Tracked(group)))
}

/// Outcome of a health check run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthResult {
    /// Every check passed.
    Healthy,
    /// A check failed; the text says why.
    Unhealthy(String),
}

fn unhealthy(reason: impl Into<String>) -> HealthResult {
    HealthResult::Unhealthy(reason.into())
}

/// Runs health checks with a per-check timeout.
#[derive(Debug, Clone, Copy)]
pub struct HealthChecker {
    timeout: Duration,
}

/// Which resource and unit a run is for; exported to custom commands.
#[derive(Clone, Copy)]
struct Ids<'a> {
    resource: &'a ResourceId,
    unit: &'a UnitId,
}

impl HealthChecker {
    /// A checker whose network and command checks give up after `timeout`.
    #[must_use]
    pub fn new(timeout: Duration) -> Self {
        Self { timeout }
    }

    /// Run `spec` in order against `settings`, stopping at the first failure.
    ///
    /// # Panics
    /// The http check panics when called from a tokio runtime's async
    /// context (it blocks on the check runtime).
    ///
    /// Custom commands get no `CLERK_RESOURCE` / `CLERK_UNIT`; use
    /// [`HealthChecker::check_unit`] when the ids are known.
    #[must_use]
    pub fn check(&self, spec: &HealthSpec, settings: &UnitSettings) -> HealthResult {
        self.run(spec, None, settings)
    }

    /// Like [`HealthChecker::check`], also exporting `CLERK_RESOURCE` and
    /// `CLERK_UNIT` to custom commands.
    ///
    /// # Panics
    /// As [`HealthChecker::check`]: not callable from inside a tokio runtime.
    #[must_use]
    pub fn check_unit(
        &self,
        spec: &HealthSpec,
        resource: &ResourceId,
        unit: &UnitId,
        settings: &UnitSettings,
    ) -> HealthResult {
        self.run(spec, Some(Ids { resource, unit }), settings)
    }

    fn run(
        &self,
        spec: &HealthSpec,
        ids: Option<Ids<'_>>,
        settings: &UnitSettings,
    ) -> HealthResult {
        for check in &spec.checks {
            let result = match check {
                Check::Builtin(b) => self.builtin(*b, settings),
                Check::Custom { command } => self.custom(command, ids, settings),
            };
            if result != HealthResult::Healthy {
                return result;
            }
        }
        HealthResult::Healthy
    }

    fn builtin(&self, check: BuiltinCheck, settings: &UnitSettings) -> HealthResult {
        match (check, settings) {
            (BuiltinCheck::None, _) => HealthResult::Healthy,
            (BuiltinCheck::Exists, UnitSettings::Device { path }) => exists(path),
            (BuiltinCheck::Accessible, UnitSettings::Device { path }) => accessible(path),
            (BuiltinCheck::NotOpen, UnitSettings::Device { path }) => not_open(path),
            (BuiltinCheck::TcpConnect, UnitSettings::Service { host, port, .. }) => {
                self.tcp_connect(host, *port)
            }
            (BuiltinCheck::PortFree, UnitSettings::Service { host, port, .. }) => {
                port_free(host, *port)
            }
            (
                BuiltinCheck::Http,
                UnitSettings::Service {
                    scheme, host, port, ..
                },
            ) => self.http(scheme.as_deref(), host, *port),
            _ => unhealthy("check does not apply to this unit"),
        }
    }

    fn tcp_connect(&self, host: &str, port: u16) -> HealthResult {
        let host = host.to_owned();
        connect_within(
            move || {
                (host.as_str(), port)
                    .to_socket_addrs()
                    .map(Iterator::collect)
            },
            self.timeout,
        )
    }

    fn http(&self, scheme: Option<&str>, host: &str, port: u16) -> HealthResult {
        if scheme.is_some_and(|s| !s.eq_ignore_ascii_case("http")) {
            return unhealthy(format!("{} not supported", scheme.unwrap_or_default()));
        }
        let rt = match runtime() {
            Ok(rt) => rt,
            Err(e) => return unhealthy(format!("http error: {e}")),
        };
        let timeout = self.timeout;
        let fetch = async move { tokio::time::timeout(timeout, http_get_status(host, port)).await };
        match rt.block_on(fetch) {
            Err(_) => unhealthy("timeout"),
            Ok(Err(e)) => unhealthy(format!("http error: {e}")),
            Ok(Ok(status)) if status.is_success() => HealthResult::Healthy,
            Ok(Ok(status)) => unhealthy(format!("http status {}", status.as_u16())),
        }
    }

    /// Run `sh -c command`. Settings reach it only as environment variables.
    fn custom(&self, command: &str, ids: Option<Ids<'_>>, settings: &UnitSettings) -> HealthResult {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(command)
            .envs(settings.env_vars())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            // Own process group, so a timeout kills the whole tree.
            .process_group(0);
        if let Some(ids) = ids {
            cmd.env("CLERK_RESOURCE", ids.resource.as_str())
                .env("CLERK_UNIT", ids.unit.as_str());
        }
        let (mut child, _tracked) = match spawn_tracked(&mut cmd) {
            Ok(spawned) => spawned,
            Err(e) => return unhealthy(format!("cannot run command: {e}")),
        };
        let stderr_rx = capture_stderr(&mut child);
        let deadline = Instant::now() + self.timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() >= deadline => {
                    kill_group(&child);
                    let _ = child.wait();
                    return unhealthy("timeout");
                }
                Ok(None) => thread::sleep(Duration::from_millis(10)),
                Err(e) => {
                    kill_group(&child);
                    let _ = child.wait();
                    return unhealthy(format!("cannot wait for command: {e}"));
                }
            }
        };
        if status.success() {
            return HealthResult::Healthy;
        }
        // A background grandchild may keep the pipe open; do not wait for it.
        let stderr = stderr_rx
            .recv_timeout(Duration::from_millis(200))
            .unwrap_or_default();
        let stderr = stderr.trim();
        if stderr.is_empty() {
            unhealthy(format!("command failed ({status})"))
        } else {
            unhealthy(stderr)
        }
    }
}

/// Resolve with `resolve`, then connect to the addresses in turn, all
/// within one `budget`. Resolution runs on a helper thread so that a slow
/// resolver cannot hold the caller past the budget (the thread is left to
/// finish on its own); each connect gets only the time that is left.
fn connect_within<F>(resolve: F, budget: Duration) -> HealthResult
where
    F: FnOnce() -> io::Result<Vec<SocketAddr>> + Send + 'static,
{
    let deadline = Instant::now() + budget;
    let (tx, rx) = mpsc::channel();
    let spawned = thread::Builder::new()
        .name("clerk-resolve".to_owned())
        .spawn(move || {
            // The receiver may have given up; nothing to do then.
            let _ = tx.send(resolve());
        });
    if spawned.is_err() {
        return unhealthy("connect failed");
    }
    let Ok(Ok(addrs)) = rx.recv_timeout(budget) else {
        return unhealthy("connect failed");
    };
    for addr in addrs {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        if TcpStream::connect_timeout(&addr, left).is_ok() {
            return HealthResult::Healthy;
        }
    }
    unhealthy("connect failed")
}

/// Read the child's stderr on a helper thread; yields at most 1 KiB of text.
fn capture_stderr(child: &mut Child) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    if let Some(mut pipe) = child.stderr.take() {
        thread::spawn(move || {
            let mut kept = Vec::new();
            let mut buf = [0u8; 4096];
            // Keep reading after the limit so the child never blocks on a
            // full pipe.
            while let Ok(n) = pipe.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let room = STDERR_LIMIT.saturating_sub(kept.len());
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
            let _ = tx.send(String::from_utf8_lossy(&kept).into_owned());
        });
    }
    rx
}

fn kill_group(child: &Child) {
    let Ok(pid) = libc::pid_t::try_from(child.id()) else {
        return;
    };
    // SAFETY: `kill` has no memory-safety preconditions. The negative pid
    // addresses the process group created for this child via `process_group(0)`.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
}

/// True when something exists at `path` (following symlinks).
#[must_use]
pub fn path_exists(path: &Path) -> bool {
    fs::metadata(path).is_ok()
}

fn exists(path: &Path) -> HealthResult {
    if path_exists(path) {
        HealthResult::Healthy
    } else {
        unhealthy("path missing")
    }
}

fn accessible(path: &Path) -> HealthResult {
    // access(2), never open(2): opening a USB serial node toggles DTR/RTS,
    // which resets ESP32 boards on every check.
    let Ok(cpath) = CString::new(path.as_os_str().as_bytes()) else {
        return unhealthy("not accessible: path contains NUL");
    };
    // SAFETY: `cpath` is a valid NUL-terminated string that outlives the call.
    let rc = unsafe { libc::faccessat(libc::AT_FDCWD, cpath.as_ptr(), libc::R_OK | libc::W_OK, 0) };
    if rc == 0 {
        HealthResult::Healthy
    } else {
        unhealthy(format!("not accessible: {}", io::Error::last_os_error()))
    }
}

/// Scan `/proc/*/fd/*` for another process holding `path` open.
fn not_open(path: &Path) -> HealthResult {
    let Ok(target) = fs::metadata(path) else {
        // Nothing to hold open.
        return HealthResult::Healthy;
    };
    let is_device = target.file_type().is_char_device() || target.file_type().is_block_device();
    let Ok(procs) = fs::read_dir("/proc") else {
        return unhealthy("cannot scan /proc");
    };
    let own = std::process::id();
    for proc in procs.flatten() {
        let name = proc.file_name();
        let Some(pid) = name.to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        if pid == own {
            continue;
        }
        // Other users' fd directories are unreadable: skip silently.
        let Ok(fds) = fs::read_dir(proc.path().join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(meta) = fs::metadata(fd.path()) else {
                continue;
            };
            let same_file = meta.dev() == target.dev() && meta.ino() == target.ino();
            let same_device = is_device
                && (meta.file_type().is_char_device() || meta.file_type().is_block_device())
                && meta.rdev() == target.rdev();
            if same_file || same_device {
                let comm = fs::read_to_string(proc.path().join("comm")).unwrap_or_default();
                return unhealthy(format!("open by pid {pid} ({})", comm.trim()));
            }
        }
    }
    HealthResult::Healthy
}

fn port_free(host: &str, port: u16) -> HealthResult {
    // std's TcpListener::bind sets SO_REUSEADDR on Unix, so sockets lingering
    // in TIME_WAIT do not count as "in use"; only a live listener does.
    match TcpListener::bind((host, port)) {
        Ok(_) => HealthResult::Healthy,
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => unhealthy("port in use"),
        Err(e) => unhealthy(format!("bind failed: {e}")),
    }
}

/// Ends a task when dropped: the runtime outlives the check, and a
/// connection must not.
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A plain HTTP/1.1 `GET /`; yields the response status.
async fn http_get_status(host: &str, port: u16) -> Result<hyper::StatusCode, String> {
    let stream = tokio::net::TcpStream::connect((host, port))
        .await
        .map_err(|e| e.to_string())?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| e.to_string())?;
    let _conn = AbortOnDrop(tokio::spawn(conn));
    let req = Request::get("/")
        .header(hyper::header::HOST, host_header(host, port))
        .body(Empty::<Bytes>::new())
        .map_err(|e| e.to_string())?;
    let resp = sender.send_request(req).await.map_err(|e| e.to_string())?;
    Ok(resp.status())
}

/// `host:port`, with an IPv6 literal bracketed.
fn host_header(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUDGET: Duration = Duration::from_secs(1);
    const SLACK: Duration = Duration::from_millis(500);

    /// TEST-NET addresses: never routed, so a connect hangs until timed out.
    fn blackholes() -> Vec<SocketAddr> {
        vec![
            "192.0.2.1:9".parse().unwrap(),
            "198.51.100.7:9".parse().unwrap(),
        ]
    }

    #[test]
    fn tcp_connect_budget_covers_every_resolved_address() {
        let started = Instant::now();
        let r = connect_within(|| Ok(blackholes()), BUDGET);
        assert_eq!(r, unhealthy("connect failed"));
        assert!(
            started.elapsed() < BUDGET + SLACK,
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn tcp_connect_budget_covers_a_slow_resolver() {
        let started = Instant::now();
        let r = connect_within(
            || {
                thread::sleep(Duration::from_secs(3));
                Ok(blackholes())
            },
            BUDGET,
        );
        assert_eq!(r, unhealthy("connect failed"));
        assert!(
            started.elapsed() < BUDGET + SLACK,
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn tcp_connect_still_reaches_a_listener() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        assert_eq!(
            connect_within(move || Ok(vec![addr]), BUDGET),
            HealthResult::Healthy
        );
    }
}
