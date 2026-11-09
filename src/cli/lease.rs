//! Leasing commands: `acquire`, `attach`, `cancel`, `release`, `extend`,
//! `run` and `wait`.
//!
//! `acquire` and `attach` show the live view (see `live`) only when stdin
//! is a terminal. Otherwise nothing reads stdin or touches the terminal:
//! `acquire` prints the grant, or the queued request and exits 6 (with
//! `--wait`, it waits for the offer and claims it); `attach` claims an
//! offered lease and prints it, and `cancel` withdraws a queued request.
//!
//! Leases of the user's CLI (no agent session) belong to lease-scoped
//! `User` holders. `attach`, `release` and `extend` act as the holder the
//! status view names for the lease, and only when it is such a holder: a
//! lease of an agent session or of another running clerk is not the user's
//! to touch here (`not_found`, exit 3).

use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::IsTerminal;
use std::os::fd::AsRawFd;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::time::Duration;

use clap::{Args, ValueEnum};
use leclerk::client::{ApiError, Client};
use leclerk::clock::{Clock, SystemClock};
use leclerk::model::{Caller, HolderRef, LeaseId, RequestId, UnitSettings};
use leclerk::store::notices::{Notice, NoticeFilter, NoticeKind};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::signal::unix::{SignalKind, signal};
use tokio::time::Instant;
use ulid::Ulid;

use super::caller::{self, CallerKind};
use super::live::{self, Phase, RawMode, Screen};
use super::output::{EXIT_INTERRUPTED, EXIT_QUEUED, Exit, View};
use super::views::{brief_text, relative, text};
use super::{Ctx, UsageError};

/// Seconds one notice long-poll waits.
pub const POLL_SECS: u64 = 30;
/// The longest `--for`.
const MAX_FOR: Duration = Duration::from_hours(30 * 24);
/// Pause after a failed notice poll, so a sick daemon is not hammered.
pub const POLL_RETRY: Duration = Duration::from_secs(1);
/// Longest default purpose of `clerk run`, in characters.
const PURPOSE_MAX: usize = 80;

/// A `--for` value that is not `<n>s|m|h|d` with n ≥ 1, up to 30 days.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DurationError {
    #[error(
        "{0:?} is not a duration: use <n>s|m|h|d with a whole n of at least 1, e.g. 90s, 30m, 2h or 1d"
    )]
    Malformed(String),
    #[error("{0:?} is longer than 30 days: use <n>s|m|h|d up to 30d")]
    TooLong(String),
}

/// `<n>s|m|h|d` with a whole n ≥ 1, at most 30 days.
///
/// # Errors
/// [`DurationError`] for anything else.
pub fn parse_duration(s: &str) -> Result<Duration, DurationError> {
    let malformed = || DurationError::Malformed(s.to_owned());
    let too_long = || DurationError::TooLong(s.to_owned());
    let (at, unit) = s.char_indices().last().ok_or_else(malformed)?;
    let digits = &s[..at];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(malformed());
    }
    let per: u64 = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        _ => return Err(malformed()),
    };
    // All digits, so the only parse failure left is overflow.
    let n: u64 = digits.parse().map_err(|_| too_long())?;
    if n == 0 {
        return Err(malformed());
    }
    let secs = n.checked_mul(per).ok_or_else(too_long)?;
    let duration = Duration::from_secs(secs);
    if duration > MAX_FOR {
        return Err(too_long());
    }
    Ok(duration)
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum PriorityArg {
    Low,
    Normal,
    High,
}

impl PriorityArg {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Normal => "normal",
            Self::High => "high",
        }
    }
}

/// What every lease request states.
#[derive(Args)]
pub struct LeaseFlags {
    /// How long you expect to use it: <n>s|m|h|d, up to 30 days.
    #[arg(
        long = "for",
        value_name = "DURATION",
        value_parser = parse_duration,
        allow_hyphen_values = true
    )]
    duration: Duration,
    /// Open the lease to interruption by more urgent requests (agent
    /// sessions only).
    #[arg(long)]
    interruptible: bool,
    /// Queue priority.
    #[arg(long, value_enum, default_value = "normal")]
    priority: PriorityArg,
    /// Why you need it; shown to others who wait.
    #[arg(long)]
    purpose: Option<String>,
}

/// `clerk acquire`.
#[derive(Args)]
pub struct AcquireArgs {
    /// `<resource>` or `<resource>/<unit>`.
    target: String,
    #[command(flatten)]
    lease: LeaseFlags,
    /// Without a terminal: wait until granted, then print the lease.
    #[arg(long, conflicts_with = "no_wait")]
    wait: bool,
    /// Without a terminal: print the queued request and exit 6 (the
    /// default).
    #[arg(long)]
    no_wait: bool,
}

/// `clerk extend`.
#[derive(Args)]
pub struct ExtendArgs {
    /// The lease id.
    lease: String,
    /// The new end, from now: <n>s|m|h|d, up to 30 days.
    #[arg(
        long = "for",
        value_name = "DURATION",
        value_parser = parse_duration,
        allow_hyphen_values = true
    )]
    duration: Duration,
}

/// `clerk run`.
#[derive(Args)]
pub struct RunArgs {
    /// `<resource>` or `<resource>/<unit>`.
    target: String,
    #[command(flatten)]
    lease: LeaseFlags,
    /// The command to run, after `--`.
    #[arg(last = true, required = true, value_name = "CMD")]
    command: Vec<OsString>,
}

/// `clerk wait`.
#[derive(Args)]
pub struct WaitArgs {
    /// Return after the first batch of notices (or after 30 s with none).
    #[arg(long)]
    once: bool,
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

pub async fn acquire(ctx: &Ctx, args: AcquireArgs) -> anyhow::Result<()> {
    let target = target_arg(&args.target)?;
    let interactive = std::io::stdin().is_terminal();
    // `--wait` holds the request as this process while it waits, so its
    // offer goes to no one else's `clerk wait` and a killed script leaves
    // nothing queued; every lease it ends up holding (granted at once or
    // claimed later) moves to a user holder before it is printed.
    let kind = if interactive || args.wait {
        CallerKind::Interactive
    } else {
        CallerKind::Detached
    };
    let caller = ctx.caller(kind)?;
    interruptible_needs_session(&args.lease, &caller)?;
    let body = request_args(
        &target,
        &args.lease,
        "held from the command line".to_owned(),
        &caller,
    );
    let client = ctx.client().await?;
    if interactive {
        let _raw = RawMode::enable()?;
        let screen = Screen::new(ctx.out.mode());
        let outcome = client.call("acquire", &caller, body).await?;
        screen.say(&outcome, &outcome_text(&outcome));
        let phase = if outcome["outcome"] == "queued" {
            screen.hint("Ctrl+C cancels the request.");
            Phase::Queued {
                request: id(&outcome, "request_id")?,
            }
        } else {
            screen.hint_held();
            Phase::Held {
                lease: id(&outcome, "lease_id")?,
            }
        };
        return live::run(&client, &caller, &screen, phase).await;
    }
    let outcome = client.call("acquire", &caller, body).await?;
    if outcome["outcome"] != "queued" {
        // Granted at once to `--wait`'s process holder: hand the lease to
        // a user holder before this process exits, or liveness ends it.
        detach_from_process(&client, &caller, outcome.clone()).await?;
        return ctx.out.emit(&View::new(outcome, outcome_text));
    }
    if !args.wait {
        let mut outcome = outcome;
        outcome["next"] = Value::String(queued_next(&outcome));
        ctx.out.emit(&View::new(outcome, |v| {
            format!("{}\n{}", outcome_text(v), text(&v["next"]))
        }))?;
        return Err(Exit(EXIT_QUEUED).into());
    }
    let request = id(&outcome, "request_id")?;
    note(&outcome_text(&outcome));
    let lease = await_offer(ctx, &client, &caller, &request).await?;
    let lease = detach_from_process(&client, &caller, lease).await?;
    ctx.out.emit(&View::new(lease, granted_text))
}

/// What to do about a request left queued: how to claim its offer, and how
/// to withdraw it.
fn queued_next(outcome: &Value) -> String {
    let request = text(&outcome["request_id"]);
    format!(
        "When it is offered (`clerk wait` shows the offer with its lease id), `clerk attach \
         <lease>` claims it; `clerk cancel {request}` withdraws the request."
    )
}

/// Hand a lease this process holds to a fresh user holder, so it outlives
/// the process; other holders keep it. Returns the lease as it is now.
async fn detach_from_process(
    client: &Client,
    caller: &Caller,
    lease: Value,
) -> anyhow::Result<Value> {
    if !matches!(caller.holder, HolderRef::CliProcess { .. }) {
        return Ok(lease);
    }
    let holder = HolderRef::User {
        tag: Ulid::new().to_string(),
    };
    let lease_id = id(&lease, "lease_id")?;
    Ok(client
        .call(
            "rehome_holder",
            caller,
            json!({"lease_id": lease_id, "new_holder": holder}),
        )
        .await?)
}

pub async fn attach(ctx: &Ctx, lease: String) -> anyhow::Result<()> {
    let client = ctx.client().await?;
    let caller = lease_caller(ctx, &client, &lease).await?;
    let view = client
        .call("get_lease", &caller, json!({ "lease_id": lease }))
        .await?;
    if view["state"] == "ended" {
        return Err(no_live_lease(&lease).into());
    }
    if !std::io::stdin().is_terminal() {
        // An offer waiting to be claimed is claimed now.
        let view = if view["state"] == "offered" {
            claim(&client, &caller, &lease).await?
        } else {
            view
        };
        return ctx.out.emit(&View::new(view, granted_text));
    }
    let _raw = RawMode::enable()?;
    let screen = Screen::new(ctx.out.mode());
    // An offer waiting to be claimed: claim it, and Ctrl+C withdraws it.
    let offered_for = view["request_id"]
        .as_str()
        .filter(|_| view["state"] == "offered");
    let phase = if let Some(request) = offered_for {
        screen.say(
            &view,
            &format!("Offered: lease {lease} is yours to take; claiming it…"),
        );
        Phase::Offered {
            request: request.to_owned(),
            lease,
        }
    } else {
        screen.say(&view, &granted_text(&view));
        screen.hint_held();
        Phase::Held { lease }
    };
    live::run(&client, &caller, &screen, phase).await
}

pub async fn cancel_request(ctx: &Ctx, request: String) -> anyhow::Result<()> {
    let client = ctx.client().await?;
    let caller = request_caller(ctx, &client, &request).await?;
    match cancel(&client, &caller, &request).await {
        Ok(()) => {}
        // The daemon names only the caller's own requests.
        Err(e) if e.code == "not_found" || e.code == "not_holder" => {
            return Err(no_open_request(&request).into());
        }
        Err(e) => return Err(e.into()),
    }
    ctx.out
        .emit(&View::new(json!({ "cancelled": request }), |v| {
            format!(
                "Cancelled: request {} is withdrawn, with any offer it had; nothing is held.",
                text(&v["cancelled"])
            )
        }))
}

pub async fn release(ctx: &Ctx, lease: String) -> anyhow::Result<()> {
    let client = ctx.client().await?;
    let caller = lease_caller(ctx, &client, &lease).await?;
    let view = finish(&client, &caller, &lease).await?;
    ctx.out.emit(&View::new(view, |v| {
        format!(
            "Released: lease {} has ended and its cleanup is confirmed.",
            text(&v["lease_id"])
        )
    }))
}

pub async fn extend(ctx: &Ctx, args: ExtendArgs) -> anyhow::Result<()> {
    let client = ctx.client().await?;
    let caller = lease_caller(ctx, &client, &args.lease).await?;
    let ends_at = SystemClock.now_ms().saturating_add(millis(args.duration));
    let view = client
        .call(
            "extend",
            &caller,
            json!({ "lease_id": args.lease, "ends_at": ends_at }),
        )
        .await?;
    ctx.out.emit(&View::new(view, |v| {
        let ends = v["ends_at"]
            .as_i64()
            .map_or_else(|| "-".to_owned(), |at| relative(at, SystemClock.now_ms()));
        format!("Extended: lease {} now ends {ends}.", text(&v["lease_id"]))
    }))
}

pub async fn run(ctx: &Ctx, args: RunArgs) -> anyhow::Result<()> {
    let target = target_arg(&args.target)?;
    let caller = ctx.caller(CallerKind::Run)?;
    interruptible_needs_session(&args.lease, &caller)?;
    let purpose = purpose_of(&args.command);
    let body = request_args(&target, &args.lease, purpose, &caller);
    let client = ctx.client().await?;
    let outcome = client.call("acquire", &caller, body).await?;
    let lease = match outcome["outcome"].as_str() {
        Some("granted") => outcome,
        Some("queued") => {
            note(&outcome_text(&outcome));
            await_offer(ctx, &client, &caller, &id(&outcome, "request_id")?).await?
        }
        _ => anyhow::bail!("the daemon answered the acquire with {outcome}"),
    };
    note(&granted_text(&lease));
    match supervise(&client, &caller, &lease, &args.command).await? {
        0 => Ok(()),
        code => Err(Exit(code).into()),
    }
}

pub async fn wait(ctx: &Ctx, args: WaitArgs) -> anyhow::Result<()> {
    let caller = ctx.caller(CallerKind::Detached)?;
    let client = ctx.client().await?;
    loop {
        let batch = next_batch(&client, &caller).await?;
        if args.once {
            let value = serde_json::to_value(&batch)?;
            return ctx.out.emit(&View::new(value, move |_| {
                if batch.is_empty() {
                    "No notices arrived.".to_owned()
                } else {
                    batch.iter().map(notice_text).collect::<Vec<_>>().join("\n")
                }
            }));
        }
        for n in batch {
            let value = serde_json::to_value(&n)?;
            ctx.out.emit(&View::new(value, move |_| notice_text(&n)))?;
        }
    }
}

// ---------------------------------------------------------------------------
// Shared steps
// ---------------------------------------------------------------------------

/// Claim the offered `lease`.
///
/// Debug builds only: `CLERK_TEST_CLAIM_DELAY_MS` delays the claim, so
/// tests can press Ctrl+C while an offer is pending. Release builds never
/// read it.
pub async fn claim(client: &Client, caller: &Caller, lease: &str) -> Result<Value, ApiError> {
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("CLERK_TEST_CLAIM_DELAY_MS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
    client
        .call("claim", caller, json!({ "lease_id": lease }))
        .await
}

/// Withdraw `request`; if it was already offered a unit, the offer.
pub async fn cancel(client: &Client, caller: &Caller, request: &str) -> Result<(), ApiError> {
    client
        .call("cancel", caller, json!({ "request_id": request }))
        .await
        .map(drop)
}

/// End `lease` with its cleanup confirmed: release it when held (an offered
/// lease is declined, a paused one forfeited), then confirm the cleanup.
/// Returns the ended lease.
///
/// # Errors
/// `not_found` for an ended lease; the daemon's errors.
pub async fn finish(client: &Client, caller: &Caller, lease: &str) -> Result<Value, ApiError> {
    let args = json!({ "lease_id": lease });
    let mut view = client.call("get_lease", caller, args.clone()).await?;
    match view["state"].as_str() {
        Some("ended") => return Err(no_live_lease(lease)),
        Some("held" | "paused" | "offered") => {
            view = client.call("release", caller, args.clone()).await?;
        }
        _ => {}
    }
    if view["state"] != "cleaning" {
        return Ok(view);
    }
    client
        .call(
            "confirm",
            caller,
            json!({ "lease_id": lease, "how": "clean" }),
        )
        .await?;
    client.call("get_lease", caller, args).await
}

/// The error for a notice that dropped the caller's request (exit 4).
pub fn dropped(n: &Notice) -> ApiError {
    ApiError {
        code: "queue_dropped".to_owned(),
        message: n.action.clone(),
        details: None,
    }
}

/// Wait for `request`'s offer and claim it; returns the held lease. Only
/// notices about the request are taken (the holder's others stay pending);
/// those that are not the offer are shown on stderr. SIGINT, SIGTERM or
/// SIGHUP cancels the request, and so does a failed wait, so a detached
/// holder never leaves a request queued behind it.
async fn await_offer(
    ctx: &Ctx,
    client: &Client,
    caller: &Caller,
    request: &str,
) -> anyhow::Result<Value> {
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let only_ours = filter(&[], &[request]);
    loop {
        let waited = tokio::select! {
            batch = client.wait_notices_matching(&caller.holder, Some(&only_ours), POLL_SECS) => batch,
            _ = interrupt.recv() => {
                cancel(client, caller, request).await?;
                return Err(Exit(EXIT_INTERRUPTED).into());
            }
            _ = terminate.recv() => {
                cancel(client, caller, request).await?;
                return Err(Exit(128 + 15).into());
            }
            _ = hangup.recv() => {
                cancel(client, caller, request).await?;
                return Err(Exit(128 + 1).into());
            }
        };
        let batch = match waited {
            Ok(batch) => batch,
            Err(e) => {
                cancel_after_failure(ctx, caller, request).await;
                return Err(e.into());
            }
        };
        // The whole batch is shown; the last word on the request decides.
        let mut offer = None;
        let mut gone = None;
        for n in &batch {
            match (n.kind, &n.lease_id) {
                (NoticeKind::Offered, Some(lease)) => {
                    offer = Some(lease.as_str().to_owned());
                    gone = None;
                }
                (NoticeKind::QueueDropped, _) => {
                    note(&notice_text(n));
                    gone = Some(dropped(n));
                    offer = None;
                }
                _ => note(&notice_text(n)),
            }
        }
        if let Some(e) = gone {
            return Err(e.into());
        }
        if let Some(lease) = offer {
            return Ok(claim(client, caller, &lease).await?);
        }
    }
}

/// Cancel `request` after its wait failed, through a fresh connection (the
/// daemon is started again if it went away). Best effort: the original
/// failure is what the command reports.
async fn cancel_after_failure(ctx: &Ctx, caller: &Caller, request: &str) {
    let cancelled = match ctx.client().await {
        Ok(client) => cancel(&client, caller, request)
            .await
            .map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };
    match cancelled {
        Ok(()) => note(&format!(
            "Request {request} is cancelled, as waiting for it failed."
        )),
        Err(e) => note(&format!(
            "Request {request} may still be queued; cancelling it failed ({e})."
        )),
    }
}

/// A notice filter naming `leases` and `requests` (ids that do not parse
/// are left out; they come from the daemon, so there are none).
pub fn filter(leases: &[&str], requests: &[&str]) -> NoticeFilter {
    NoticeFilter {
        leases: leases
            .iter()
            .filter_map(|l| LeaseId::parse(l).ok())
            .collect(),
        requests: requests
            .iter()
            .filter_map(|r| RequestId::parse(r).ok())
            .collect(),
    }
}

/// Run `command` while holding `lease`, then give the lease back: cleanup
/// confirmed on exit 0, else ended at once with the unit dirty. SIGTERM
/// goes on to the command, and SIGINT too unless the terminal already
/// delivered it (Ctrl+C); an override, an expiry or an
/// administrator's release sends it SIGTERM at the lease's deadline.
/// Returns the command's exit code (128 + signal when killed).
async fn supervise(
    client: &Client,
    caller: &Caller,
    lease: &Value,
    command: &[OsString],
) -> anyhow::Result<u8> {
    let lease_id = id(lease, "lease_id")?;
    let settings: UnitSettings = serde_json::from_value(lease["settings"].clone())?;
    let (program, rest) = command
        .split_first()
        .ok_or_else(|| UsageError("clerk run needs a command after --".to_owned()))?;
    let mut child = match tokio::process::Command::new(program)
        .args(rest)
        .env("CLERK_LEASE_ID", &lease_id)
        .env("CLERK_RESOURCE", text(&lease["resource"]))
        .env("CLERK_UNIT", text(&lease["unit"]))
        .envs(settings.env_vars())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            note(&format!(
                "Cannot run {}: {e}. The lease is given back.",
                program.to_string_lossy()
            ));
            finish(client, caller, &lease_id).await?;
            return Ok(127);
        }
    };
    let pid = child.id().and_then(|p| libc::pid_t::try_from(p).ok());
    let send = |sig: libc::c_int| {
        if let Some(pid) = pid {
            // SAFETY: kill(2) on our own child, which has not been reaped:
            // `child.wait()` has not returned while the loop runs.
            unsafe {
                libc::kill(pid, sig);
            }
        }
    };
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let only_ours = filter(&[&lease_id], &[]);
    let wait = || client.wait_notices_matching(&caller.holder, Some(&only_ours), POLL_SECS);
    let mut notices = Box::pin(wait());
    let mut stop_at: Option<Instant> = None;
    let mut stopped = false;
    let status = loop {
        let deadline = async move {
            match stop_at {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            status = child.wait() => break status?,
            batch = &mut notices => {
                match batch {
                    Ok(batch) => {
                        for n in &batch {
                            note(&notice_text(n));
                            if let Some(at) = stop_time(n) {
                                stop_at = Some(stop_at.map_or(at, |s| s.min(at)));
                            }
                        }
                        notices = Box::pin(wait());
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "cannot read notices; retrying");
                        tokio::time::sleep(POLL_RETRY).await;
                        notices = Box::pin(wait());
                    }
                }
            }
            () = deadline, if !stopped => {
                note("The lease is over: stopping the command (SIGTERM).");
                send(libc::SIGTERM);
                stopped = true;
            }
            // A Ctrl+C on the terminal already reached the command, which
            // shares our foreground process group; pass on only the rest.
            _ = interrupt.recv() => if !terminal_delivers_sigint() {
                note("Passing SIGINT on to the command.");
                send(libc::SIGINT);
            },
            _ = terminate.recv() => send(libc::SIGTERM),
        }
    };
    let code = exit_code(status);
    let given_back = if code == 0 {
        finish(client, caller, &lease_id).await.map(drop)
    } else {
        client
            .call(
                "release",
                caller,
                json!({ "lease_id": lease_id, "failed": true }),
            )
            .await
            .map(drop)
    };
    if let Err(e) = given_back {
        // An override or an administrator may have ended it already.
        tracing::debug!(error = %e, "lease not given back");
    }
    Ok(code)
}

/// True when our controlling terminal's foreground process group is ours
/// (whatever stdin is): a SIGINT then came from Ctrl+C, which the terminal
/// sent to the whole group, the command included. Without a controlling
/// terminal, false: the SIGINT came from elsewhere and is passed on.
fn terminal_delivers_sigint() -> bool {
    let Ok(tty) = std::fs::File::open("/dev/tty") else {
        return false;
    };
    // SAFETY: tcgetpgrp(2) on a descriptor `tty` keeps open for the call,
    // and getpgrp(2), only read process state.
    unsafe { libc::tcgetpgrp(tty.as_raw_fd()) == libc::getpgrp() }
}

/// When a notice about the running command's lease means the command must
/// stop: an override, an expiry (cleanup window) or the lease's end.
fn stop_time(n: &Notice) -> Option<Instant> {
    match n.kind {
        NoticeKind::Overridden | NoticeKind::CleanupWindowStarted | NoticeKind::LeaseEnded => {
            let left = n
                .deadline
                .map_or(0, |d| d.saturating_sub(SystemClock.now_ms()).max(0));
            let left = Duration::from_millis(u64::try_from(left).unwrap_or(0));
            Some(Instant::now() + left)
        }
        _ => None,
    }
}

fn exit_code(status: ExitStatus) -> u8 {
    let code = status
        .code()
        .or_else(|| status.signal().map(|s| 128 + s))
        .unwrap_or(1);
    u8::try_from(code).unwrap_or(1)
}

/// The next notices for the caller: an agent session's own, or, for the
/// user, those of every live user-holder lease or request (each lease made
/// by the user's CLI has its own holder). Waits up to [`POLL_SECS`].
async fn next_batch(client: &Client, caller: &Caller) -> anyhow::Result<Vec<Notice>> {
    if caller::is_agent(caller) {
        return Ok(client.wait_notices(&caller.holder, POLL_SECS).await?);
    }
    let deadline = Instant::now() + Duration::from_secs(POLL_SECS);
    loop {
        let holders = user_holders(client, caller).await?;
        if let [only] = holders.as_slice() {
            let left = deadline.saturating_duration_since(Instant::now());
            return Ok(client.wait_notices(only, left.as_secs().max(1)).await?);
        }
        let mut batch = Vec::new();
        for holder in &holders {
            batch.extend(client.wait_notices(holder, 0).await?);
        }
        if !batch.is_empty() || Instant::now() >= deadline {
            return Ok(batch);
        }
        tokio::time::sleep(POLL_RETRY).await;
    }
}

/// Every `User` holder with a live lease or a queued request.
async fn user_holders(client: &Client, caller: &Caller) -> anyhow::Result<Vec<HolderRef>> {
    let status = client
        .call("status", caller, json!({ "view": "all" }))
        .await?;
    let mut holders = Vec::new();
    for r in status["resources"].as_array().into_iter().flatten() {
        let units = r["units"].as_array().into_iter().flatten();
        let held = units.map(|u| &u["occupancy"]["holder"]);
        let queued = r["queue"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|q| &q["holder"]);
        for h in held.chain(queued) {
            if let Ok(holder @ HolderRef::User { .. }) = serde_json::from_value(h.clone())
                && !holders.contains(&holder)
            {
                holders.push(holder);
            }
        }
    }
    Ok(holders)
}

/// Who acts on `lease`: an agent session acts as itself (the daemon checks
/// it holds the lease); the user acts as the lease's `User` holder, which
/// the daemon names for live user leases only.
///
/// # Errors
/// `not_found` when the user has no live lease of that id.
async fn lease_caller(ctx: &Ctx, client: &Client, lease: &str) -> anyhow::Result<Caller> {
    let caller = ctx.caller(CallerKind::Detached)?;
    if caller::is_agent(&caller) {
        return Ok(caller);
    }
    let answer = match client
        .call("lease_holder", &caller, json!({ "lease_id": lease }))
        .await
    {
        Ok(answer) => answer,
        Err(e) if e.code == "not_found" => return Err(no_live_lease(lease).into()),
        Err(e) => return Err(e.into()),
    };
    let holder: HolderRef = serde_json::from_value(answer["holder"].clone())?;
    Ok(Caller { holder, ..caller })
}

/// Who acts on `request`: an agent session acts as itself; the user acts
/// as the request's `User` holder, which the daemon names while the request
/// is queued or its offer pending.
///
/// # Errors
/// `not_found` when the user has no such open request.
async fn request_caller(ctx: &Ctx, client: &Client, request: &str) -> anyhow::Result<Caller> {
    let caller = ctx.caller(CallerKind::Detached)?;
    if caller::is_agent(&caller) {
        return Ok(caller);
    }
    let answer = match client
        .call("request_holder", &caller, json!({ "request_id": request }))
        .await
    {
        Ok(answer) => answer,
        Err(e) if e.code == "not_found" => return Err(no_open_request(request).into()),
        Err(e) => return Err(e.into()),
    };
    let holder: HolderRef = serde_json::from_value(answer["holder"].clone())?;
    Ok(Caller { holder, ..caller })
}

fn no_open_request(request: &str) -> ApiError {
    ApiError {
        code: "not_found".to_owned(),
        message: format!(
            "No open request {request} of yours: it is unknown, was granted, claimed or \
             dropped, or belongs to an agent session or another running clerk. `clerk leases` \
             lists the queued ones."
        ),
        details: None,
    }
}

fn no_live_lease(lease: &str) -> ApiError {
    ApiError {
        code: "not_found".to_owned(),
        message: format!(
            "No live lease {lease} of yours: it is unknown, has ended, or belongs to an agent \
             session or another running clerk. `clerk leases` lists the live ones."
        ),
        details: None,
    }
}

/// Only an agent session's leases can be interrupted: a lease held from the
/// command line is never asked to yield.
///
/// # Errors
/// [`UsageError`] for `--interruptible` without an agent session.
fn interruptible_needs_session(flags: &LeaseFlags, caller: &Caller) -> Result<(), UsageError> {
    if flags.interruptible && !caller::is_agent(caller) {
        return Err(UsageError(format!(
            "--interruptible needs an agent session ({} is not set): leases held from the \
             command line are never interrupted. Drop --interruptible.",
            caller::SESSION_VAR
        )));
    }
    Ok(())
}

/// `{"resource": r}` or `{"resource": r, "unit": u}` from `r` or `r/u`.
fn target_arg(target: &str) -> Result<Value, UsageError> {
    match target.split_once('/') {
        None if !target.is_empty() => Ok(json!({ "resource": target })),
        Some((r, u)) if !r.is_empty() && !u.is_empty() && !u.contains('/') => {
            Ok(json!({ "resource": r, "unit": u }))
        }
        _ => Err(UsageError(format!(
            "{target:?} is not a target: use <resource> or <resource>/<unit>"
        ))),
    }
}

/// The `acquire` arguments; the override reason is the caller's.
fn request_args(target: &Value, flags: &LeaseFlags, purpose: String, caller: &Caller) -> Value {
    json!({
        "target": target,
        "duration_ms": millis(flags.duration),
        "interruptible": flags.interruptible,
        "priority": flags.priority.as_str(),
        "purpose": flags.purpose.clone().unwrap_or(purpose),
        "override_reason": caller.override_reason,
    })
}

/// `clerk run <cmd…>`, cut to [`PURPOSE_MAX`] characters.
fn purpose_of(command: &[OsString]) -> String {
    let words: Vec<String> = command
        .iter()
        .map(|w| w.to_string_lossy().into_owned())
        .collect();
    super::output::truncate(&format!("clerk run {}", words.join(" ")), PURPOSE_MAX)
}

fn millis(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

/// The string field `key` of a daemon answer.
fn id(v: &Value, key: &str) -> anyhow::Result<String> {
    v[key]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("the daemon's answer has no {key}: {v}"))
}

/// A line on stderr, for progress that is not the command's output.
fn note(text: &str) {
    eprintln!("{text}");
}

// ---------------------------------------------------------------------------
// Human rendering
// ---------------------------------------------------------------------------

/// An acquire answer: the grant with its brief, or the queue position.
fn outcome_text(v: &Value) -> String {
    match v["outcome"].as_str() {
        Some("queued") => {
            let mut t = format!(
                "Queued: you are at position {} for this unit (request {}).",
                v["position"],
                text(&v["request_id"])
            );
            if let Some(ms) = v["estimated_wait_ms"].as_i64() {
                let now = SystemClock.now_ms();
                let _ = write!(t, " Expected turn: {}.", relative(now + ms, now));
            }
            t
        }
        Some("reclaiming") => format!(
            "Reclaiming: your lent unit comes back to lease {}.",
            text(&v["lease_id"])
        ),
        _ => granted_text(v),
    }
}

/// A grant or held lease, with its brief.
pub fn granted_text(v: &Value) -> String {
    let mut t = format!(
        "Granted: lease {} on {}/{}",
        text(&v["lease_id"]),
        text(&v["resource"]),
        text(&v["unit"])
    );
    if let Some(at) = v["ends_at"].as_i64() {
        let _ = write!(t, ", ends {}", relative(at, SystemClock.now_ms()));
    }
    let _ = write!(t, ".\n{}", brief_text(v));
    t
}

/// One notice: its kind, then what to do.
pub fn notice_text(n: &Notice) -> String {
    let kind = serde_json::to_value(n.kind)
        .ok()
        .and_then(|k| k.as_str().map(str::to_owned))
        .unwrap_or_default();
    format!("[{kind}] {}", n.action)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_take_every_unit() {
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_mins(30));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_hours(2));
        assert_eq!(parse_duration("30d").unwrap(), MAX_FOR);
    }

    #[test]
    fn durations_refuse_zero_signs_bare_numbers_and_junk() {
        for bad in [
            "0", "0s", "-5m", "+5m", "5", "abc", "", "5w", "1.5h", "m", "5 m",
        ] {
            assert!(
                matches!(parse_duration(bad), Err(DurationError::Malformed(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn durations_refuse_more_than_30_days() {
        for bad in ["31d", "100000d", "99999999999999999999999s"] {
            assert!(
                matches!(parse_duration(bad), Err(DurationError::TooLong(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn duration_errors_name_the_format() {
        for bad in ["abc", "100000d"] {
            let message = parse_duration(bad).unwrap_err().to_string();
            assert!(message.contains("<n>s|m|h|d"), "{message}");
        }
    }

    #[test]
    fn targets_are_a_resource_or_a_unit() {
        assert_eq!(target_arg("b").unwrap(), json!({"resource": "b"}));
        assert_eq!(
            target_arg("b/u").unwrap(),
            json!({"resource": "b", "unit": "u"})
        );
        for bad in ["", "/u", "b/", "b/u/v"] {
            assert!(target_arg(bad).is_err(), "{bad}");
        }
    }
}
