//! `clerk wait` in an agent session: notices for Claude Code's Monitor
//! tool (`--follow`) or a background command (one batch).
//!
//! Each step long-polls the daemon for [`STEP`] (`GET /v1/agent/wait`).
//! Notices come back undelivered; an `offered` one is claimed first
//! (waiting proves the agent is present), and every notice is
//! acknowledged only after its output is flushed, so a wait killed in
//! between leaves them for the next. The wait ends when the session is no
//! longer running in its Claude Code process: the process is gone (checked
//! every step) or runs another session now (`/clear`; the daemon says so).
//! Only one wait per session: a second exits at once. SIGTERM and SIGINT
//! (a background time limit) end a one-batch wait with an empty batch
//! saying to start it again.

use std::io::Write as _;
use std::time::Duration;

use anyhow::Context;
use leclerk::client::{ApiError, Client, DaemonError};
use leclerk::model::{Caller, HolderRef};
use leclerk::scheduler::liveness::process_alive;
use serde_json::{Value, json};
use tokio::signal::unix::{SignalKind, signal};
use tokio::time::Instant;

use super::Ctx;
use super::caller;
use super::lease::WaitArgs;
use super::output::{EXIT_DAEMON, Exit};

/// How long a new wait waits out another wait's arm before giving way:
/// longer than a killed wait's hold (its last step plus the daemon's hold).
const ARM_GRACE: Duration = Duration::from_secs(6);

/// One long-poll step; also how soon a dead host is noticed.
const STEP: Duration = Duration::from_secs(2);

/// The next step after any wait that ended without leaving one armed.
pub const ARM_AGAIN: &str = "While you are queued or hold an interruptible lease, keep one \
     `clerk wait` running with the Bash tool's run_in_background (timeout 7200000); it costs \
     nothing while idle. When it completes, read its output and act on it.";

/// Run `clerk wait` for agent session `caller`.
///
/// # Errors
/// `agent_unidentified` without a usable `CLAUDE_PID`; the daemon's errors.
pub async fn agent(ctx: &Ctx, caller: &Caller, args: &WaitArgs) -> anyhow::Result<()> {
    let host = caller::require_host(caller)?.context("an agent session has a host")?;
    let HolderRef::AgentSession { session_id } = &caller.holder else {
        anyhow::bail!("not an agent session");
    };
    let client = match ctx.waiting_client().await {
        Err(DaemonError::Stopped) => return daemon_stopped(Format::of(ctx.out), args),
        got => got?,
    };
    // This wait's arm token: its steps hold the session's one armed wait.
    let arm = ulid::Ulid::new().to_string();
    let result = steps(ctx, caller, args, &client, host, session_id, &arm).await;
    // However it ended (a batch, a signal, an error), the wait gives its arm
    // up at once, so a wait started right after arms (a Monitor re-armed on
    // expiry). Best effort: the hold lapses by itself anyway.
    let _ = client.agent_disarm(session_id, &arm).await;
    result
}

/// The steps of an agent's wait, until it has something to print or ends.
async fn steps(
    ctx: &Ctx,
    caller: &Caller,
    args: &WaitArgs,
    client: &Client,
    host: leclerk::model::Host,
    session_id: &str,
    arm: &str,
) -> anyhow::Result<()> {
    let md = Format::of(ctx.out);
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let deadline = args.timeout.map(|t| Instant::now() + t);
    let started = Instant::now();
    let mut first = true;
    loop {
        if !process_alive(host.pid, host.start_time) {
            return finish(
                md,
                args,
                &ended("Claude Code process running this session has exited"),
            );
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return finish(md, args, &empty(&["No notices arrived before --timeout."]));
        }
        let step = tokio::select! {
            got = client.agent_wait(session_id, host, STEP.as_secs(), first, arm) => match got {
                Ok(step) => step,
                Err(e) => return daemon_gone(md, args, e),
            },
            _ = term.recv() => return finish(md, args, &stopped()),
            _ = int.recv() => return finish(md, args, &stopped()),
        };
        let was_first = first;
        first = false;
        match step["status"].as_str() {
            Some("timeout") => {}
            // Another wait holds the arm. It may be a wait that was killed
            // outright and could not give it up: wait that hold out once
            // before giving way.
            Some("armed") if was_first && started.elapsed() < ARM_GRACE => {
                tokio::time::sleep(Duration::from_millis(500)).await;
                first = true;
            }
            Some("armed") => {
                return print(
                    md,
                    &json!({
                        "notices": [],
                        "claimed": [],
                        "armed": true,
                        "summary": ["A clerk wait is already armed for this session; this one \
                                     took nothing."],
                        "next": "Nothing: the armed wait reports the notices.",
                    }),
                );
            }
            Some("ended") => {
                return finish(
                    md,
                    args,
                    &ended("this Claude Code process runs another session now"),
                );
            }
            Some("notices") => {
                let all = step["notices"].as_array().cloned().unwrap_or_default();
                let notices = actionable(client, caller, session_id, all).await?;
                if notices.is_empty() {
                    continue;
                }
                if args.follow {
                    for notice in notices {
                        let claimed = claim_if_offered(client, caller, &notice).await;
                        let line = follow_line(&notice, claimed);
                        print(md, &line)?;
                        ack(client, session_id, &[&notice]).await?;
                    }
                } else {
                    let mut claimed = Vec::new();
                    let mut summary = Vec::new();
                    for notice in &notices {
                        match claim_if_offered(client, caller, notice).await {
                            Some(Ok(lease)) => claimed.push(lease),
                            Some(Err(e)) => summary.push(format!(
                                "Could not claim lease {}: {e}.",
                                notice["lease_id"].as_str().unwrap_or("?")
                            )),
                            None => {}
                        }
                        summary.push(action_of(notice));
                    }
                    print(
                        md,
                        &json!({
                            "notices": notices,
                            "claimed": claimed,
                            "summary": summary,
                            "next": format!("Act on each notice as it says. {ARM_AGAIN}"),
                        }),
                    )?;
                    let all: Vec<&Value> = notices.iter().collect();
                    ack(client, session_id, &all).await?;
                    return Ok(());
                }
            }
            other => anyhow::bail!("the daemon answered the wait with {other:?}"),
        }
    }
}

/// The notices of `all` that need the agent; the others are acknowledged
/// here and never printed.
async fn actionable(
    client: &Client,
    caller: &Caller,
    session: &str,
    all: Vec<Value>,
) -> anyhow::Result<Vec<Value>> {
    let mut keep = Vec::new();
    let mut noise = Vec::new();
    for notice in all {
        if needs_no_action(client, caller, &notice).await {
            noise.push(notice);
        } else {
            keep.push(notice);
        }
    }
    ack(client, session, &noise.iter().collect::<Vec<_>>()).await?;
    Ok(keep)
}

/// Whether `notice` needs nothing from the agent, so it must not end a
/// wait (waking an agent re-reads its whole conversation): a queue move, a
/// lease that ended cleanly, or the cleanup notice of a lease already over.
async fn needs_no_action(client: &Client, caller: &Caller, notice: &Value) -> bool {
    match notice["kind"].as_str() {
        Some("position_changed") => true,
        Some("lease_ended") => notice["dirty"].is_null(),
        Some("cleanup_window_started") => {
            let Some(lease) = notice["lease_id"].as_str() else {
                return false;
            };
            client
                .call("get_lease", caller, json!({ "lease_id": lease }))
                .await
                .is_ok_and(|l| l["state"] == "ended")
        }
        _ => false,
    }
}

/// The text a notice tells its holder.
fn action_of(notice: &Value) -> String {
    notice["action"].as_str().unwrap_or_default().to_owned()
}

/// Claim `notice`'s lease when it is an offer: `Some` with the lease or the
/// reason it failed; `None` for any other notice.
async fn claim_if_offered(
    client: &Client,
    caller: &Caller,
    notice: &Value,
) -> Option<Result<Value, String>> {
    if notice["kind"] != "offered" {
        return None;
    }
    let lease = notice["lease_id"].as_str()?;
    // Already held by this session (claimed by an earlier wait that died
    // before acknowledging, or by hand) counts as claimed.
    Some(
        super::lease::claim(client, caller, lease)
            .await
            .map_err(|e| e.message),
    )
}

/// The fields of a notice an agent acts on; the rest (its id, holder,
/// creation time) only lengthen a Monitor event, which Monitor cuts short.
fn compact_notice(notice: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for k in [
        "kind",
        "action",
        "resource",
        "unit",
        "lease_id",
        "request_id",
        "deadline",
        "position",
        "other_purpose",
    ] {
        if let Some(v) = notice.get(k).filter(|v| !v.is_null()) {
            out.insert(k.to_owned(), v.clone());
        }
    }
    Value::Object(out)
}

/// One `--follow` line for `notice`: short, since Monitor cuts long events
/// (the claimed lease's description and usage are in `clerk leases --mine`).
fn follow_line(notice: &Value, claimed: Option<Result<Value, String>>) -> Value {
    let mut line = json!({ "notice": compact_notice(notice) });
    let next = match claimed {
        Some(Ok(lease)) => {
            // Claimed: the offer's deadline, request and "claim it" text are
            // spent.
            line["notice"] = json!({
                "kind": notice["kind"],
                "resource": notice["resource"],
                "unit": notice["unit"],
            });
            let id = lease["lease_id"].as_str().unwrap_or("?").to_owned();
            line["claimed"] = json!({
                "lease_id": id,
                "resource": lease["resource"],
                "unit": lease["unit"],
                "ends_at": lease["ends_at"],
            });
            format!(
                "It is yours (claimed). Read its description and usage with `clerk leases --mine`, \
                 use it, then `clerk release {id}`."
            )
        }
        Some(Err(e)) => format!(
            "Could not claim the offered lease ({e}); run `clerk leases --mine` to see where you \
             stand."
        ),
        None => "Act on the notice as it says.".to_owned(),
    };
    line["next"] = json!(next);
    line
}

/// A wait that ended with nothing to report.
fn empty(summary: &[&str]) -> Value {
    json!({ "notices": [], "claimed": [], "summary": summary, "next": ARM_AGAIN })
}

/// The session is no longer running in this process.
fn ended(why: &str) -> Value {
    json!({
        "notices": [],
        "claimed": [],
        "summary": [format!("The wait ended: {why}.")],
        "next": "Nothing for this session; a new session arms its own wait.",
    })
}

/// End a wait on the daemon's error `e`: said in words when the daemon went
/// away for good, stopped or replaced by one speaking another protocol (a
/// restart is ridden out inside the client).
fn daemon_gone(md: Format, args: &WaitArgs, e: ApiError) -> anyhow::Result<()> {
    match e.code.as_str() {
        "daemon_stopping" => daemon_stopped(md, args),
        "protocol_mismatch" => finish(
            md,
            args,
            &gone(
                "the clerk was upgraded to a version this wait cannot talk to",
                "Start the wait again (the new `clerk` on PATH speaks the new protocol).",
            ),
        ),
        _ => Err(e.into()),
    }
}

/// End a wait whose daemon was stopped. A `--follow` wait also exits
/// [`EXIT_DAEMON`]: nothing will report to it any more.
fn daemon_stopped(md: Format, args: &WaitArgs) -> anyhow::Result<()> {
    finish(
        md,
        args,
        &gone(
            "the clerk daemon was stopped",
            "Tell the user the clerk daemon is stopped; arm a wait again once it runs.",
        ),
    )?;
    if args.follow {
        return Err(Exit(EXIT_DAEMON).into());
    }
    Ok(())
}

/// The daemon went away for good (`why`); `next` says what to do.
fn gone(why: &str, next: &str) -> Value {
    json!({
        "notices": [],
        "claimed": [],
        "summary": [format!("The wait ended: {why}.")],
        "next": next,
    })
}

/// SIGTERM or SIGINT (a background command's time limit).
fn stopped() -> Value {
    empty(&["The wait was stopped (time limit or signal) before any notice arrived."])
}

/// End the wait with `value`: printed for a one-batch wait; a `--follow`
/// wait just ends (Monitor reports the end itself).
fn finish(md: Format, _args: &WaitArgs, value: &Value) -> anyhow::Result<()> {
    // A `--follow` wait says why it ended too: Monitor shows the line, and
    // the agent knows to arm again (or not).
    print(md, value)
}

/// How a wait prints its results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Json,
    Markdown,
    /// The person view (`CLERK_AGENT_FORMAT=human`).
    Human,
}

impl Format {
    fn of(out: super::output::Output) -> Self {
        if out.is_markdown() {
            Self::Markdown
        } else if out.mode() == super::output::Mode::Human {
            Self::Human
        } else {
            Self::Json
        }
    }
}

/// Print `value` (a wait result) and flush: one JSON line, Markdown or the
/// person view, each followed by a blank line.
fn print(format: Format, value: &Value) -> anyhow::Result<()> {
    let mut stdout = std::io::stdout().lock();
    let now = leclerk::clock::Clock::now_ms(&leclerk::clock::SystemClock);
    match format {
        Format::Json => writeln!(stdout, "{value}")?,
        Format::Markdown if value.get("notice").is_some() => {
            writeln!(stdout, "{}\n", super::md::follow(value, now))?;
        }
        Format::Markdown => writeln!(stdout, "{}\n", super::md::wait(value, now))?,
        Format::Human => writeln!(stdout, "{}\n", human(value))?,
    }
    stdout.flush()?;
    Ok(())
}

/// A wait result in the person view: each notice as `[kind] action`, each
/// claimed lease as a grant, then the summary and the next step.
fn human(value: &Value) -> String {
    let list = |k: &str| match value.get(k) {
        Some(Value::Array(a)) => a.clone(),
        Some(v) if !v.is_null() => vec![v.clone()],
        _ => Vec::new(),
    };
    let mut parts: Vec<String> = Vec::new();
    for n in list("notices").into_iter().chain(list("notice")) {
        parts.push(format!(
            "[{}] {}",
            n["kind"].as_str().unwrap_or("notice"),
            n["action"].as_str().unwrap_or_default()
        ));
    }
    for l in list("claimed") {
        parts.push(super::lease::granted_text(&l));
    }
    for line in value["summary"].as_array().into_iter().flatten() {
        if let Some(t) = line.as_str().filter(|t| !t.is_empty()) {
            parts.push(t.to_owned());
        }
    }
    if let Some(next) = value["next"].as_str() {
        parts.push(next.to_owned());
    }
    parts.join("\n")
}

/// Acknowledge `notices` (printed and flushed).
async fn ack(client: &Client, session: &str, notices: &[&Value]) -> anyhow::Result<()> {
    let ids: Vec<String> = notices
        .iter()
        .filter_map(|n| n["id"].as_str().map(str::to_owned))
        .collect();
    if !ids.is_empty() {
        client.agent_ack(session, &ids).await?;
    }
    Ok(())
}
