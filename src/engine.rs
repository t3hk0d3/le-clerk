//! The engine: one actor that owns the store and every service.
//!
//! It runs on a dedicated OS thread, never inside a tokio runtime. Async
//! callers reach it through an [`EngineHandle`], which sends jobs over a
//! channel; each job carries a oneshot reply sender. Every command runs in
//! one store transaction. Health checks run off this thread in a
//! [`CheckPool`]; each result comes back as a job and re-enters dispatch.
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

use crate::admin::AdminError;
use crate::clock::Clock;
use crate::config::Config;
use crate::error::ErrorCode;
use crate::health::HealthResult;
use crate::inventory::{Inventory, InventoryError, PromotionError, Promotions};
use crate::model::{HolderRef, Host, NoticeId};
use crate::scheduler::checks::{CheckPool, CheckTicket};
use crate::scheduler::session::{Hosting, Sessions};
use crate::scheduler::{HealthProbe, Scheduler, SchedulerError, liveness};
use crate::store::notices::{self, Notice, NoticeFilter};
use crate::store::retention::{self, Retention, Swept};
use crate::store::{Store, StoreError};

pub use command::*;

/// Longest a notice long-poll may wait.
pub const MAX_WAIT: Duration = Duration::from_secs(60);
/// Default gap between liveness probes of CLI-process holders.
pub const LIVENESS_INTERVAL: Duration = Duration::from_secs(2);
/// Most notices one take returns.
pub const NOTICE_BATCH: u32 = 100;
/// How much longer than `health_timeout` an acquire waits for a check.
const CHECK_WAIT_SLACK: Duration = Duration::from_secs(2);
/// Gap between retention sweeps.
const RETENTION_INTERVAL: Duration = Duration::from_hours(24);
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

    /// `not_yours`: the caller asked for what is another holder's.
    #[must_use]
    pub fn not_yours(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Conflict, "not_yours", message)
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

    /// `session_not_hosted`: `session` may not hold anything, because no
    /// running process is known to run it, or (`hosting`) the calling
    /// process runs another session.
    #[must_use]
    pub fn session_not_hosted(session: &str, hosting: &Hosting) -> Self {
        let why = match hosting {
            Hosting::HostRunsOther(other) => {
                format!("the calling process runs agent session {other} now")
            }
            Hosting::Hosted | Hosting::NotHosted => {
                "the clerk knows no running process for it".to_owned()
            }
        };
        Self::new(
            ErrorKind::Conflict,
            "session_not_hosted",
            format!("agent session {session} cannot hold leases: {why}"),
        )
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

    /// `override_required`: an agent session called something that needs the
    /// user's say-so, and gave no reason. The reason is client-asserted: this
    /// guards against mistakes, not hostile processes.
    #[must_use]
    pub fn override_required(effect: &str) -> Self {
        Self::new(
            ErrorKind::Conflict,
            "override_required",
            format!("Refused: an agent must pass --override \"<reason>\" to {effect}."),
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
            | SchedulerError::NotYours { .. }
            | SchedulerError::Retiring
            | SchedulerError::NoUnits
            | SchedulerError::BorrowerNotOverrun { .. }
            | SchedulerError::BadState { .. }
            | SchedulerError::DeviceAbsent { .. } => ErrorKind::Conflict,
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

/// See [`Job::Command`].
type DeferOnCheck = bool;

/// Work for the engine thread.
enum Job {
    /// An RPC command. With `DeferOnCheck`, an acquire answers `checking`
    /// rather than queue behind a health check on its way.
    Command(Box<Command>, Option<Host>, DeferOnCheck, Reply<Value>),
    /// Deliver up to [`NOTICE_BATCH`] pending notices of a holder (with a
    /// filter, only those it matches). In-process only: nothing on the wire
    /// reaches it, as a lost answer would lose the notices.
    TakeNotices(HolderRef, Option<NoticeFilter>, Reply<Vec<Notice>>),
    /// Return up to [`NOTICE_BATCH`] pending notices of a holder without
    /// delivering them (acknowledged delivery; with a filter, only those it
    /// matches).
    ReturnNotices(HolderRef, Option<NoticeFilter>, Reply<Vec<Notice>>),
    /// Mark returned notices of a holder delivered.
    AckNotices(HolderRef, Vec<NoticeId>, Reply<usize>),
    /// Every pending notice of a holder, changing nothing.
    PendingNotices(HolderRef, Reply<Vec<Notice>>),
    /// An agent call or hook reported the process running a session.
    ReportHost(String, Host, Reply<()>),
    /// May a notice wait of a session from a host run? Only while the host
    /// does not run another session ([`Sessions::touch`]).
    AdmitWait(String, Host, Reply<bool>),
    /// Whether an agent session has a running host.
    SessionAlive(String, Reply<bool>),
    /// A Claude Code hook's RPC.
    Hook(Box<HookCommand>, Reply<HookAnswer>),
    /// A health check came back.
    CheckDone {
        unit: CheckTicket,
        result: HealthResult,
    },
}

/// What one step of an agent session's notice wait came to
/// ([`EngineHandle::agent_wait`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", content = "notices", rename_all = "snake_case")]
pub enum AgentWait {
    /// Pending notices, oldest first (not yet delivered).
    Notices(Vec<Notice>),
    /// None arrived in time.
    Timeout,
    /// Another wait of the session is open.
    Armed,
    /// The session's process runs another session now, so this wait is
    /// over.
    Ended,
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

/// Open waits (blocking notice waits), by holder key, and each agent
/// session's armed wait: what the Stop hook asks through `session_brief`.
#[derive(Default)]
struct OpenWaits {
    counts: Mutex<HashMap<String, usize>>,
    /// The armed `clerk wait` of each holder: its arm token, and until when
    /// it counts as armed (through the gaps between its steps).
    arms: Mutex<HashMap<String, (String, tokio::time::Instant)>>,
}

impl OpenWaits {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, usize>> {
        // Counters only; a panic while locked leaves nothing to repair.
        self.counts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn arms(&self) -> MutexGuard<'_, HashMap<String, (String, tokio::time::Instant)>> {
        // Tokens and deadlines only; nothing to repair after a panic.
        self.arms.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Arm `key`'s wait for `arm` until `until`, checking and inserting
    /// under one lock: `false` when another token holds a live arm, or
    /// when `first` and a blocking wait is open with no arm of its own.
    fn try_arm(&self, key: &str, arm: &str, first: bool, until: tokio::time::Instant) -> bool {
        let now = tokio::time::Instant::now();
        let mut arms = self.arms();
        let live = arms.get(key).filter(|(_, until)| *until > now);
        if live.is_some_and(|(owner, _)| owner != arm) {
            return false;
        }
        // Lock order: arms, then counts (nothing takes them the other way).
        if first && live.is_none() && self.lock().get(key).is_some_and(|n| *n > 0) {
            return false;
        }
        arms.insert(key.to_owned(), (arm.to_owned(), until));
        true
    }

    /// Hold `key`'s wait for `arm` until `until`, only while `arm` still
    /// owns it: a disarmed or taken-over arm is not re-created.
    fn extend_if_owner(&self, key: &str, arm: &str, until: tokio::time::Instant) {
        if let Some((owner, held)) = self.arms().get_mut(key)
            && owner == arm
        {
            *held = until;
        }
    }

    /// `arm` gives `key`'s wait up (another token's arm stays).
    fn disarm(&self, key: &str, arm: &str) {
        let mut arms = self.arms();
        if arms.get(key).is_some_and(|(owner, _)| owner == arm) {
            arms.remove(key);
        }
    }

    /// Whether `key` has a wait armed now (by any token).
    fn armed(&self, key: &str) -> bool {
        let now = tokio::time::Instant::now();
        self.arms().get(key).is_some_and(|(_, until)| *until > now)
    }
}

/// How long an agent's wait stays armed after one of its steps returns:
/// longer than the gap before its next step (a claim, a print, an ack).
pub const ARM_HOLD: Duration = Duration::from_secs(3);

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
    /// Shared so that the check pool can hold a weak reference: the engine
    /// stops when the last handle goes.
    jobs: Arc<mpsc::Sender<Job>>,
    waiters: Arc<Waiters>,
    stopped: watch::Receiver<bool>,
    open_waits: Arc<OpenWaits>,
    config: Arc<Config>,
    clock: Arc<dyn Clock>,
    /// How the daemon is going away, once `daemon_stop` was called:
    /// [`GOING_STOP`] or [`GOING_RESTART`] (0 while it runs).
    going: Arc<std::sync::atomic::AtomicU8>,
}

/// [`EngineHandle`]'s `going` for a stop.
const GOING_STOP: u8 = 1;
/// [`EngineHandle`]'s `going` for a restart.
const GOING_RESTART: u8 = 2;

impl EngineHandle {
    /// Run `command` on the engine thread, from no known process: see
    /// [`EngineHandle::call_from`].
    ///
    /// # Errors
    /// As [`EngineHandle::call_from`].
    pub async fn call(&self, command: Command) -> Result<Value, ApiError> {
        self.call_from(command, None).await
    }

    /// Run `command` on the engine thread; `host` is the process running
    /// the calling agent session, if it is one and said so
    /// ([`Sessions::touch`]).
    ///
    /// A mutating command replies as soon as its own transaction commits;
    /// the follow-up tick and dispatch run after the reply. The engine is
    /// single-threaded, so the next command (from any caller) still observes
    /// their effects.
    ///
    /// # Errors
    /// The command's [`ApiError`] (`session_not_hosted` for an agent session
    /// that may not hold what it asks for), or `engine_unavailable` when the
    /// engine has stopped.
    pub async fn call_from(&self, command: Command, host: Option<Host>) -> Result<Value, ApiError> {
        // The first stop says how; a later one (the shutdown's own) does
        // not change it.
        let marked = match &command {
            Command::DaemonStop(_, a) => self.mark_going(a.restart),
            _ => false,
        };
        let result = if matches!(command, Command::Acquire(..)) {
            self.settled_acquire(command, host).await
        } else {
            self.send_command(command, host, false).await
        };
        if marked && result.is_err() {
            // The stop failed, so the daemon goes on: waits must too.
            self.going.store(0, std::sync::atomic::Ordering::SeqCst);
        }
        result
    }

    async fn send_command(
        &self,
        command: Command,
        host: Option<Host>,
        defer: DeferOnCheck,
    ) -> Result<Value, ApiError> {
        let (reply, answer) = oneshot::channel();
        self.jobs
            .send(Job::Command(Box::new(command), host, defer, reply))
            .map_err(|_| ApiError::unavailable())?;
        answer.await.map_err(|_| ApiError::unavailable())?
    }

    /// Run an acquire, waiting here (never on the engine thread) while a
    /// health check of a target unit is on its way, for `health_timeout`
    /// plus [`CHECK_WAIT_SLACK`] at most: it then answers as if the check
    /// had run in place, granted or queued with what really blocks it.
    /// Nothing is queued meanwhile. Past the wait it queues, `blocked_by`
    /// checking.
    async fn settled_acquire(
        &self,
        command: Command,
        host: Option<Host>,
    ) -> Result<Value, ApiError> {
        let first = self
            .send_command(command.clone(), host.clone(), true)
            .await?;
        if first != still_checking() {
            return Ok(first);
        }
        let limit = Duration::from_secs(self.config.health_timeout) + CHECK_WAIT_SLACK;
        // The engine wakes every waiter when a check comes back.
        let answer = self
            .wait_until(&command.caller().holder, limit, || async {
                let answer = self
                    .send_command(command.clone(), host.clone(), true)
                    .await?;
                Ok((answer != still_checking()).then_some(answer))
            })
            .await
            .map_err(|e| self.announced(e))?;
        match answer {
            Some(answer) => Ok(answer),
            None => self.send_command(command, host, false).await,
        }
    }

    /// The daemon starts going away, for good or (`restart`) to come back:
    /// long-polls answer what it announces from now on, the parked ones at
    /// once, while the engine still runs. The first call says how; later
    /// ones do not change it.
    pub fn begin_shutdown(&self, restart: bool) {
        self.mark_going(restart);
        self.waiters.notify_all();
    }

    /// True once a deliberate stop began (not a restart, and not an engine
    /// that merely died).
    #[must_use]
    pub fn is_stopping(&self) -> bool {
        self.going.load(std::sync::atomic::Ordering::SeqCst) == GOING_STOP
    }

    /// Record how the daemon is going away, unless that is known already;
    /// true when this call decided it.
    fn mark_going(&self, restart: bool) -> bool {
        let going = if restart { GOING_RESTART } else { GOING_STOP };
        self.going
            .compare_exchange(
                0,
                going,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_ok()
    }

    /// Deliver `holder`'s pending notices, oldest first, waiting up to
    /// `timeout` (capped at [`MAX_WAIT`]) for one to arrive. Returns an
    /// empty list on timeout.
    ///
    /// In-process only: delivery here is final, so no answer that can be
    /// lost on its way to a client may come from it. The wire uses
    /// [`EngineHandle::wait_returned`].
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
            .await
            .map_err(|e| self.announced(e))?;
        Ok(got.unwrap_or_default())
    }

    /// `e`, or once the daemon is going away, what it announces to waiting
    /// clients instead: `daemon_restarting` (reconnect) or
    /// `daemon_stopping` (give up).
    fn announced(&self, e: ApiError) -> ApiError {
        self.going_away().unwrap_or(e)
    }

    /// What the daemon announces to waiting clients once it is going away.
    fn going_away(&self) -> Option<ApiError> {
        match self.going.load(std::sync::atomic::Ordering::SeqCst) {
            GOING_RESTART => Some(ApiError::new(
                ErrorKind::Unavailable,
                "daemon_restarting",
                "the clerk daemon is restarting; reconnect in a moment",
            )),
            GOING_STOP => Some(ApiError::new(
                ErrorKind::Unavailable,
                "daemon_stopping",
                "the clerk daemon was stopped",
            )),
            _ => None,
        }
    }

    /// Run `check` now and again whenever `holder` gets a notice, until it
    /// answers `Some`, for at most `timeout`; `None` on timeout (after one
    /// last check). Dropping the future (a client gone) ends the wait.
    ///
    /// # Errors
    /// The first error `check` returns; what the daemon announces once it
    /// is going away.
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
            if let Some(announced) = self.going_away() {
                return Err(announced);
            }
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

    /// Up to [`NOTICE_BATCH`] pending notices of `holder` (with `filter`,
    /// only those it matches), oldest first, waiting up to `timeout` (capped
    /// at [`MAX_WAIT`]) for one to arrive; empty on timeout. They are
    /// returned **without** being delivered: they stay pending, and come
    /// back on the next call, until [`EngineHandle::ack_notices`] names them.
    ///
    /// # Errors
    /// As [`EngineHandle::wait_notices`].
    pub async fn wait_returned(
        &self,
        holder: HolderRef,
        filter: Option<NoticeFilter>,
        timeout: Duration,
    ) -> Result<Vec<Notice>, ApiError> {
        let got = self
            .wait_until(&holder, timeout.min(MAX_WAIT), || async {
                let got = self.return_notices(holder.clone(), filter.clone()).await?;
                Ok((!got.is_empty()).then_some(got))
            })
            .await
            .map_err(|e| self.announced(e))?;
        Ok(got.unwrap_or_default())
    }

    /// Up to [`NOTICE_BATCH`] pending notices of `holder`, oldest first,
    /// returned **without** delivering them: they stay pending until
    /// [`EngineHandle::ack_notices`] names them (acknowledged delivery, as
    /// `clerk wait` uses it). With `filter`, only the notices it matches.
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

    /// Count an open wait of `holder` (a `clerk wait` long-poll) until the
    /// returned guard is dropped.
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
        let key = holder_key(holder);
        let open = self.open_waits.lock().get(&key).copied().unwrap_or(0);
        // An armed agent wait counts between its steps too.
        open.max(usize::from(self.open_waits.armed(&key)))
    }

    /// One step of an agent session's notice wait (`clerk wait`) from
    /// `host`: up to `timeout` (capped at [`MAX_WAIT`]) for pending notices,
    /// returned without delivering them (the CLI acknowledges them once
    /// printed, [`EngineHandle::ack_notices`]). `arm` is the wait's
    /// token: while another token holds the session's wait (blocking, or
    /// within [`ARM_HOLD`] of its last step) the step answers `Armed`.
    /// `first` marks a wait's first step.
    ///
    /// # Errors
    /// A storage failure, or `engine_unavailable`.
    pub async fn agent_wait(
        &self,
        session_id: String,
        host: Host,
        timeout: Duration,
        first: bool,
        arm: String,
    ) -> Result<AgentWait, ApiError> {
        let holder = HolderRef::AgentSession {
            session_id: session_id.clone(),
        };
        if !self
            .ask(|reply| Job::AdmitWait(session_id, host, reply))
            .await?
        {
            return Ok(AgentWait::Ended);
        }
        let key = holder_key(&holder);
        let timeout = timeout.min(MAX_WAIT);
        let until = tokio::time::Instant::now() + timeout + ARM_HOLD;
        if !self.open_waits.try_arm(&key, &arm, first, until) {
            return Ok(AgentWait::Armed);
        }
        let _open = self.open_wait(&holder);
        let got = self
            .wait_until(&holder, timeout, || async {
                let got = self.return_notices(holder.clone(), None).await?;
                Ok((!got.is_empty()).then_some(got))
            })
            .await;
        self.open_waits
            .extend_if_owner(&key, &arm, tokio::time::Instant::now() + ARM_HOLD);
        Ok(got
            .map_err(|e| self.announced(e))?
            .map_or(AgentWait::Timeout, AgentWait::Notices))
    }

    /// The wait with token `arm` of agent session `session_id` ended: it
    /// gives its arm up, so another wait may arm at once.
    pub fn disarm(&self, session_id: String, arm: &str) {
        let key = holder_key(&HolderRef::AgentSession { session_id });
        self.open_waits.disarm(&key, arm);
    }

    /// Agent session `session_id` is run by `host` (an agent call or hook
    /// said so): see [`Sessions::report`].
    ///
    /// # Errors
    /// A storage failure, or `engine_unavailable`.
    pub async fn report_host(&self, session_id: String, host: Host) -> Result<(), ApiError> {
        self.ask(|reply| Job::ReportHost(session_id, host, reply))
            .await
    }

    /// Whether agent session `session_id` has a running host.
    ///
    /// # Errors
    /// `engine_unavailable`.
    pub async fn session_alive(&self, session_id: String) -> Result<bool, ApiError> {
        self.ask(|reply| Job::SessionAlive(session_id, reply)).await
    }

    /// Run a hook RPC (`clerk hook …`); a `session_brief` gets the open
    /// waits of the session it resolved.
    ///
    /// # Errors
    /// The command's [`ApiError`], or `engine_unavailable`.
    pub async fn hook(&self, command: HookCommand) -> Result<Value, ApiError> {
        let answer = self
            .ask(|reply| Job::Hook(Box::new(command), reply))
            .await?;
        let mut value = answer.value;
        if let Some(holder) = answer.open_waits_of {
            value["open_waits"] = Value::from(self.open_waits(&holder));
        }
        Ok(value)
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

    /// Send the job `make` builds around a reply sender; its answer. An
    /// engine gone during a stop or a restart answers what the daemon
    /// announces, not `engine_unavailable` (which clients reconnect on).
    async fn ask<T>(&self, make: impl FnOnce(Reply<T>) -> Job) -> Result<T, ApiError> {
        let (reply, answer) = oneshot::channel();
        self.jobs
            .send(make(reply))
            .map_err(|_| self.announced(ApiError::unavailable()))?;
        answer
            .await
            .map_err(|_| self.announced(ApiError::unavailable()))?
    }
}

/// When the engine thread ends, however it ends: sets the stopped flag and
/// wakes every parked long-poll, which then answers what the daemon
/// announces (`engine_unavailable` if the engine merely died). The job
/// inbox is gone by then (`run` owns it).
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
    checks: CheckPool,
    sessions: Sessions,
    waiters: Arc<Waiters>,
    options: EngineOptions,
    /// The highest notice row whose holder has been woken.
    notices_seen: i64,
    /// The next scheduler deadline, unix ms.
    next_deadline: Option<i64>,
    /// What the retention sweep keeps.
    retention: Retention,
    /// When the retention sweep runs next, unix ms.
    next_retention: i64,
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
        let jobs = Arc::new(jobs);
        let answers = Arc::downgrade(&jobs);
        let checks = CheckPool::new(
            health.clone(),
            Duration::from_secs(config.health_timeout),
            move |unit, result| {
                // Gone means the engine is stopping: nobody needs the result.
                if let Some(jobs) = answers.upgrade() {
                    let _ = jobs.send(Job::CheckDone { unit, result });
                }
            },
        )?;
        let (stop_tx, stopped) = watch::channel(false);
        let waiters = Arc::new(Waiters::default());
        let handle_config = Arc::new(config.clone());
        let handle_clock = clock.clone();
        let retention = Retention {
            lease_days: config.retention_days,
            notice_days: config.notice_retention_days,
        };
        let sessions = Sessions::new(Duration::from_secs(config.reconnect_grace));
        let engine = Self {
            store,
            inventory: Inventory::new(clock.clone()),
            promotions: Promotions::new(clock.clone()),
            scheduler: Scheduler::new(clock.clone(), config, health),
            checks,
            sessions,
            clock,
            waiters: waiters.clone(),
            options,
            notices_seen: 0,
            next_deadline: None,
            retention,
            next_retention: i64::MIN,
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
            going: Arc::new(std::sync::atomic::AtomicU8::new(0)),
            config: handle_config,
            clock: handle_clock,
        })
    }

    fn run(mut self, inbox: mpsc::Receiver<Job>) {
        self.notices_seen = self.store.tx(notices::last_row).unwrap_or_else(|e| {
            tracing::error!(error = %e, "cannot read the notice log; waking from the start");
            0
        });
        self.restart_grace();
        self.boot_sessions();
        self.maintain();
        self.sweep_retention();
        self.start_checks();
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
            if self.clock.now_ms() >= self.next_retention {
                self.sweep_retention();
            }
            self.start_checks();
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

    /// Hand the check jobs scheduling queued to the pool. Every transaction
    /// that queued them has ended by now.
    fn start_checks(&self) {
        for job in self.scheduler.take_check_jobs() {
            self.checks.run(job);
        }
    }

    /// How long to wait for a job: until the next deadline, session grace
    /// end or probe.
    fn sleep_for(&self, next_probe: Instant) -> Duration {
        let probe = next_probe.saturating_duration_since(Instant::now());
        let now = self.clock.now_ms();
        [
            self.next_deadline,
            self.sessions.next_expiry(),
            Some(self.next_retention),
        ]
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

    /// Daemon start: leases that were running when the previous daemon went
    /// down get their restart grace.
    fn restart_grace(&mut self) {
        let result = self.guarded("restart grace", |engine| {
            let sched = &engine.scheduler;
            Ok(engine.store.tx(|tx| sched.restart_grace(tx))?)
        });
        match result {
            Ok(0) => {}
            Ok(graced) => tracing::info!(leases = graced, "restart grace applied"),
            Err(e) => tracing::error!(error = %e, "cannot apply the restart grace"),
        }
    }

    /// Delete what is past retention, then schedule the next sweep a day on
    /// (a failed sweep too: it is housekeeping, not worth a retry loop).
    fn sweep_retention(&mut self) {
        let now = self.clock.now_ms();
        let keep = self.retention;
        let result = self.guarded("retention sweep", |engine| {
            engine
                .store
                .tx(|tx| retention::sweep(tx, now, &keep))
                .map_err(ApiError::from)
        });
        match result {
            Ok(swept) if swept == Swept::default() => {}
            Ok(swept) => tracing::info!(?swept, "retention sweep"),
            Err(e) => tracing::error!(error = %e, "retention sweep failed"),
        }
        let gap = i64::try_from(RETENTION_INTERVAL.as_millis()).unwrap_or(i64::MAX);
        self.next_retention = now.saturating_add(gap);
    }

    /// Daemon start: every agent session with leases or requests gets the
    /// reconnect grace from now.
    fn boot_sessions(&mut self) {
        let result = self.guarded("session boot", |engine| {
            let now = engine.clock.now_ms();
            Ok(engine
                .sessions
                .transact(&mut engine.store, |tx, sessions| sessions.boot(tx, now))?)
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
            let sched = &engine.scheduler;
            Ok(engine
                .sessions
                .transact(&mut engine.store, |tx, sessions| {
                    sessions.expire(tx, sched, now)
                })?)
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
            Job::Command(command, host, defer, reply) => {
                self.handle_command(*command, host, defer, reply);
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
            Job::AdmitWait(session_id, host, reply) => {
                let result = self.guarded("admit_wait", |engine| {
                    let now = engine.clock.now_ms();
                    Ok(engine
                        .sessions
                        .transact(&mut engine.store, |tx, sessions| {
                            sessions
                                .touch(tx, &session_id, Some(host), now)
                                .map(|hosting| hosting == Hosting::Hosted)
                        })?)
                });
                let _ = reply.send(result);
            }
            Job::ReportHost(session_id, host, reply) => {
                let result = self.guarded("report_host", |engine| {
                    let now = engine.clock.now_ms();
                    engine
                        .sessions
                        .transact(&mut engine.store, |tx, sessions| {
                            sessions.touch(tx, &session_id, Some(host), now)
                        })?;
                    Ok(())
                });
                let _ = reply.send(result);
            }
            Job::SessionAlive(session_id, reply) => {
                let _ = reply.send(Ok(self.sessions.is_alive(&session_id)));
            }
            Job::Hook(command, reply) => {
                let result = self.guarded(command.method(), |engine| {
                    let now = engine.clock.now_ms();
                    let sched = &engine.scheduler;
                    engine.sessions.transact(&mut engine.store, |tx, sessions| {
                        command.execute(tx, sessions, sched, now)
                    })
                });
                let changed = result.as_ref().is_ok_and(|a| a.changed);
                let _ = reply.send(result);
                if changed {
                    self.maintain();
                }
            }
            Job::CheckDone { unit, result } => {
                let recorded = self.guarded("check_done", |engine| {
                    let sched = &engine.scheduler;
                    Ok(engine.store.tx(|tx| sched.check_done(tx, unit, result))?)
                });
                if let Err(e) = recorded {
                    tracing::error!(error = %e, "cannot record a health check");
                }
                self.maintain();
                // Acquires waiting for this result ask again.
                self.waiters.notify_all();
            }
        }
    }

    /// Run an RPC command for its caller and answer `reply`.
    fn handle_command(
        &mut self,
        command: Command,
        host: Option<Host>,
        defer: DeferOnCheck,
        reply: Reply<Value>,
    ) {
        // Reads see overdue deadlines processed.
        if self.deadline_due() {
            self.maintain();
        }
        let method = command.method();
        let mutating = !command.read_only();
        let stop = matches!(command, Command::DaemonStop(..));
        let result = self.guarded(method, |engine| engine.execute(command, host, defer));
        if stop && result.is_ok() {
            self.stopping = true;
        }
        // The caller may have gone away; nothing to do then.
        let _ = reply.send(result);
        if mutating {
            self.maintain();
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

    fn execute(
        &mut self,
        command: Command,
        host: Option<Host>,
        defer_on_check: DeferOnCheck,
    ) -> Result<Value, ApiError> {
        let hosting = self.touch_caller(&command, host)?;
        command.validate_hosting(hosting.as_ref(), &self.sessions)?;
        let services = Services {
            inventory: &self.inventory,
            promotions: &self.promotions,
            scheduler: &self.scheduler,
            now_ms: self.clock.now_ms(),
            defer_on_check,
        };
        self.store.tx(|tx| command.run(tx, &services))
    }

    /// Record that `command`'s caller, when an agent session, called from
    /// `host`, in its own transaction: a call counts even when the command
    /// then fails. `None` for any other caller.
    fn touch_caller(
        &mut self,
        command: &Command,
        host: Option<Host>,
    ) -> Result<Option<Hosting>, ApiError> {
        let HolderRef::AgentSession { session_id } = &command.caller().holder else {
            return Ok(None);
        };
        let now = self.clock.now_ms();
        let hosting = self.sessions.transact(&mut self.store, |tx, sessions| {
            sessions.touch(tx, session_id, host, now)
        })?;
        Ok(Some(hosting))
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

    /// End the leases and requests of CLI-process holders that are gone,
    /// and start the grace of agent sessions whose host process is gone
    /// and of agent holders with leases and no host at all.
    fn probe(&mut self) {
        let hosts = self.guarded("host probe", |engine| {
            let now = engine.clock.now_ms();
            Ok(engine
                .sessions
                .transact(&mut engine.store, |tx, sessions| {
                    let mut gone = sessions.probe(tx, now)?;
                    gone.extend(sessions.sweep_hostless(tx, now)?);
                    Ok::<_, StoreError>(gone)
                })?)
        });
        match hosts {
            Ok(gone) if gone.is_empty() => {}
            Ok(gone) => tracing::info!(?gone, "agent sessions lost their host; grace started"),
            Err(e) => tracing::error!(error = %e, "host probe failed"),
        }
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
