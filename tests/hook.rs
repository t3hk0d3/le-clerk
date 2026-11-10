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

use common::{Env, PATIENCE, SESSION_VAR};

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

// ---------------------------------------------------------------------------
// The real daemon, an agent session and its host
// ---------------------------------------------------------------------------

/// A process standing for a Claude Code process; killed when dropped.
struct Host(Child);

impl Host {
    fn spawn() -> Self {
        Self(Command::new("sleep").arg("300").spawn().unwrap())
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Run `clerk <args>` as agent session `session` hosted by `host`, in a
/// project directory of `env`; its JSON output and exit code.
fn agent(env: &Env, session: &str, host: &Host, args: &[&str]) -> (Value, Option<i32>) {
    let out = env
        .cmd()
        .args(args)
        .env(SESSION_VAR, session)
        .env(CLAUDE_PID, host.pid().to_string())
        .current_dir(env.dir("p"))
        .output()
        .unwrap();
    let body = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    (body, out.status.code())
}

/// `board` with one unit, registered by the user.
fn seed(env: &Env) {
    env.ok(&["resource", "create", "board", "--type", "generic"]);
    env.ok(&["unit", "add", "board", "u1", "--key", "k1"]);
}

/// `session` hosted by `host` holds `board/u1`; the lease id.
fn holds(env: &Env, session: &str, host: &Host) -> String {
    let (body, code) = agent(env, session, host, &["acquire", "board", "--for", "10m"]);
    assert_eq!(code, Some(0), "{body}");
    body["lease_id"].as_str().unwrap().to_owned()
}

/// The live leases, as the user sees them.
fn live_leases(env: &Env) -> usize {
    env.ok(&["leases"]).as_array().map_or(0, Vec::len)
}

/// The hook's `additionalContext`.
fn context_of(out: &Output) -> String {
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {out:?}"));
    v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or_else(|| panic!("no additionalContext: {v}"))
        .to_owned()
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

        mod on_no_agent_variables {
            use super::*;

            #[test]
            fn exits_0_at_once_with_no_output_without_asking_the_daemon() {
                let slow = Slow::new();
                for (event, _) in EVENTS {
                    let (out, took) =
                        run_hook(slow.home.path(), event, &typical(event, "s1"), None);
                    assert_silent_within(&out, took, Duration::from_millis(500), event);
                }
            }
        }

        mod session_start {
            use super::*;

            fn start(env: &Env, session: &str, source: &str, host: &Host) -> Output {
                let doc = input("session-start", session, &json!({ "source": source }));
                run_hook(env.home(), "session-start", &doc, Some(host.pid())).0
            }

            mod on_clear_after_the_previous_session_released_leases {
                use super::*;

                #[test]
                fn the_brief_names_what_was_released() {
                    let env = Env::new();
                    seed(&env);
                    let host = Host::spawn();
                    let lease = holds(&env, "s1", &host);
                    let end = input("session-end", "s1", &json!({ "reason": "clear" }));
                    run_hook(env.home(), "session-end", &end, Some(host.pid()));
                    let context = context_of(&start(&env, "s2", "clear", &host));
                    assert!(context.contains(&lease), "{context}");
                    assert!(context.contains("released"), "{context}");
                }
            }

            mod on_resume_while_holding {
                use super::*;

                #[test]
                fn the_brief_lists_the_lease_and_the_wait_command() {
                    let env = Env::new();
                    seed(&env);
                    let old = Host::spawn();
                    let lease = holds(&env, "s1", &old);
                    drop(old);
                    let new = Host::spawn();
                    let context = context_of(&start(&env, "s1", "resume", &new));
                    assert!(context.contains(&lease), "{context}");
                    assert!(context.contains("clerk wait --follow"), "{context}");
                    assert!(context.contains("clerk skill"), "{context}");
                }

                #[test]
                fn re_hosts_the_session_so_its_lease_survives_the_grace() {
                    let env = Env::new();
                    std::fs::create_dir_all(env.home().join("config")).unwrap();
                    std::fs::write(
                        env.home().join("config/config.toml"),
                        "reconnect_grace = 2\n",
                    )
                    .unwrap();
                    seed(&env);
                    let old = Host::spawn();
                    holds(&env, "s1", &old);
                    drop(old);
                    let new = Host::spawn();
                    start(&env, "s1", "resume", &new);
                    thread::sleep(Duration::from_secs(5));
                    assert_eq!(live_leases(&env), 1);
                }
            }

            mod on_startup_or_holding_nothing {
                use super::*;

                #[test]
                fn prints_nothing() {
                    let env = Env::new();
                    seed(&env);
                    let host = Host::spawn();
                    assert_eq!(start(&env, "s1", "startup", &host).stdout, b"");
                    assert_eq!(start(&env, "s1", "compact", &host).stdout, b"");
                }
            }
        }

        mod stop {
            use super::*;

            fn stop(env: &Env, session: &str, host: &Host, active: bool) -> Output {
                let doc = input("stop", session, &json!({ "stop_hook_active": active }));
                run_hook(env.home(), "stop", &doc, Some(host.pid())).0
            }

            #[test]
            fn on_holding_with_no_wait_blocks_naming_the_wait() {
                let env = Env::new();
                seed(&env);
                let host = Host::spawn();
                holds(&env, "s1", &host);
                let out = stop(&env, "s1", &host, false);
                let v: Value = serde_json::from_slice(&out.stdout).unwrap();
                assert_eq!(v["decision"], "block", "{v}");
                assert!(
                    v["reason"]
                        .as_str()
                        .unwrap()
                        .contains("clerk wait --follow"),
                    "{v}"
                );
            }

            #[test]
            fn on_stop_hook_active_or_holding_nothing_prints_nothing() {
                let env = Env::new();
                seed(&env);
                let host = Host::spawn();
                assert_eq!(stop(&env, "s1", &host, false).stdout, b"");
                holds(&env, "s1", &host);
                assert_eq!(stop(&env, "s1", &host, true).stdout, b"");
            }
        }

        mod session_end {
            use super::*;

            fn end(env: &Env, session: &str, host: &Host, reason: &str) {
                let doc = input("session-end", session, &json!({ "reason": reason }));
                let (out, took) = run_hook(env.home(), "session-end", &doc, Some(host.pid()));
                assert_silent_within(&out, took, END_BUDGET, reason);
            }

            #[test]
            fn on_any_reason_from_the_host_the_lease_ends_at_once() {
                let env = Env::new();
                seed(&env);
                let host = Host::spawn();
                holds(&env, "s1", &host);
                end(&env, "s1", &host, "clear");
                assert_eq!(live_leases(&env), 0);
            }

            #[test]
            fn from_a_process_that_is_not_the_host_nothing_changes() {
                let env = Env::new();
                seed(&env);
                let host = Host::spawn();
                let other = Host::spawn();
                holds(&env, "s1", &host);
                end(&env, "s1", &other, "prompt_input_exit");
                assert_eq!(live_leases(&env), 1);
            }
        }
    }
}
