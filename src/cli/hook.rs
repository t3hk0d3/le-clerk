//! `clerk hook session-start|stop|pre-tool-use|session-end`: the Claude Code
//! hooks of the Le Clerk plugin.
//!
//! - Each reads one JSON document (Claude Code's documented hook input,
//!   at most [`MAX_INPUT`] bytes) from stdin and `CLAUDE_PID` (the Claude
//!   Code process, the session's host) from the environment, and asks the
//!   daemon about the input's session (see [`leclerk::engine::HookCommand`]).
//! - They never start the daemon (probe only), and they fail open: on any
//!   error, timeout, missing daemon or malformed input they exit 0 with no
//!   output. Everything, start to exit, fits Claude Code's budget: 2 s,
//!   1.5 s for session-end (whose RPC gets at most [`END_RPC_LIMIT`]).
//! - stdout carries only the documented hook output, or nothing.
//!
//! Per event:
//! - session-start: reports the session's host (the Claude Code process),
//!   which unhosts the session that process ran before (`/clear`).
//! - stop: a block decision telling the agent to start `clerk wait`, when
//!   the session holds a live lease or a queued request, has no open wait,
//!   and `stop_hook_active` is false.
//! - pre-tool-use (not inside a subagent, whose input has `agent_id`): the
//!   session's pending notices that no hook has shown yet, as
//!   `additionalContext` (at most [`MAX_CONTEXT`] characters; the rest are
//!   counted and shown by a later hook), then marked shown. They stay
//!   pending for `clerk wait`.
//! - session-end: the session's leases and requests end now, when this
//!   process is the session's host.

use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use clap::ValueEnum;
use leclerk::client::{self, Client, PROBE_TIMEOUT};
use leclerk::config::Paths;
use leclerk::model::{Caller, HolderRef, Host};
use serde::Deserialize;
use serde_json::{Value, json};

use super::caller;

/// Claude Code's budget for session-start, stop and pre-tool-use hooks.
const BUDGET: Duration = Duration::from_secs(2);
/// Claude Code's budget for session-end hooks.
const END_BUDGET: Duration = Duration::from_millis(1500);
/// Kept back from the budget for process start-up, exit and output.
const MARGIN: Duration = Duration::from_millis(300);
/// The longest a session-end RPC may take.
const END_RPC_LIMIT: Duration = Duration::from_secs(1);
/// The most hook input read; more is malformed input.
const MAX_INPUT: u64 = 8 * 1024 * 1024;
/// The longest `additionalContext` Claude Code takes, in characters.
const MAX_CONTEXT: usize = 10_000;
/// The longest line one notice takes in the context, in characters.
const MAX_NOTICE_LINE: usize = 1_000;
/// Room kept for the "and N more" line.
const SUMMARY_ROOM: usize = 120;

/// The hook event a `clerk hook` runs for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Event {
    /// `SessionStart`.
    SessionStart,
    /// `Stop`.
    Stop,
    /// `PreToolUse`.
    PreToolUse,
    /// `SessionEnd`.
    SessionEnd,
}

impl Event {
    /// Claude Code's budget for the hook.
    fn budget(self) -> Duration {
        match self {
            Self::SessionEnd => END_BUDGET,
            _ => BUDGET,
        }
    }
}

/// The fields of Claude Code's hook input the hooks use; the others are
/// ignored.
#[derive(Debug, Deserialize)]
struct HookInput {
    session_id: String,
    /// `SessionStart`.
    #[serde(default)]
    source: Option<String>,
    /// `SessionEnd`.
    #[serde(default)]
    reason: Option<String>,
    /// `Stop`.
    #[serde(default)]
    stop_hook_active: bool,
    /// Set inside a subagent.
    #[serde(default)]
    agent_id: Option<String>,
}

/// Run the hook; always exit 0. Only the hook's documented output reaches
/// stdout, and only when everything succeeded in time.
pub fn main(event: Event) -> ExitCode {
    // Run by something other than an agent (no Claude Code variables):
    // nothing to do, and the daemon is not asked.
    if !agent_detected(|k| std::env::var_os(k)) {
        return ExitCode::SUCCESS;
    }
    let deadline = Instant::now() + event.budget().saturating_sub(MARGIN);
    if let Ok(Some(output)) = run(event, deadline) {
        let mut stdout = io::stdout().lock();
        // Nothing else can be done about a closed stdout.
        let _ = writeln!(stdout, "{output}").and_then(|()| stdout.flush());
    }
    ExitCode::SUCCESS
}

/// Whether an agent runs the hook: Claude Code, which sets
/// `CLAUDE_CODE_SESSION_ID` or `CLAUDE_PID` for hook commands, is the only
/// agent so far.
fn agent_detected(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> bool {
    [caller::SESSION_VAR, caller::PID_VAR]
        .iter()
        .any(|k| var(k).is_some_and(|v| !v.is_empty()))
}

/// The hook's output, if it has one.
fn run(event: Event, deadline: Instant) -> anyhow::Result<Option<Value>> {
    let input = read_input(deadline)?;
    let host = caller::host();
    let paths = Paths::resolve()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let hook = Hook {
        paths,
        input,
        host,
        deadline,
    };
    let result = runtime.block_on(async {
        tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), hook.run(event))
            .await
            .context("the hook ran out of time")?
    });
    // A request still in flight is abandoned with the process.
    runtime.shutdown_background();
    result
}

/// Read and parse one hook input document from stdin, by `deadline`.
fn read_input(deadline: Instant) -> anyhow::Result<HookInput> {
    let (tx, rx) = mpsc::channel();
    // The reader is abandoned if stdin never ends; the process exits
    // without it.
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let read = io::stdin()
            .lock()
            .take(MAX_INPUT + 1)
            .read_to_end(&mut bytes);
        let _ = tx.send(read.map(|_| bytes));
    });
    let left = deadline.saturating_duration_since(Instant::now());
    let bytes = rx.recv_timeout(left).context("no hook input in time")??;
    if bytes.len() as u64 > MAX_INPUT {
        bail!("the hook input is larger than {MAX_INPUT} bytes");
    }
    let input: HookInput = serde_json::from_slice(&bytes).context("bad hook input")?;
    if input.session_id.is_empty() {
        bail!("the hook input has an empty session_id");
    }
    Ok(input)
}

/// One hook run.
struct Hook {
    paths: Paths,
    input: HookInput,
    /// The Claude Code process running the session (`CLAUDE_PID`).
    host: Option<Host>,
    deadline: Instant,
}

impl Hook {
    async fn run(&self, event: Event) -> anyhow::Result<Option<Value>> {
        match event {
            Event::SessionStart => self.session_start().await,
            Event::Stop => self.stop().await,
            Event::PreToolUse => self.pre_tool_use().await,
            Event::SessionEnd => self.session_end().await,
        }
    }

    /// Time left until the deadline.
    fn left(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    /// A client of the running daemon; never starts one.
    async fn client(&self) -> anyhow::Result<Session> {
        let client = Client::connect_within(&self.paths, self.left().min(PROBE_TIMEOUT)).await?;
        Ok(Session {
            client,
            caller: client::cli_caller(),
            args: json!({
                "session_id": self.input.session_id,
                "host": self.host,
            }),
        })
    }

    /// Report the session's host, whatever the source (that unhosts the
    /// session this process ran before, e.g. on `/clear`).
    async fn session_start(&self) -> anyhow::Result<Option<Value>> {
        let Some(source) = self.input.source.as_deref() else {
            bail!("the session-start input has no source");
        };
        if self.host.is_none() {
            return Ok(None);
        }
        let session = self.client().await?;
        let started = session
            .call("session_start", json!({ "source": source }), self.left())
            .await?;
        if !matches!(source, "clear" | "compact" | "resume") {
            return Ok(None);
        }
        let caller = Caller {
            holder: HolderRef::AgentSession {
                session_id: self.input.session_id.clone(),
            },
            project: None,
            override_reason: None,
        };
        let mine = session
            .client
            .call_within("list_my_leases", &caller, json!({}), self.left())
            .await?;
        let brief = session
            .call("session_brief", json!({}), self.left())
            .await?;
        let armed = brief["open_waits"].as_u64().unwrap_or(0) > 0;
        let released = started["released"].as_array().cloned().unwrap_or_default();
        Ok(start_brief(&mine, armed, &released).map(|context| {
            json!({
                "hookSpecificOutput": {
                    "hookEventName": "SessionStart",
                    "additionalContext": context,
                }
            })
        }))
    }

    /// Block once while the session holds or waits for something without
    /// an armed `clerk wait`.
    async fn stop(&self) -> anyhow::Result<Option<Value>> {
        if self.input.stop_hook_active {
            return Ok(None);
        }
        let session = self.client().await?;
        let brief = session
            .call("session_brief", json!({}), self.left())
            .await?;
        let leases = count(&brief, "leases")?;
        let requests = count(&brief, "requests")?;
        let open_waits = count(&brief, "open_waits")?;
        if leases + requests == 0 || open_waits > 0 {
            return Ok(None);
        }
        Ok(Some(json!({
            "decision": "block",
            "reason": stop_reason(leases, requests),
        })))
    }

    /// Show the notices no hook has shown yet, and mark them shown.
    async fn pre_tool_use(&self) -> anyhow::Result<Option<Value>> {
        // A subagent's tool call: the notices are for the main agent, which
        // holds the leases and arms the wait.
        if self.input.agent_id.is_some() {
            return Ok(None);
        }
        let session = self.client().await?;
        let brief = session
            .call("session_brief", json!({}), self.left())
            .await?;
        let unshown = brief["unshown"]
            .as_array()
            .context("the brief has no unshown notices")?;
        let Some((context, ids)) = notice_context(unshown) else {
            return Ok(None);
        };
        session
            .call("mark_notices_shown", json!({ "ids": ids }), self.left())
            .await?;
        Ok(Some(json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "additionalContext": context,
            }
        })))
    }

    /// End the session now (the daemon keeps it on `clear` / `resume`
    /// while its proxy is connected).
    async fn session_end(&self) -> anyhow::Result<Option<Value>> {
        let Some(reason) = self.input.reason.as_deref() else {
            bail!("the session-end input has no reason");
        };
        let session = self.client().await?;
        session
            .call(
                "session_end",
                json!({ "reason": reason }),
                self.left().min(END_RPC_LIMIT),
            )
            .await?;
        Ok(None)
    }
}

/// A connected client and the hook's session arguments.
struct Session {
    client: Client,
    caller: Caller,
    args: Value,
}

impl Session {
    /// Call hook RPC `method` with the session arguments plus `extra`.
    async fn call(&self, method: &str, extra: Value, limit: Duration) -> anyhow::Result<Value> {
        let mut args = self.args.clone();
        if let (Some(args), Value::Object(extra)) = (args.as_object_mut(), extra) {
            args.extend(extra);
        }
        Ok(self
            .client
            .call_within(method, &self.caller, args, limit)
            .await?)
    }
}

/// A count field of a brief.
fn count(brief: &Value, field: &str) -> anyhow::Result<u64> {
    brief[field]
        .as_u64()
        .with_context(|| format!("the brief has no {field}"))
}

/// The Stop hook's reason: what is held, and the call to make.
fn stop_reason(leases: u64, requests: u64) -> String {
    let what = match (leases, requests) {
        (0, r) => plural(r, "queued request"),
        (l, 0) => plural(l, "live lease"),
        (l, r) => format!(
            "{} and {}",
            plural(l, "live lease"),
            plural(r, "queued request")
        ),
    };
    format!(
        "Le Clerk: this session has {what} but no clerk wait is armed, so offers, yield \
         requests and expiry warnings would go unanswered. {WAIT_HOW} Release leases you no \
         longer need."
    )
}

/// How an agent keeps a wait armed.
const WAIT_HOW: &str = "Arm a wait: the Monitor tool on `clerk wait --follow` (timeout_ms \
     1800000, re-armed when it expires), or `clerk wait` in the background (read its output \
     when it completes, then start it again).";

/// The `SessionStart` brief for a session that holds `mine` (the
/// `list_my_leases` answer), with a wait `armed` or not, after its process's
/// previous session `released` these leases. `None` when there is nothing
/// to say.
fn start_brief(mine: &Value, armed: bool, released: &[Value]) -> Option<String> {
    let leases = mine["leases"].as_array().map_or(&[][..], Vec::as_slice);
    let requests = mine["requests"].as_array().map_or(&[][..], Vec::as_slice);
    if leases.is_empty() && requests.is_empty() && released.is_empty() {
        return None;
    }
    let mut text = String::from("Le Clerk (shared resources; use the clerk skill):\n");
    if !released.is_empty() {
        let ids: Vec<String> = released.iter().map(super::output::plain).collect();
        let _ = writeln!(
            text,
            "- The previous conversation ended, so its leases were released: {}.",
            ids.join(", ")
        );
    }
    for l in leases {
        let _ = writeln!(
            text,
            "- You hold lease {} on {}/{} ({}).",
            super::output::plain(&l["lease_id"]),
            super::output::plain(&l["resource"]),
            super::output::plain(&l["unit"]),
            super::output::plain(&l["state"]),
        );
    }
    for r in requests {
        let _ = writeln!(
            text,
            "- You are queued for {} (request {}).",
            super::output::plain(&r["resource"]),
            super::output::plain(&r["request_id"]),
        );
    }
    if leases.is_empty() && requests.is_empty() {
        return Some(text.trim_end().to_owned());
    }
    if armed {
        text.push_str("- A clerk wait is armed; act on its notices as they arrive.");
    } else {
        let _ = write!(text, "- No clerk wait is armed. {WAIT_HOW}");
    }
    Some(text)
}

fn plural(n: u64, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else {
        format!("{n} {what}s")
    }
}

/// The `additionalContext` for `notices` and the ids it shows; `None`
/// without notices. At most [`MAX_CONTEXT`] characters: notices that do
/// not fit are counted, not shown (a later hook shows them).
fn notice_context(notices: &[Value]) -> Option<(String, Vec<String>)> {
    // A notice without an id cannot be marked shown: it is left out.
    let notices: Vec<(&str, &Value)> = notices
        .iter()
        .filter_map(|n| n["id"].as_str().map(|id| (id, n)))
        .collect();
    if notices.is_empty() {
        return None;
    }
    let mut context = format!(
        "Le Clerk: {} for this session. Act on them; your armed clerk wait also reports \
         them.\n",
        plural(notices.len() as u64, "pending notice")
    );
    let mut ids = Vec::new();
    let mut used = context.chars().count();
    for (id, notice) in &notices {
        let line: String = notice_line(notice).chars().take(MAX_NOTICE_LINE).collect();
        let len = line.chars().count() + 1;
        if used + len + SUMMARY_ROOM > MAX_CONTEXT {
            break;
        }
        context.push_str(&line);
        context.push('\n');
        used += len;
        ids.push((*id).to_owned());
    }
    let rest = notices.len() - ids.len();
    if rest > 0 {
        // Writing to a String cannot fail.
        let _ = writeln!(
            context,
            "…and {} not shown here; the clerk wait reports them.",
            plural(rest as u64, "more notice")
        );
    }
    Some((context.trim_end().to_owned(), ids))
}

/// One notice as a line: id, kind, what it is about, and what to do.
fn notice_line(notice: &Value) -> String {
    let text = |field: &str| notice[field].as_str().map(str::to_owned);
    let mut about = Vec::new();
    match (text("resource"), text("unit")) {
        (Some(r), Some(u)) => about.push(format!("{r}/{u}")),
        (Some(r), None) => about.push(r),
        _ => {}
    }
    if let Some(lease) = text("lease_id") {
        about.push(format!("lease {lease}"));
    }
    if let Some(request) = text("request_id") {
        about.push(format!("request {request}"));
    }
    let kind = text("kind").unwrap_or_default();
    let id = text("id").unwrap_or_default();
    let action = text("action").unwrap_or_default();
    if about.is_empty() {
        format!("- [{id}] {kind}: {action}")
    } else {
        format!("- [{id}] {kind} ({}): {action}", about.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(id: &str, action: &str) -> Value {
        json!({"id": id, "kind": "offered", "resource": "board", "unit": "u1",
               "lease_id": "L1", "action": action})
    }

    #[test]
    fn context_is_none_without_notices() {
        assert!(notice_context(&[]).is_none());
    }

    #[test]
    fn context_lists_each_notice_with_its_id_and_action() {
        let (context, ids) = notice_context(&[notice("N1", "Claim it.")]).unwrap();
        assert_eq!(ids, vec!["N1".to_owned()]);
        assert!(context.contains("[N1] offered (board/u1, lease L1): Claim it."));
        assert!(context.contains("clerk wait"));
    }

    #[test]
    fn context_stays_within_the_cap_and_counts_the_rest() {
        let action = "x".repeat(900);
        let notices: Vec<Value> = (0..50).map(|i| notice(&format!("N{i}"), &action)).collect();
        let (context, ids) = notice_context(&notices).unwrap();
        assert!(context.chars().count() <= MAX_CONTEXT);
        assert!(ids.len() < 50 && !ids.is_empty());
        assert!(context.contains(&format!("{} more notices", 50 - ids.len())));
    }

    #[test]
    fn context_leaves_out_notices_without_an_id() {
        let mut no_id = notice("N0", "Lost.");
        no_id.as_object_mut().unwrap().remove("id");
        assert!(notice_context(std::slice::from_ref(&no_id)).is_none());
        let (context, ids) = notice_context(&[no_id, notice("N1", "Claim it.")]).unwrap();
        assert_eq!(ids, vec!["N1".to_owned()]);
        assert!(context.contains("1 pending notice "), "{context}");
        assert!(!context.contains("more notice"), "{context}");
    }

    #[test]
    fn stop_reason_names_the_wait_and_what_is_held() {
        let reason = stop_reason(1, 2);
        assert!(reason.contains("1 live lease and 2 queued requests"));
        assert!(reason.contains("clerk wait --follow"));
    }

    #[test]
    fn agent_detected_by_either_variable() {
        let only =
            |name: &'static str| move |k: &str| (k == name).then(|| std::ffi::OsString::from("1"));
        assert!(agent_detected(only("CLAUDE_PID")));
        assert!(agent_detected(only("CLAUDE_CODE_SESSION_ID")));
        assert!(!agent_detected(|_| None));
        assert!(!agent_detected(|_| Some(std::ffi::OsString::new())));
    }

    #[test]
    fn start_brief_is_none_with_nothing_to_say() {
        assert_eq!(
            start_brief(&json!({"leases": [], "requests": []}), false, &[]),
            None
        );
    }
}
