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
use leclerk::client::Client;
use leclerk::model::{Caller, HolderRef};
use leclerk::scheduler::liveness::process_alive;
use serde_json::{Value, json};
use tokio::signal::unix::{SignalKind, signal};
use tokio::time::Instant;

use super::Ctx;
use super::caller;
use super::lease::WaitArgs;

/// One long-poll step; also how soon a dead host is noticed.
const STEP: Duration = Duration::from_secs(2);

/// The next step after any wait that ended without leaving one armed.
pub const ARM_AGAIN: &str = "While you hold or wait for anything, keep a wait armed: \
     `clerk wait --follow` with the Monitor tool (timeout_ms 1800000), or `clerk wait` \
     in the background.";

/// Run `clerk wait` for agent session `caller`.
///
/// # Errors
/// `agent_unidentified` without a usable `CLAUDE_PID`; the daemon's errors.
pub async fn agent(ctx: &Ctx, caller: &Caller, args: &WaitArgs) -> anyhow::Result<()> {
    let host = caller::require_host(caller)?.context("an agent session has a host")?;
    let HolderRef::AgentSession { session_id } = &caller.holder else {
        anyhow::bail!("not an agent session");
    };
    let client = ctx.client().await?;
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
            got = client.agent_wait(session_id, host, STEP.as_secs(), first, arm) => got?,
            _ = term.recv() => return finish(md, args, &stopped()),
            _ = int.recv() => return finish(md, args, &stopped()),
        };
        first = false;
        match step["status"].as_str() {
            Some("timeout") => {}
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
                let notices = step["notices"].as_array().cloned().unwrap_or_default();
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

/// One `--follow` line for `notice`.
fn follow_line(notice: &Value, claimed: Option<Result<Value, String>>) -> Value {
    let mut summary = vec![action_of(notice)];
    let mut line = json!({ "notice": notice });
    let next = match claimed {
        Some(Ok(lease)) => {
            summary.push(format!(
                "Claimed: you hold lease {} now; read its description and usage in `claimed`.",
                lease["lease_id"].as_str().unwrap_or("?")
            ));
            line["claimed"] = lease;
            "Use the unit, then `clerk release <lease>` when you are done.".to_owned()
        }
        Some(Err(e)) => {
            summary.push(format!("Could not claim the offered lease: {e}."));
            "Run `clerk leases --mine` to see where you stand.".to_owned()
        }
        None => "Act on the notice as it says.".to_owned(),
    };
    line["summary"] = json!(summary);
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

/// SIGTERM or SIGINT (a background command's time limit).
fn stopped() -> Value {
    empty(&["The wait was stopped (time limit or signal) before any notice arrived."])
}

/// End the wait with `value`: printed for a one-batch wait; a `--follow`
/// wait just ends (Monitor reports the end itself).
fn finish(md: Format, args: &WaitArgs, value: &Value) -> anyhow::Result<()> {
    if args.follow {
        return Ok(());
    }
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
