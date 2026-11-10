//! Leasing commands: `acquire`, `attach`, `cancel`, `release`, `extend`,
//! `run` and `wait`.
//!
//! `acquire` and `attach` show the live view (see `live`) only when stdin
//! is a terminal. Otherwise nothing reads stdin or touches the terminal:
//! `acquire` prints the grant, or the queued request and exits 6 (with
//! `--wait`, it waits for the offer and claims it); `attach` claims an
//! offered lease and prints it, and `cancel` withdraws a queued request.
//!
//! `wait` in an agent session is [`super::wait::agent`].
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
use tokio::signal::unix::{SignalKind, signal};
use tokio::time::Instant;
use ulid::Ulid;

use super::caller::{self, CallerKind};
use super::live::{self, Phase, RawMode, Screen};
use super::output::{EXIT_INTERRUPTED, EXIT_QUEUED, Exit, View};
use super::render::{self, SEP, Style};
use super::views::{brief_text, text};
use super::{Ctx, UsageError};

/// Seconds one notice long-poll waits.
pub const POLL_SECS: u64 = 30;
/// Pause after a failed notice poll, so a sick daemon is not hammered.
pub const POLL_RETRY: Duration = Duration::from_secs(1);
/// Longest default purpose of `clerk run`, in characters.
const PURPOSE_MAX: usize = 80;

pub use leclerk::duration::parse_duration;

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
    /// Grant the unit without running its health checks: you vouch for it,
    /// e.g. your own running job already has it open. A specific unit only
    /// (<resource>/<unit>); never takes it from a lease holder; audited.
    #[arg(long)]
    skip_checks: bool,
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
    pub once: bool,
    /// Agent sessions: print one JSON line per notice as it arrives and
    /// keep running until the session ends (for Claude Code's Monitor tool).
    #[arg(long)]
    pub follow: bool,
    /// Agent sessions: give up after this long with an empty batch:
    /// <n>s|m|h|d.
    #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
    pub timeout: Option<Duration>,
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
    caller::require_host(&caller)?;
    if args.wait && caller::is_agent(&caller) {
        return Err(UsageError(
            "acquire --wait blocks the agent: acquire without it, then arm `clerk wait --follow` \
             with the Monitor tool (or `clerk wait` in the background) to hear of the offer."
                .to_owned(),
        )
        .into());
    }
    interruptible_needs_session(&args.lease, &caller)?;
    let mut body = request_args(
        &target,
        &args.lease,
        "held from the command line".to_owned(),
        &caller,
    );
    if args.skip_checks {
        // Sent only when set, so a daemon that predates it still serves
        // every other acquire.
        body["skip_checks"] = json!(true);
    }
    let client = ctx.client().await?;
    if interactive {
        let _raw = RawMode::enable()?;
        let screen = Screen::new(ctx.out.mode());
        let outcome = client.call("acquire", &caller, body).await?;
        screen.say(&outcome, &outcome_text(&outcome, &args.target));
        let phase = if outcome["outcome"] == "queued" {
            screen.hint("Ctrl+C cancels.");
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
        return ctx.out.emit(&View::new(outcome, granted_text));
    }
    if !args.wait {
        let mut outcome = outcome;
        // The daemon's queued answer names only the request: add what was
        // asked for, so the result reads on its own.
        outcome["resource"] = target["resource"].clone();
        if let Some(unit) = target.get("unit") {
            outcome["unit"] = unit.clone();
        }
        outcome["next"] = Value::String(queued_next(&outcome));
        let shown = args.target.clone();
        ctx.out.emit(&View::new(outcome, move |v| {
            format!("{}\n{}", outcome_text(v, &shown), text(&v["next"]))
        }))?;
        return Err(Exit(EXIT_QUEUED).into());
    }
    let request = id(&outcome, "request_id")?;
    note(|| outcome_text(&outcome, &args.target));
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
    caller::require_host(&caller)?;
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

pub async fn release(ctx: &Ctx, lease: String, cleanup: bool) -> anyhow::Result<()> {
    let client = ctx.client().await?;
    let caller = lease_caller(ctx, &client, &lease).await?;
    if cleanup {
        let view = client
            .call("release", &caller, json!({ "lease_id": lease }))
            .await?;
        return ctx.out.emit(&View::new(view, |v| {
            format!(
                "Released: lease {} is in its cleanup window; `clerk confirm {} clean` ends it.",
                text(&v["lease_id"]),
                text(&v["lease_id"])
            )
        }));
    }
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
        let ends = v["ends_at"].as_i64().map_or_else(
            || "-".to_owned(),
            |at| render::when(at, SystemClock.now_ms()),
        );
        format!("Extended: lease {} now ends {ends}.", text(&v["lease_id"]))
    }))
}

pub async fn run(ctx: &Ctx, args: RunArgs) -> anyhow::Result<()> {
    let target = target_arg(&args.target)?;
    let caller = ctx.caller(CallerKind::Run)?;
    caller::require_host(&caller)?;
    interruptible_needs_session(&args.lease, &caller)?;
    let purpose = purpose_of(&args.command);
    let body = request_args(&target, &args.lease, purpose, &caller);
    let client = ctx.client().await?;
    let outcome = client.call("acquire", &caller, body).await?;
    let lease = match outcome["outcome"].as_str() {
        Some("granted") => outcome,
        Some("queued") => {
            note(|| outcome_text(&outcome, &args.target));
            await_offer(ctx, &client, &caller, &id(&outcome, "request_id")?).await?
        }
        _ => anyhow::bail!("the daemon answered the acquire with {outcome}"),
    };
    note(|| granted_text(&lease));
    match supervise(&client, &caller, &lease, &args.command).await? {
        0 => Ok(()),
        code => Err(Exit(code).into()),
    }
}

pub async fn wait(ctx: &Ctx, args: WaitArgs) -> anyhow::Result<()> {
    let caller = ctx.caller(CallerKind::Detached)?;
    if caller::is_agent(&caller) {
        return super::wait::agent(ctx, &caller, &args).await;
    }
    let client = ctx.waiting_client().await?;
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

/// Claim the offered `lease`. When the caller already holds it (an agent
/// session's MCP `lease_wait` claims its offers too), its current state is
/// the answer, as for MCP's claim.
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
    let args = json!({ "lease_id": lease });
    match client.call("claim", caller, args.clone()).await {
        Ok(view) => Ok(view),
        Err(e) => match client.call("get_lease", caller, args).await {
            Ok(view) if matches!(view["state"].as_str(), Some("held" | "paused")) => Ok(view),
            _ => Err(e),
        },
    }
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
/// holder never leaves a request queued behind it. A stopped daemon takes
/// no cancel: see [`request_after_stop`] for what becomes of the request.
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
            Err(e) if e.code == "daemon_stopping" => {
                note(|| request_after_stop(&caller.holder, request));
                return Err(e.into());
            }
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
                    note(|| notice_text(n));
                    gone = Some(dropped(n));
                    offer = None;
                }
                _ => note(|| notice_text(n)),
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

/// What becomes of `holder`'s `request` when the daemon is stopped: a
/// process's ends at the next start, once the process is gone (liveness); a
/// session's or a user's stays queued, and can still be offered.
pub fn request_after_stop(holder: &HolderRef, request: &str) -> String {
    if matches!(holder, HolderRef::CliProcess { .. }) {
        format!("Request {request} ends with the daemon, which was stopped.")
    } else {
        format!(
            "The clerk daemon was stopped; request {request} stays queued and may be offered \
             after it starts again. Once it runs, `clerk cancel {request}` withdraws it."
        )
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
        Ok(()) => note(|| format!("Request {request} is cancelled, as waiting for it failed.")),
        Err(e) => {
            note(|| format!("Request {request} may still be queued; cancelling it failed ({e})."));
        }
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
            note(|| {
                format!(
                    "Cannot run {}: {e}. The lease is given back.",
                    program.to_string_lossy()
                )
            });
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
    // Off once the daemon was stopped: polling on would only reconnect.
    let mut polling = true;
    let status = loop {
        let deadline = async move {
            match stop_at {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            status = child.wait() => break status?,
            batch = &mut notices, if polling => {
                match batch {
                    Ok(batch) => {
                        for n in &batch {
                            note(|| notice_text(n));
                            if let Some(at) = stop_time(n) {
                                stop_at = Some(stop_at.map_or(at, |s| s.min(at)));
                            }
                        }
                        notices = Box::pin(wait());
                    }
                    Err(e) if e.code == "daemon_stopping" => {
                        note(|| {
                            "The clerk daemon was stopped: the command keeps running, without \
                             notices about its lease."
                                .to_owned()
                        });
                        polling = false;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "cannot read notices; retrying");
                        tokio::time::sleep(POLL_RETRY).await;
                        notices = Box::pin(wait());
                    }
                }
            }
            () = deadline, if !stopped => {
                note(|| "The lease is over: stopping the command (SIGTERM).".to_owned());
                send(libc::SIGTERM);
                stopped = true;
            }
            // A Ctrl+C on the terminal already reached the command, which
            // shares our foreground process group; pass on only the rest.
            _ = interrupt.recv() => if !terminal_delivers_sigint() {
                note(|| "Passing SIGINT on to the command.".to_owned());
                send(libc::SIGINT);
            },
            _ = terminate.recv() => send(libc::SIGTERM),
        }
    };
    let code = exit_code(status);
    give_back(client, caller, &lease_id, code).await;
    Ok(code)
}

/// Give `lease_id` back after its command exited with `code`: cleanup
/// confirmed on exit 0, else ended at once with the unit dirty.
async fn give_back(client: &Client, caller: &Caller, lease_id: &str, code: u8) {
    let given_back = if code == 0 {
        finish(client, caller, lease_id).await.map(drop)
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
pub async fn lease_caller(ctx: &Ctx, client: &Client, lease: &str) -> anyhow::Result<Caller> {
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

/// A line on stderr, for progress that is not the command's output; styled
/// for stderr.
fn note(text: impl FnOnce() -> String) {
    let line = render::on_stderr(|| render::finish(&text(), Style::current()));
    eprintln!("{line}");
}

// ---------------------------------------------------------------------------
// Human rendering
// ---------------------------------------------------------------------------

/// An acquire answer on `target`: the grant with its brief, or the queue
/// position and what blocks the request.
fn outcome_text(v: &Value, target: &str) -> String {
    match v["outcome"].as_str() {
        Some("queued") => {
            let style = Style::current();
            let mut t = format!(
                "Queued: position {} on {}{SEP}{}",
                v["position"],
                style.id(target),
                text(&v["request_id"])
            );
            if let Some(line) = blocked_text(&v["blocked_by"], style) {
                let _ = write!(t, "\n  {line}");
            }
            if let Some(ms) = v["estimated_wait_ms"].as_i64() {
                let now = SystemClock.now_ms();
                let _ = write!(t, "\n  Expected turn: {}.", render::when(now + ms, now));
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

/// What keeps a queued request from a unit, in one sentence.
fn blocked_text(b: &Value, style: Style) -> Option<String> {
    let now = SystemClock.now_ms();
    let ends = |b: &Value| {
        b["ends_at"]
            .as_i64()
            .map(|at| format!(", ends {}", render::when(at, now)))
            .unwrap_or_default()
    };
    let purpose = |b: &Value| {
        b["purpose"]
            .as_str()
            .filter(|p| !p.is_empty())
            .map(|p| format!(" for {}", render::quoted(p)))
            .unwrap_or_default()
    };
    match b["kind"].as_str()? {
        "unhealthy" => {
            let units: Vec<&Value> = b["units"].as_array()?.iter().collect();
            let recheck = render::span(b["recheck_ms"].as_u64().unwrap_or(30_000) / 1000);
            let names: Vec<String> = units.iter().map(|u| text(&u["unit"])).collect();
            Some(match units.as_slice() {
                [one] => format!(
                    "{} is free but {}; it is re-checked every {recheck}.",
                    names.join(", "),
                    style.warn(&format!("unhealthy ({})", text(&one["message"])))
                ),
                _ => format!(
                    "{} are free but {}; they are re-checked every {recheck}.",
                    names.join(", "),
                    style.warn(&format!(
                        "unhealthy ({})",
                        units
                            .iter()
                            .map(|u| format!("{}: {}", text(&u["unit"]), text(&u["message"])))
                            .collect::<Vec<_>>()
                            .join("; ")
                    ))
                ),
            })
        }
        "checking" => {
            let names: Vec<String> = b["units"].as_array()?.iter().map(text).collect();
            Some(format!(
                "{} free and being health-checked; you are offered a unit as soon as a check \
                 passes.",
                match names.as_slice() {
                    [one] => format!("{one} is"),
                    _ => format!("{} are", names.join(", ")),
                }
            ))
        }
        "held" => Some(format!(
            "{} is held by {}{}{}.",
            text(&b["unit"]),
            render::holder(&b["holder"]),
            purpose(b),
            ends(b)
        )),
        "linked" => Some(match b["held_resource"].as_str() {
            Some(r) => format!(
                "{} is busy: the same device is held as {}/{} by {}{}{}.",
                text(&b["unit"]),
                style.id(r),
                text(&b["held_unit"]),
                render::holder(&b["holder"]),
                purpose(b),
                ends(b)
            ),
            None => format!(
                "{} is busy: the same device is held in another project{}.",
                text(&b["unit"]),
                ends(b)
            ),
        }),
        _ => None,
    }
}

/// A grant or held lease, with its brief.
pub fn granted_text(v: &Value) -> String {
    let style = Style::current();
    let mut t = format!(
        "Granted: {}/{}",
        style.id(&text(&v["resource"])),
        text(&v["unit"])
    );
    if let Some(at) = v["ends_at"].as_i64() {
        let _ = write!(
            t,
            " until {} ({})",
            render::clock(at),
            render::when(at, SystemClock.now_ms())
        );
    }
    let _ = write!(t, "{SEP}{}\n{}", text(&v["lease_id"]), brief_text(v));
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
