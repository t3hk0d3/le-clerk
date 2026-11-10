//! The engine: one actor that owns the store and every service.
//!
//! It runs on a dedicated OS thread, never inside a tokio runtime (the http
//! health check builds its own runtime). Async callers reach it through an
//! [`EngineHandle`], which sends jobs over a channel; each job carries a
//! oneshot reply sender. Every command runs in one store transaction.
//!
//! The loop sleeps (`recv_timeout`) until the earliest of: the next job, the
//! next scheduler deadline, the next liveness probe. After every mutating
//! command, every deadline wake-up and every probe it ticks and dispatches
//! (one transaction), then wakes notice waiters whose holders got new
//! notices. A failure there is logged and the loop keeps running; a panic in
//! a job or a tick is caught, its transaction rolled back, and reported as
//! `internal_error`.

mod command;
pub mod wire;

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;
use tokio::sync::{Notify, oneshot, watch};
use ulid::Ulid;

use crate::admin::AdminError;
use crate::clock::Clock;
use crate::config::Config;
use crate::error::ErrorCode;
use crate::inventory::{Inventory, InventoryError, PromotionError, Promotions};
use crate::model::{HolderRef, NoticeId};
use crate::scheduler::session::Sessions;
use crate::scheduler::{HealthProbe, Scheduler, SchedulerError, liveness};
use crate::store::notices::{self, Notice, NoticeFilter};
use crate::store::{Store, StoreError};

pub use command::*;

/// Longest a notice long-poll may wait.
pub const MAX_WAIT: Duration = Duration::from_secs(60);
/// Default gap between liveness probes of CLI-process holders.
pub const LIVENESS_INTERVAL: Duration = Duration::from_secs(2);
/// Most notices one take returns.
pub const NOTICE_BATCH: u32 = 100;
/// How long the engine waits before retrying after a failed tick.
const TICK_RETRY: Duration = Duration::from_secs(1);

/// The broad class of an [`ApiError`]; the server maps it to an HTTP status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// No such thing, or no such method (404).
    NotFound,
    /// Conflict, busy, refused, already held, scope not allowed, id clash
    /// (409).
    Conflict,
    /// The request or one of its values is invalid (422).
    Invalid,
    /// Storage or another internal failure (500).
    Internal,
    /// The engine is not running (503).
    Unavailable,
}

/// An error as the API reports it: `{"code": "...", "message": "...",
/// "details": {...}}`, `details` present only for codes that carry
/// structured data (see [`ErrorCode::details`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApiError {
    /// How the error maps onto a status.
    #[serde(skip)]
    pub kind: ErrorKind,
    /// Stable `snake_case` code.
    pub code: &'static str,
    /// Human-readable text.
    pub message: String,
    /// Machine-readable data about the error, for codes that have it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

impl ApiError {
    fn new(kind: ErrorKind, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            code,
            message: message.into(),
            details: None,
        }
    }

    /// `unknown_method`.
    #[must_use]
    pub fn unknown_method(method: &str) -> Self {
        Self::new(
            ErrorKind::NotFound,
            "unknown_method",
            format!("there is no method named {method:?}; check the spelling against the API"),
        )
    }

    /// `invalid_request`: the body or query does not parse.
    #[must_use]
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Invalid, "invalid_request", message)
    }

    /// `invalid_argument`: a value parses but is not allowed.
    #[must_use]
    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Invalid, "invalid_argument", message)
    }

    /// `scope_not_allowed`: an agent session asked for scopes beyond its
    /// own (every scope, an explicit scope, or another holder's identity).
    #[must_use]
    pub fn scope_not_allowed(what: &str) -> Self {
        Self::new(
            ErrorKind::Conflict,
            "scope_not_allowed",
            format!(
                "an agent session may not {what}: it sees only user scope and its own project; \
                 ask the user to do it from their terminal"
            ),
        )
    }

    /// `internal_error`.
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, "internal_error", message)
    }

    /// `engine_unavailable`: the engine thread has stopped.
    #[must_use]
    pub fn unavailable() -> Self {
        Self::new(
            ErrorKind::Unavailable,
            "engine_unavailable",
            "the clerk engine is not running; restart the daemon",
        )
    }

    fn domain(kind: ErrorKind, err: &(impl ErrorCode + std::fmt::Display)) -> Self {
        Self {
            details: err.details(),
            ..Self::new(kind, err.code(), err.to_string())
        }
    }
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        Self::domain(ErrorKind::Internal, &e)
    }
}

impl From<InventoryError> for ApiError {
    fn from(e: InventoryError) -> Self {
        let kind = match &e {
            InventoryError::NotFound => ErrorKind::NotFound,
            InventoryError::AlreadyExists
            | InventoryError::ScopeNotAllowed
            | InventoryError::Busy
            | InventoryError::DuplicateInScope { .. }
            | InventoryError::AmbiguousDuplicate { .. }
            | InventoryError::LinkConflict { .. } => ErrorKind::Conflict,
            InventoryError::InvalidId(_)
            | InventoryError::InvalidSettings { .. }
            | InventoryError::InvalidText(_)
            | InventoryError::InvalidTag(_)
            | InventoryError::ConflictingTagEdit { .. } => ErrorKind::Invalid,
            InventoryError::Store(_) => ErrorKind::Internal,
        };
        Self::domain(kind, &e)
    }
}

impl From<PromotionError> for ApiError {
    fn from(e: PromotionError) -> Self {
        let kind = match &e {
            PromotionError::NotFound => ErrorKind::NotFound,
            PromotionError::IdClash { .. } => ErrorKind::Conflict,
            PromotionError::Store(_) => ErrorKind::Internal,
        };
        Self::domain(kind, &e)
    }
}

impl From<SchedulerError> for ApiError {
    fn from(e: SchedulerError) -> Self {
        let kind = match &e {
            SchedulerError::NotFound => ErrorKind::NotFound,
            SchedulerError::AlreadyHeld
            | SchedulerError::ExtendRefused { .. }
            | SchedulerError::ExtensionUsed
            | SchedulerError::NotHolder
            | SchedulerError::Retiring
            | SchedulerError::NoUnits
            | SchedulerError::BorrowerNotOverrun { .. }
            | SchedulerError::BadState { .. } => ErrorKind::Conflict,
            SchedulerError::ExtendNotLater { .. } => ErrorKind::Invalid,
            SchedulerError::Store(_) => ErrorKind::Internal,
        };
        Self::domain(kind, &e)
    }
}

impl From<AdminError> for ApiError {
    fn from(e: AdminError) -> Self {
        match e {
            AdminError::Inventory(e) => e.into(),
            AdminError::Scheduler(e) => e.into(),
            AdminError::Store(e) => e.into(),
            AdminError::NotFound => Self::domain(ErrorKind::NotFound, &e),
            AdminError::IdClash { .. }
            | AdminError::ImportClash { .. }
            | AdminError::LinkConflict { .. }
            | AdminError::NotDirectlyLinked { .. } => Self::domain(ErrorKind::Conflict, &e),
            AdminError::Invalid(_) => Self::domain(ErrorKind::Invalid, &e),
        }
    }
}

/// Tunables that tests override; the daemon uses the defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineOptions {
    /// Gap between liveness probes of CLI-process holders.
    pub liveness_interval: Duration,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            liveness_interval: LIVENESS_INTERVAL,
        }
    }
}

type Reply<T> = oneshot::Sender<Result<T, ApiError>>;

/// Work for the engine thread.
enum Job {
    /// An RPC command.
    Command(Box<Command>, Reply<Value>),
    /// Deliver up to [`NOTICE_BATCH`] pending notices of a holder (with a
    /// filter, only those it matches).
    TakeNotices(HolderRef, Option<NoticeFilter>, Reply<Vec<Notice>>),
    /// Return up to [`NOTICE_BATCH`] pending notices of a holder without
    /// delivering them (acknowledged delivery; with a filter, only those it
    /// matches).
    ReturnNotices(HolderRef, Option<NoticeFilter>, Reply<Vec<Notice>>),
    /// Mark returned notices of a holder delivered.
    AckNotices(HolderRef, Vec<NoticeId>, Reply<usize>),
    /// Every pending notice of a holder, changing nothing.
    PendingNotices(HolderRef, Reply<Vec<Notice>>),
    /// A liveness stream of an agent session opened.
    SessionConnect(SessionConnect, Reply<()>),
    /// A liveness stream closed (nobody waits for the answer).
    SessionDisconnect(Ulid),
    /// End an agent session's leases and requests now (`SessionEnd`).
    SessionEndNow(String, Reply<()>),
    /// A live proxy session (first) takes over a resumed one (second).
    SessionAlias(String, String, Reply<()>),
    /// The session a hook or request acts for.
    SessionResolve(Option<u32>, String, Reply<String>),
    /// Whether an agent session has an open liveness stream.
    SessionAlive(String, Reply<bool>),
}

/// A liveness stream that opened.
struct SessionConnect {
    session_id: String,
    connection: Ulid,
    claude_pid: Option<u32>,
}

/// One open liveness stream of an agent session (see
/// [`EngineHandle::session_connect`]); dropping it closes the stream, which
/// starts the session's reconnect grace when it was the last one.
pub struct SessionLink {
    jobs: mpsc::Sender<Job>,
    connection: Ulid,
}

impl Drop for SessionLink {
    fn drop(&mut self) {
        // A stopped engine has nothing left to tell.
        let _ = self.jobs.send(Job::SessionDisconnect(self.connection));
    }
}

/// The canonical key of a holder: its JSON encoding, as stored with its
/// notices.
fn holder_key(holder: &HolderRef) -> String {
    // Serialising a plain enum of strings and integers cannot fail; an empty
    // key would only cause a spurious wake-up.
    serde_json::to_string(holder).unwrap_or_default()
}

/// Notice waiters, by holder key.
#[derive(Default)]
struct Waiters {
    slots: Mutex<HashMap<String, Slot>>,
}

struct Slot {
    notify: Arc<Notify>,
    /// Live `wait_notices` calls for the holder.
    waiters: usize,
    /// Of those, the ones parked on `notify` right now.
    parked: usize,
}

impl Waiters {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Slot>> {
        // The map stays consistent under every operation, so a panic while
        // it was locked leaves nothing to repair.
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn register(self: &Arc<Self>, key: String) -> Registration {
        let notify = {
            let mut slots = self.lock();
            let slot = slots.entry(key.clone()).or_insert_with(|| Slot {
                notify: Arc::new(Notify::new()),
                waiters: 0,
                parked: 0,
            });
            slot.waiters += 1;
            slot.notify.clone()
        };
        Registration {
            waiters: self.clone(),
            key,
            notify,
        }
    }

    fn notify(&self, key: &str) {
        if let Some(slot) = self.lock().get(key) {
            slot.notify.notify_waiters();
        }
    }

    fn notify_all(&self) {
        for slot in self.lock().values() {
            slot.notify.notify_waiters();
        }
    }

    fn parked(&self, key: &str) -> bool {
        self.lock().get(key).is_some_and(|s| s.parked > 0)
    }

    fn adjust(&self, key: &str, f: impl FnOnce(&mut Slot)) {
        let mut slots = self.lock();
        if let Some(slot) = slots.get_mut(key) {
            f(slot);
            if slot.waiters == 0 {
                slots.remove(key);
            }
        }
    }
}

/// One `wait_notices` call's hold on its holder's slot; dropping it (also
/// on cancellation) releases the slot.
struct Registration {
    waiters: Arc<Waiters>,
    key: String,
    notify: Arc<Notify>,
}

impl Registration {
    fn park(&self) -> Parked<'_> {
        self.waiters.adjust(&self.key, |s| s.parked += 1);
        Parked(self)
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.waiters
            .adjust(&self.key, |s| s.waiters = s.waiters.saturating_sub(1));
    }
}

/// Marks a waiter as parked while alive.
struct Parked<'a>(&'a Registration);

impl Drop for Parked<'_> {
    fn drop(&mut self) {
        self.0
            .waiters
            .adjust(&self.0.key, |s| s.parked = s.parked.saturating_sub(1));
    }
}

/// Open waits (blocking `lease_wait` calls and open MCP tasks), by holder
/// key: what the Stop hook asks through `session_brief`.
#[derive(Default)]
struct OpenWaits {
    counts: Mutex<HashMap<String, usize>>,
}

impl OpenWaits {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, usize>> {
        // Counters only; a panic while locked leaves nothing to repair.
        self.counts.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One open wait of a holder, counted while alive (see
/// [`EngineHandle::open_wait`]); dropping it, also on cancellation, closes
/// the wait.
pub struct OpenWait {
    waits: Arc<OpenWaits>,
    key: String,
}

impl Drop for OpenWait {
    fn drop(&mut self) {
        let mut counts = self.waits.lock();
        if let Some(n) = counts.get_mut(&self.key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                counts.remove(&self.key);
            }
        }
    }
}

/// A cheap, cloneable handle to a running engine.
#[derive(Clone)]
pub struct EngineHandle {
    jobs: mpsc::Sender<Job>,
    waiters: Arc<Waiters>,
    stopped: watch::Receiver<bool>,
    open_waits: Arc<OpenWaits>,
    config: Arc<Config>,
    clock: Arc<dyn Clock>,
}

impl EngineHandle {
    /// Run `command` on the engine thread.
    ///
    /// A mutating command replies as soon as its own transaction commits;
    /// the follow-up tick and dispatch run after the reply. The engine is
    /// single-threaded, so the next command (from any caller) still observes
    /// their effects.
    ///
    /// # Errors
    /// The command's [`ApiError`], or `engine_unavailable` when the engine
    /// has stopped.
    pub async fn call(&self, command: Command) -> Result<Value, ApiError> {
        let (reply, answer) = oneshot::channel();
        self.jobs
            .send(Job::Command(Box::new(command), reply))
            .map_err(|_| ApiError::unavailable())?;
        answer.await.map_err(|_| ApiError::unavailable())?
    }

    /// Deliver `holder`'s pending notices, oldest first, waiting up to
    /// `timeout` (capped at [`MAX_WAIT`]) for one to arrive. Returns an
    /// empty list on timeout.
    ///
    /// # Errors
    /// A storage failure, or `engine_unavailable`.
    pub async fn wait_notices(
        &self,
        holder: HolderRef,
        timeout: Duration,
    ) -> Result<Vec<Notice>, ApiError> {
        self.wait_notices_matching(holder, None, timeout).await
    }

    /// [`EngineHandle::wait_notices`], but with `filter` only the notices
    /// it matches are delivered (and waited for); the holder's others stay
    /// pending.
    ///
    /// # Errors
    /// As [`EngineHandle::wait_notices`].
    pub async fn wait_notices_matching(
        &self,
        holder: HolderRef,
        filter: Option<NoticeFilter>,
        timeout: Duration,
    ) -> Result<Vec<Notice>, ApiError> {
        let got = self
            .wait_until(&holder, timeout.min(MAX_WAIT), || async {
                let got = self.take(holder.clone(), filter.clone()).await?;
                Ok((!got.is_empty()).then_some(got))
            })
            .await?;
        Ok(got.unwrap_or_default())
    }

    /// Run `check` now and again whenever `holder` gets a notice, until it
    /// answers `Some`, for at most `timeout`; `None` on timeout (after one
    /// last check). Dropping the future (a client gone) ends the wait.
    ///
    /// # Errors
    /// The first error `check` returns.
    pub async fn wait_until<T, F, Fut>(
        &self,
        holder: &HolderRef,
        timeout: Duration,
        mut check: F,
    ) -> Result<Option<T>, ApiError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<Option<T>, ApiError>>,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        // Registered before the first check, so a notice published between
        // the check and the wait is never missed.
        let registration = self.waiters.register(holder_key(holder));
        loop {
            let notified = registration.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(found) = check().await? {
                return Ok(Some(found));
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(None);
            }
            let _parked = registration.park();
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return Ok(None);
            }
        }
    }

    /// Up to [`NOTICE_BATCH`] pending notices of `holder`, oldest first,
    /// returned **without** delivering them: they stay pending until
    /// [`EngineHandle::ack_notices`] names them (acknowledged delivery, for
    /// MCP). With `filter`, only the notices it matches.
    ///
    /// # Errors
    /// A storage failure, or `engine_unavailable`.
    pub async fn return_notices(
        &self,
        holder: HolderRef,
        filter: Option<NoticeFilter>,
    ) -> Result<Vec<Notice>, ApiError> {
        self.ask(|reply| Job::ReturnNotices(holder, filter, reply))
            .await
    }

    /// Mark delivered those of `ids` that `holder` was returned; the others
    /// are ignored. Returns how many were marked.
    ///
    /// # Errors
    /// A storage failure, or `engine_unavailable`.
    pub async fn ack_notices(
        &self,
        holder: HolderRef,
        ids: Vec<NoticeId>,
    ) -> Result<usize, ApiError> {
        self.ask(|reply| Job::AckNotices(holder, ids, reply)).await
    }

    /// Every pending (undelivered) notice of `holder`, oldest first,
    /// changing nothing.
    ///
    /// # Errors
    /// A storage failure, or `engine_unavailable`.
    pub async fn pending_notices(&self, holder: HolderRef) -> Result<Vec<Notice>, ApiError> {
        self.ask(|reply| Job::PendingNotices(holder, reply)).await
    }

    /// Count an open wait of `holder` (a blocking `lease_wait`, or an open
    /// MCP task) until the returned guard is dropped.
    #[must_use]
    pub fn open_wait(&self, holder: &HolderRef) -> OpenWait {
        let key = holder_key(holder);
        *self.open_waits.lock().entry(key.clone()).or_insert(0) += 1;
        OpenWait {
            waits: self.open_waits.clone(),
            key,
        }
    }

    /// How many waits of `holder` are open now (see
    /// [`EngineHandle::open_wait`]).
    #[must_use]
    pub fn open_waits(&self, holder: &HolderRef) -> usize {
        self.open_waits
            .lock()
            .get(&holder_key(holder))
            .copied()
            .unwrap_or(0)
    }

    /// A liveness stream of agent session `session_id` opened on
    /// `connection` (a `clerk mcp` process) for Claude Code process
    /// `claude_pid`: the session is alive and its grace, if any, cancelled,
    /// until the returned link is dropped.
    ///
    /// # Errors
    /// `engine_unavailable`.
    pub async fn session_connect(
        &self,
        session_id: String,
        connection: Ulid,
        claude_pid: Option<u32>,
    ) -> Result<SessionLink, ApiError> {
        let connect = SessionConnect {
            session_id,
            connection,
            claude_pid,
        };
        self.ask(|reply| Job::SessionConnect(connect, reply))
            .await?;
        Ok(SessionLink {
            jobs: self.jobs.clone(),
            connection,
        })
    }

    /// `SessionEnd`: end agent session `session_id`'s leases and requests
    /// now, without grace.
    ///
    /// # Errors
    /// A storage failure, or `engine_unavailable`.
    pub async fn session_end_now(&self, session_id: String) -> Result<(), ApiError> {
        self.ask(|reply| Job::SessionEndNow(session_id, reply))
            .await
    }

    /// `SessionStart` resume: the live proxy session `from` takes over the
    /// resumed session `to` (its leases, requests and pending notices; its
    /// grace is cancelled; `to` resolves to `from` from now on).
    ///
    /// # Errors
    /// A storage failure, or `engine_unavailable`.
    pub async fn session_alias(&self, from: String, to: String) -> Result<(), ApiError> {
        self.ask(|reply| Job::SessionAlias(from, to, reply)).await
    }

    /// The session a hook or request of `session_id` from Claude Code
    /// process `claude_pid` acts for: the live proxy session with that PID,
    /// else `session_id` after alias resolution.
    ///
    /// # Errors
    /// `engine_unavailable`.
    pub async fn session_resolve(
        &self,
        claude_pid: Option<u32>,
        session_id: String,
    ) -> Result<String, ApiError> {
        self.ask(|reply| Job::SessionResolve(claude_pid, session_id, reply))
            .await
    }

    /// Whether agent session `session_id` has an open liveness stream.
    ///
    /// # Errors
    /// `engine_unavailable`.
    pub async fn session_alive(&self, session_id: String) -> Result<bool, ApiError> {
        self.ask(|reply| Job::SessionAlive(session_id, reply)).await
    }

    /// The configuration the engine runs with.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The engine's clock, unix milliseconds.
    #[must_use]
    pub fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }

    /// True while a [`EngineHandle::wait_notices`] for `holder` is parked,
    /// waiting for a notice.
    #[must_use]
    pub fn is_waiting(&self, holder: &HolderRef) -> bool {
        self.waiters.parked(&holder_key(holder))
    }

    /// Resolves once the engine thread has stopped (after `daemon_stop`, or
    /// when every handle is gone).
    pub async fn stopped(&self) {
        let mut stopped = self.stopped.clone();
        // An error means the engine dropped its sender: it has stopped.
        let _ = stopped.wait_for(|s| *s).await;
    }

    async fn take(
        &self,
        holder: HolderRef,
        filter: Option<NoticeFilter>,
    ) -> Result<Vec<Notice>, ApiError> {
        self.ask(|reply| Job::TakeNotices(holder, filter, reply))
            .await
    }

    /// Send the job `make` builds around a reply sender; its answer.
    async fn ask<T>(&self, make: impl FnOnce(Reply<T>) -> Job) -> Result<T, ApiError> {
        let (reply, answer) = oneshot::channel();
        self.jobs
            .send(make(reply))
            .map_err(|_| ApiError::unavailable())?;
        answer.await.map_err(|_| ApiError::unavailable())?
    }
}

/// When the engine thread ends, however it ends: sets the stopped flag and
/// wakes every parked long-poll, whose next take then fails at once with
/// `engine_unavailable`. The job inbox is gone by then (`run` owns it).
struct StopSignal {
    stopped: watch::Sender<bool>,
    waiters: Arc<Waiters>,
}

impl Drop for StopSignal {
    fn drop(&mut self) {
        self.stopped.send_replace(true);
        self.waiters.notify_all();
    }
}

/// The engine actor. Create one with [`Engine::spawn`].
pub struct Engine {
    store: Store,
    clock: Arc<dyn Clock>,
    inventory: Inventory,
    promotions: Promotions,
    scheduler: Scheduler,
    sessions: Sessions,
    waiters: Arc<Waiters>,
    options: EngineOptions,
    /// The highest notice row whose holder has been woken.
    notices_seen: i64,
    /// The next scheduler deadline, unix ms.
    next_deadline: Option<i64>,
    stopping: bool,
}

impl Engine {
    /// Start the engine on its own thread with default options.
    ///
    /// # Errors
    /// [`io::Error`] when the thread cannot be started.
    pub fn spawn(
        store: Store,
        clock: Arc<dyn Clock>,
        config: Config,
        health: Arc<dyn HealthProbe>,
    ) -> io::Result<EngineHandle> {
        Self::spawn_with(store, clock, config, health, EngineOptions::default())
    }

    /// [`Engine::spawn`] with explicit options.
    ///
    /// # Errors
    /// [`io::Error`] when the thread cannot be started.
    pub fn spawn_with(
        store: Store,
        clock: Arc<dyn Clock>,
        config: Config,
        health: Arc<dyn HealthProbe>,
        options: EngineOptions,
    ) -> io::Result<EngineHandle> {
        let (jobs, inbox) = mpsc::channel();
        let (stop_tx, stopped) = watch::channel(false);
        let waiters = Arc::new(Waiters::default());
        let handle_config = Arc::new(config.clone());
        let handle_clock = clock.clone();
        let sessions = Sessions::new(Duration::from_secs(config.reconnect_grace));
        let engine = Self {
            store,
            inventory: Inventory::new(clock.clone()),
            promotions: Promotions::new(clock.clone()),
            scheduler: Scheduler::new(clock.clone(), config, health),
            sessions,
            clock,
            waiters: waiters.clone(),
            options,
            notices_seen: 0,
            next_deadline: None,
            stopping: false,
        };
        let signal_waiters = waiters.clone();
        thread::Builder::new()
            .name("clerk-engine".to_owned())
            .spawn(move || {
                let _signal = StopSignal {
                    stopped: stop_tx,
                    waiters: signal_waiters,
                };
                engine.run(inbox);
            })?;
        Ok(EngineHandle {
            jobs,
            waiters,
            stopped,
            open_waits: Arc::new(OpenWaits::default()),
            config: handle_config,
            clock: handle_clock,
        })
    }

    fn run(mut self, inbox: mpsc::Receiver<Job>) {
        self.notices_seen = self.store.tx(notices::last_row).unwrap_or_else(|e| {
            tracing::error!(error = %e, "cannot read the notice log; waking from the start");
            0
        });
        self.boot_sessions();
        self.maintain();
        self.wake_waiters();
        let mut next_probe = Instant::now() + self.options.liveness_interval;
        loop {
            match inbox.recv_timeout(self.sleep_for(next_probe)) {
                Ok(job) => self.handle(job),
                Err(RecvTimeoutError::Timeout) => self.maintain(),
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if Instant::now() >= next_probe {
                self.probe();
                next_probe = Instant::now() + self.options.liveness_interval;
            }
            if self.deadline_due() {
                self.maintain();
            }
            if self.sessions_due() {
                self.expire_sessions();
            }
            self.wake_waiters();
            if self.stopping {
                tracing::info!("engine stopping on request");
                break;
            }
        }
        // Close the inbox before the stop signal wakes parked long-polls, so
        // their next take fails at once instead of queueing.
        drop(inbox);
    }

    /// How long to wait for a job: until the next deadline, session grace
    /// end or probe.
    fn sleep_for(&self, next_probe: Instant) -> Duration {
        let probe = next_probe.saturating_duration_since(Instant::now());
        let now = self.clock.now_ms();
        [self.next_deadline, self.sessions.next_expiry()]
            .into_iter()
            .flatten()
            .map(|at| Duration::from_millis(u64::try_from(at.saturating_sub(now)).unwrap_or(0)))
            .fold(probe, Duration::min)
    }

    fn sessions_due(&self) -> bool {
        self.sessions
            .next_expiry()
            .is_some_and(|at| at <= self.clock.now_ms())
    }

    /// Daemon start: every agent session with leases or requests gets the
    /// reconnect grace from now.
    fn boot_sessions(&mut self) {
        let result = self.guarded("session boot", |engine| {
            let now = engine.clock.now_ms();
            let sessions = &mut engine.sessions;
            Ok(engine.store.tx(|tx| sessions.boot(tx, now))?)
        });
        match result {
            Ok(graced) if graced.is_empty() => {}
            Ok(graced) => tracing::info!(?graced, "agent sessions get the reconnect grace"),
            Err(e) => tracing::error!(error = %e, "cannot apply the agent-session grace"),
        }
    }

    /// End the agent sessions whose reconnect grace is over.
    fn expire_sessions(&mut self) {
        let result = self.guarded("session expiry", |engine| {
            let now = engine.clock.now_ms();
            let (sessions, sched) = (&mut engine.sessions, &engine.scheduler);
            Ok(engine.store.tx(|tx| sessions.expire(tx, sched, now))?)
        });
        match result {
            Ok(ended) if ended.is_empty() => {}
            Ok(ended) => {
                tracing::info!(
                    ?ended,
                    "agent sessions gone; their leases and requests ended"
                );
                self.maintain();
            }
            Err(e) => tracing::error!(error = %e, "agent-session expiry failed"),
        }
    }

    fn deadline_due(&self) -> bool {
        self.next_deadline
            .is_some_and(|at| at <= self.clock.now_ms())
    }

    fn handle(&mut self, job: Job) {
        match job {
            Job::Command(command, reply) => {
                // Reads see overdue deadlines processed.
                if self.deadline_due() {
                    self.maintain();
                }
                let method = command.method();
                let mutating = !command.read_only();
                let stop = matches!(*command, Command::DaemonStop(..));
                let result = self.guarded(method, |engine| engine.execute(*command));
                if stop && result.is_ok() {
                    self.stopping = true;
                }
                // The caller may have gone away; nothing to do then.
                let _ = reply.send(result);
                if mutating {
                    self.maintain();
                }
            }
            Job::TakeNotices(holder, filter, reply) => {
                let result = self.guarded("take_notices", |engine| {
                    let now = engine.clock.now_ms();
                    Ok(engine.store.tx(|tx| {
                        notices::take_matching(tx, &holder, filter.as_ref(), NOTICE_BATCH, now)
                    })?)
                });
                let _ = reply.send(result);
            }
            Job::ReturnNotices(holder, filter, reply) => {
                let result = self.guarded("return_notices", |engine| {
                    let now = engine.clock.now_ms();
                    Ok(engine.store.tx(|tx| {
                        notices::returned(tx, &holder, filter.as_ref(), NOTICE_BATCH, now)
                    })?)
                });
                let _ = reply.send(result);
            }
            Job::AckNotices(holder, ids, reply) => {
                let result = self.guarded("ack_notices", |engine| {
                    let now = engine.clock.now_ms();
                    Ok(engine.store.tx(|tx| notices::ack(tx, &holder, &ids, now))?)
                });
                let _ = reply.send(result);
            }
            Job::PendingNotices(holder, reply) => {
                let result = self.guarded("pending_notices", |engine| {
                    Ok(engine.store.tx(|tx| notices::pending(tx, &holder))?)
                });
                let _ = reply.send(result);
            }
            Job::SessionConnect(c, reply) => {
                self.sessions
                    .connect(&c.session_id, c.connection, c.claude_pid);
                let _ = reply.send(Ok(()));
            }
            Job::SessionDisconnect(connection) => {
                let now = self.clock.now_ms();
                self.sessions.disconnect(connection, now);
            }
            Job::SessionEndNow(session_id, reply) => {
                let result = self.guarded("session_end_now", |engine| {
                    let (sessions, sched) = (&mut engine.sessions, &engine.scheduler);
                    engine
                        .store
                        .tx(|tx| sessions.end_now(tx, sched, &session_id))?;
                    Ok(())
                });
                let _ = reply.send(result);
                self.maintain();
            }
            Job::SessionAlias(from, to, reply) => {
                let result = self.guarded("session_alias", |engine| {
                    let (sessions, sched) = (&mut engine.sessions, &engine.scheduler);
                    Ok(engine
                        .store
                        .tx(|tx| sessions.alias(tx, sched, &from, &to))?)
                });
                let _ = reply.send(result);
                self.maintain();
            }
            Job::SessionResolve(claude_pid, session_id, reply) => {
                let _ = reply.send(Ok(self.sessions.resolve(claude_pid, &session_id)));
            }
            Job::SessionAlive(session_id, reply) => {
                let _ = reply.send(Ok(self.sessions.is_alive(&session_id)));
            }
        }
    }

    /// Run `f`, turning a panic into `internal_error`. The panicking
    /// transaction was dropped, so it rolled back.
    fn guarded<T>(
        &mut self,
        what: &str,
        f: impl FnOnce(&mut Self) -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        panic::catch_unwind(AssertUnwindSafe(|| f(self))).unwrap_or_else(|_| {
            tracing::error!(what, "engine job panicked; its transaction was rolled back");
            Err(ApiError::internal(format!("{what} failed unexpectedly")))
        })
    }

    fn execute(&mut self, command: Command) -> Result<Value, ApiError> {
        let services = Services {
            inventory: &self.inventory,
            promotions: &self.promotions,
            scheduler: &self.scheduler,
            now_ms: self.clock.now_ms(),
        };
        self.store.tx(|tx| command.run(tx, &services))
    }

    /// Process due deadlines and dispatch, in one transaction, and remember
    /// the next deadline. On failure, retry after [`TICK_RETRY`].
    fn maintain(&mut self) {
        let result = self.guarded("tick", |engine| {
            let sched = &engine.scheduler;
            Ok(engine.store.tx(|tx| {
                sched.tick(tx)?;
                sched.dispatch(tx)?;
                // Dispatch may have made new deadlines (claim windows).
                sched.tick(tx)
            })?)
        });
        self.next_deadline = match result {
            Ok(next) => next,
            Err(e) => {
                tracing::error!(error = %e, "tick failed; retrying shortly");
                let retry = i64::try_from(TICK_RETRY.as_millis()).unwrap_or(i64::MAX);
                Some(self.clock.now_ms().saturating_add(retry))
            }
        };
    }

    /// End the leases and requests of CLI-process holders that are gone.
    fn probe(&mut self) {
        let result = self.guarded("liveness probe", |engine| {
            let sched = &engine.scheduler;
            Ok(engine.store.tx(|tx| liveness::probe_all(tx, sched))?)
        });
        match result {
            Ok(gone) if gone.is_empty() => {}
            Ok(gone) => {
                tracing::info!(?gone, "holders gone; their leases and requests ended");
                self.maintain();
            }
            Err(e) => tracing::error!(error = %e, "liveness probe failed"),
        }
    }

    /// Wake the waiters of every holder that got notices since last time.
    fn wake_waiters(&mut self) {
        let result = self.guarded("wake waiters", |engine| {
            let seen = engine.notices_seen;
            let (last, holders) = engine.store.tx(|tx| {
                let last = notices::last_row(tx)?;
                let holders = if last > seen {
                    notices::holders_after(tx, seen)?
                } else {
                    Vec::new()
                };
                Ok::<_, StoreError>((last, holders))
            })?;
            engine.notices_seen = last;
            for holder in &holders {
                engine.waiters.notify(&holder_key(holder));
            }
            Ok(())
        });
        if let Err(e) = result {
            tracing::error!(error = %e, "cannot read new notices; waiters sleep on");
        }
    }
}
