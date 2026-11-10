//! Running the daemon: the single-instance lock, the private socket, the
//! engine, serving, and a clean shutdown.
//!
//! Start-up order: create the run directory (0700), take the exclusive lock
//! (or exit quietly: another daemon won the race), remove a stale socket,
//! bind the socket (0600), open the database, apply the restart grace, spawn
//! the engine (whose first act is a tick), then serve until SIGTERM, SIGINT
//! or `daemon_stop`. On the way out the engine is stopped, the socket
//! removed, and only then the lock released, so a successor never finds a
//! socket that belongs to nobody it could not remove.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fs4::fs_std::FileExt;
use thiserror::Error;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::oneshot;

use crate::clock::{Clock, SystemClock};
use crate::config::{Config, Paths, SocketPathTooLong, check_socket_path};
use crate::daemon::server::{self, ServerInfo};
use crate::engine::{Command, Engine, EngineHandle, NoArgs};
use crate::health::HealthChecker;
use crate::model::{Caller, HolderRef};
use crate::scheduler::{Scheduler, SchedulerError};
use crate::store::{Store, StoreError};

/// Test hook: when set, the daemon reports this version instead of its own
/// (to exercise the client's version check).
pub const VERSION_OVERRIDE_ENV: &str = "CLERK_TEST_DAEMON_VERSION";

/// How long in-flight requests may take, against the live engine, once the
/// daemon stops accepting connections.
const DRAIN_GRACE: Duration = Duration::from_secs(2);
/// How long connections may take to close once the engine has stopped.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// Why the daemon could not run.
#[derive(Debug, Error)]
pub enum RunError {
    /// A file or directory could not be prepared.
    #[error("cannot prepare {path}: {source}")]
    Io {
        /// The path concerned.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// The socket path is too long for a Unix socket.
    #[error(transparent)]
    SocketPathTooLong(#[from] SocketPathTooLong),
    /// The database could not be opened.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The restart grace could not be applied.
    #[error("cannot apply the restart grace: {0}")]
    Grace(#[from] SchedulerError),
    /// The engine thread or the async runtime could not start.
    #[error("cannot start: {0}")]
    Start(io::Error),
    /// Serving failed.
    #[error("serving failed: {0}")]
    Serve(io::Error),
}

fn io_at(path: &Path) -> impl FnOnce(io::Error) -> RunError + '_ {
    move |source| RunError::Io {
        path: path.to_owned(),
        source,
    }
}

/// Run the daemon in the foreground until it is told to stop. Returns
/// `Ok` at once, doing nothing, when another daemon holds the lock.
///
/// Logs go to `paths.log`; nothing is written to stdout.
///
/// # Errors
/// [`RunError`] when a file, the database, the engine or the server fails.
pub fn run(paths: &Paths, config: Config) -> Result<(), RunError> {
    check_socket_path(&paths.socket)?;
    let run_dir = parent(&paths.socket);
    create_private_dir(run_dir)?;
    let Some(lock) = take_lock(&paths.lock)? else {
        return Ok(());
    };
    init_logging(&paths.log)?;
    let info = ServerInfo {
        version: std::env::var(VERSION_OVERRIDE_ENV).unwrap_or_else(|_| crate::VERSION.into()),
        pid: std::process::id(),
    };
    tracing::info!(pid = info.pid, version = %info.version, "daemon starting");

    remove_stale_socket(&paths.socket)?;
    let listener = bind_private(&paths.socket)?;
    let result = start_and_serve(paths, config, listener, &info);
    match &result {
        Ok(()) => tracing::info!("daemon stopped"),
        Err(e) => tracing::error!(error = %e, "daemon failed"),
    }
    if let Err(e) = fs::remove_file(&paths.socket)
        && e.kind() != io::ErrorKind::NotFound
    {
        tracing::warn!(error = %e, "cannot remove the socket");
    }
    // Released last: a successor may start only once the socket is gone.
    drop(lock);
    result
}

fn start_and_serve(
    paths: &Paths,
    config: Config,
    listener: UnixListener,
    info: &ServerInfo,
) -> Result<(), RunError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(RunError::Start)?;
    // Signal handlers go in before the slow part of start-up, so a SIGTERM
    // during the database open or the grace pass is caught (and served as
    // a clean stop) rather than killing the process half-way.
    let signals = {
        let _context = runtime.enter();
        Signals::install().map_err(RunError::Start)?
    };
    let mut store = Store::open(&paths.db)?;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let health = Arc::new(HealthChecker::new(Duration::from_secs(
        config.health_timeout,
    )));
    let grace = Scheduler::new(clock.clone(), config.clone(), health.clone());
    let graced = store.tx(|tx| grace.restart_grace(tx))?;
    if graced > 0 {
        tracing::info!(leases = graced, "restart grace applied");
    }
    let engine = Engine::spawn(store, clock, config, health).map_err(RunError::Start)?;
    runtime.block_on(serve(listener, engine, info, signals))
}

/// SIGTERM and SIGINT streams.
struct Signals {
    term: tokio::signal::unix::Signal,
    int: tokio::signal::unix::Signal,
}

impl Signals {
    /// Must run inside a tokio runtime context.
    fn install() -> io::Result<Self> {
        Ok(Self {
            term: signal(SignalKind::terminate())?,
            int: signal(SignalKind::interrupt())?,
        })
    }
}

/// Serve `engine` on `listener` until a signal or an engine stop.
///
/// Shutdown order: stop accepting connections; give in-flight requests
/// [`DRAIN_GRACE`] to finish against the live engine; stop the engine
/// (finishing the command in flight and waking parked long-polls, which
/// then return); give the remaining connections [`SHUTDOWN_GRACE`]; wait
/// for the engine thread to end.
async fn serve(
    listener: UnixListener,
    engine: EngineHandle,
    info: &ServerInfo,
    mut signals: Signals,
) -> Result<(), RunError> {
    listener.set_nonblocking(true).map_err(RunError::Serve)?;
    let listener = tokio::net::UnixListener::from_std(listener).map_err(RunError::Serve)?;
    let (begun, shutdown_begun) = oneshot::channel::<()>();

    let watcher = engine.clone();
    let shutdown = async move {
        tokio::select! {
            _ = signals.term.recv() => tracing::info!("SIGTERM received; stopping"),
            _ = signals.int.recv() => tracing::info!("SIGINT received; stopping"),
            () = watcher.stopped() => tracing::info!("engine stopped; stopping"),
        }
        let _ = begun.send(());
    };

    let app = server::router_with(engine.clone(), info);
    let serving = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .into_future();
    tokio::pin!(serving);
    tokio::select! {
        result = &mut serving => result.map_err(RunError::Serve)?,
        () = async {
            if shutdown_begun.await.is_err() {
                // The shutdown future went away with the server.
                std::future::pending::<()>().await;
            }
            tokio::time::sleep(DRAIN_GRACE).await;
            stop_engine(&engine).await;
            tokio::time::sleep(SHUTDOWN_GRACE).await;
        } => tracing::warn!("connections still open after the shutdown grace; closing them"),
    }
    stop_engine(&engine).await;
    Ok(())
}

/// Stop the engine (a no-op once it has stopped) and wait for its thread.
async fn stop_engine(engine: &EngineHandle) {
    let _ = engine
        .call(Command::DaemonStop(daemon_caller(), NoArgs {}))
        .await;
    engine.stopped().await;
}

/// The actor recorded when the daemon stops itself (on a signal).
fn daemon_caller() -> Caller {
    Caller {
        holder: HolderRef::User {
            tag: "clerk".to_owned(),
        },
        project: None,
        override_reason: None,
    }
}

fn parent(path: &Path) -> &Path {
    path.parent().unwrap_or_else(|| Path::new("."))
}

/// Create `dir` (and its parents) and make it private to this user.
fn create_private_dir(dir: &Path) -> Result<(), RunError> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(io_at(dir))?;
    // An existing directory keeps its mode; tighten it.
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(io_at(dir))
}

/// Take the exclusive daemon lock without blocking and record this pid in
/// it. `None` when another process holds it.
fn take_lock(path: &Path) -> Result<Option<File>, RunError> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(io_at(path))?;
    match FileExt::try_lock_exclusive(&file) {
        Ok(true) => {
            record_pid(&file).map_err(io_at(path))?;
            Ok(Some(file))
        }
        Ok(false) => Ok(None),
        Err(e) => Err(io_at(path)(e)),
    }
}

/// Write this process's pid into the lock file it holds, so clients can
/// name an unresponsive daemon.
fn record_pid(mut file: &File) -> io::Result<()> {
    file.set_len(0)?;
    file.seek(io::SeekFrom::Start(0))?;
    writeln!(file, "{}", std::process::id())
}

/// Send `tracing` output to the daemon log (appending).
fn init_logging(log: &Path) -> Result<(), RunError> {
    fs::create_dir_all(parent(log)).map_err(io_at(log))?;
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .map_err(io_at(log))?;
    // Fails only when a subscriber is already installed; keep that one.
    let _ = tracing_subscriber::fmt()
        .with_writer(Mutex::new(file))
        .try_init();
    Ok(())
}

/// Remove a socket file left by a daemon that died. Only called while
/// holding the lock, so no live daemon owns it.
fn remove_stale_socket(path: &Path) -> Result<(), RunError> {
    match fs::remove_file(path) {
        Ok(()) => {
            tracing::info!(socket = %path.display(), "removed a stale socket");
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_at(path)(e)),
    }
}

/// Bind the socket so that it is never accessible to anyone but this user:
/// created under a 0177 umask, then set to 0600 explicitly.
fn bind_private(path: &Path) -> Result<UnixListener, RunError> {
    // SAFETY: umask only swaps the process file-mode mask. No other thread
    // exists yet (the engine and the runtime start later), so nothing else
    // creates files under the temporary mask.
    let old = unsafe { libc::umask(0o177) };
    let bound = UnixListener::bind(path);
    // SAFETY: as above; restores the previous mask.
    unsafe { libc::umask(old) };
    let listener = bound.map_err(io_at(path))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(io_at(path))?;
    Ok(listener)
}
