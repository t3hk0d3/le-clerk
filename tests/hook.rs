//! `clerk hook session-start|stop|pre-tool-use|session-end`: the real
//! binary is run with Claude Code's documented hook input on stdin and
//! `CLAUDE_PID` in its environment, against no daemon, a wedged or slow
//! fake daemon. (Tests against the real daemon return with Part 2's hooks.)

mod common;

use std::fs;
use std::io::Write;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::{get, post};
use leclerk::config::Paths;
use serde_json::{Value, json};
use tempfile::TempDir;

use common::{PATIENCE, SESSION_VAR};

/// Claude Code's budget for session-start, stop and pre-tool-use hooks.
const BUDGET: Duration = Duration::from_secs(2);
/// Claude Code's budget for session-end hooks.
const END_BUDGET: Duration = Duration::from_millis(1500);
/// The variable Claude Code sets for hook commands.
const CLAUDE_PID: &str = "CLAUDE_PID";
/// Every hook event, with its budget.
const EVENTS: [(&str, Duration); 4] = [
    ("session-start", BUDGET),
    ("stop", BUDGET),
    ("pre-tool-use", BUDGET),
    ("session-end", END_BUDGET),
];

// ---------------------------------------------------------------------------
// Running a hook
// ---------------------------------------------------------------------------

/// Run `clerk hook <event>` with `CLERK_HOME=home`, `input` on stdin and
/// `CLAUDE_PID` set to `claude_pid` (unset when `None`); its output and wall
/// time.
fn run_hook(home: &Path, event: &str, input: &str, claude_pid: Option<u32>) -> (Output, Duration) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_clerk"));
    cmd.args(["hook", event])
        .env("CLERK_HOME", home)
        .env_remove(SESSION_VAR)
        .env_remove(CLAUDE_PID)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(pid) = claude_pid {
        cmd.env(CLAUDE_PID, pid.to_string());
    }
    let started = Instant::now();
    let mut child = cmd.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_owned();
    // A hook that stops reading early must not fail the test.
    let writer = thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let output = wait_output(child);
    let took = started.elapsed();
    let _ = writer.join();
    (output, took)
}

/// Wait for `child`, killing it after [`PATIENCE`].
fn wait_output(mut child: Child) -> Output {
    let until = Instant::now() + PATIENCE;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > until {
            let _ = child.kill();
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    child.wait_with_output().unwrap()
}

/// The documented hook input of `event` for `session` with `extra` fields.
fn input(event: &str, session: &str, extra: &Value) -> String {
    let name = match event {
        "session-start" => "SessionStart",
        "stop" => "Stop",
        "pre-tool-use" => "PreToolUse",
        _ => "SessionEnd",
    };
    let mut doc = json!({
        "session_id": session,
        "transcript_path": "/tmp/transcript.jsonl",
        "cwd": "/tmp",
        "permission_mode": "default",
        "hook_event_name": name,
    });
    for (k, v) in extra.as_object().unwrap() {
        doc[k] = v.clone();
    }
    doc.to_string()
}

/// A typical input for `event`.
fn typical(event: &str, session: &str) -> String {
    let extra = match event {
        "session-start" => json!({"source": "resume"}),
        "stop" => json!({"stop_hook_active": false, "last_assistant_message": "done",
                         "background_tasks": [], "session_crons": []}),
        "pre-tool-use" => json!({"tool_name": "Bash", "tool_input": {"command": "ls"},
                                 "tool_use_id": "toolu_1"}),
        _ => json!({"reason": "other"}),
    };
    input(event, session, &extra)
}

/// The clerk paths of `home`.
fn paths_in(home: &Path) -> Paths {
    let home = home.to_path_buf();
    Paths::from_lookup(move |k| (k == "CLERK_HOME").then(|| home.clone().into_os_string())).unwrap()
}

/// No daemon was started in `home`: its lock file was never created.
fn assert_no_daemon(home: &Path) {
    let paths = paths_in(home);
    assert!(!paths.lock.exists(), "{} exists", paths.lock.display());
}

fn assert_silent_within(out: &Output, took: Duration, budget: Duration, what: &str) {
    assert!(out.status.success(), "{what}: {out:?}");
    assert!(out.stdout.is_empty(), "{what}: stdout {out:?}");
    assert!(took < budget, "{what}: took {took:?}, budget {budget:?}");
}

// ---------------------------------------------------------------------------
// Fake daemons
// ---------------------------------------------------------------------------

/// A home whose socket accepts connections but never answers.
struct Wedged {
    home: TempDir,
    _listener: UnixListener,
}

impl Wedged {
    fn new() -> Self {
        let home = TempDir::new().unwrap();
        let run = home.path().join("run");
        fs::create_dir_all(&run).unwrap();
        let listener = UnixListener::bind(paths_in(home.path()).socket).unwrap();
        Self {
            home,
            _listener: listener,
        }
    }
}

/// A home whose daemon answers `/v1/version` at once but never answers an
/// RPC.
struct Slow {
    home: TempDir,
    _runtime: tokio::runtime::Runtime,
}

impl Slow {
    fn new() -> Self {
        let home = TempDir::new().unwrap();
        let run = home.path().join("run");
        fs::create_dir_all(&run).unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let socket = paths_in(home.path()).socket;
        runtime.block_on(async {
            let listener = tokio::net::UnixListener::bind(socket).unwrap();
            let app = Router::new()
                .route(
                    "/v1/version",
                    get(|| async {
                        axum::Json(json!({"version": leclerk::VERSION,
                                          "protocol": leclerk::PROTOCOL, "pid": 1}))
                    }),
                )
                .route(
                    "/v1/rpc/{method}",
                    post(|| async {
                        tokio::time::sleep(Duration::from_secs(3600)).await;
                        ""
                    }),
                );
            tokio::spawn(async move { axum::serve(listener, app).await });
        });
        Self {
            home,
            _runtime: runtime,
        }
    }
}

/// This test process as the hook's Claude Code process.
#[allow(clippy::unnecessary_wraps)] // run_hook takes an optional pid
fn me() -> Option<u32> {
    Some(std::process::id())
}

// ---------------------------------------------------------------------------
// cli::hook
// ---------------------------------------------------------------------------

mod cli {
    use super::*;

    mod hook {
        use super::*;

        mod any_hook {
            use super::*;

            mod on_no_daemon_unresponsive_daemon_malformed_input_or_slow_daemon {
                use super::*;

                #[test]
                fn exits_0_with_no_output_within_its_budget_never_starts_a_daemon_no_daemon() {
                    let home = TempDir::new().unwrap();
                    for (event, budget) in EVENTS {
                        let (out, took) = run_hook(home.path(), event, &typical(event, "s1"), me());
                        assert_silent_within(&out, took, budget, event);
                    }
                    assert!(!paths_in(home.path()).socket.exists());
                    assert_no_daemon(home.path());
                }

                #[test]
                fn exits_0_with_no_output_within_its_budget_never_starts_a_daemon_wedged() {
                    let wedged = Wedged::new();
                    for (event, budget) in EVENTS {
                        let (out, took) =
                            run_hook(wedged.home.path(), event, &typical(event, "s1"), me());
                        assert_silent_within(&out, took, budget, event);
                    }
                    assert_no_daemon(wedged.home.path());
                }

                #[test]
                fn exits_0_with_no_output_within_its_budget_never_starts_a_daemon_slow() {
                    let slow = Slow::new();
                    for (event, budget) in EVENTS {
                        let (out, took) =
                            run_hook(slow.home.path(), event, &typical(event, "s1"), me());
                        assert_silent_within(&out, took, budget, event);
                    }
                    assert_no_daemon(slow.home.path());
                }
            }
        }
    }
}
