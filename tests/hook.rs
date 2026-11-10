//! `clerk hook session-start|stop|pre-tool-use|session-end`: the real
//! binary is run with Claude Code's documented hook input on stdin and
//! `CLAUDE_PID` in its environment, against no daemon, a wedged or slow
//! fake daemon, or the real daemon with `clerk mcp` proxies spawned by this
//! test process (so their Claude PID is this process's pid).

mod common;

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::{get, post};
use leclerk::config::Paths;
use leclerk::model::{Caller, HolderRef};
use leclerk::store::{Store, StoreError};
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

/// The hook's stdout as JSON.
fn stdout_json(out: &Output) -> Value {
    assert!(out.status.success(), "{out:?}");
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {out:?}"))
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
// The real daemon and proxies
// ---------------------------------------------------------------------------

/// A running `clerk mcp` whose parent (its Claude PID) is this test
/// process, or with [`Proxy::spawn_under_shell`] a shell standing for
/// another Claude Code process. Dropping it kills it.
struct Proxy {
    child: Child,
    /// The child is a shell leading its own process group (the proxy is
    /// in it).
    group: bool,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<Value>,
    seen: Vec<Value>,
    next_id: u64,
}

impl Proxy {
    fn spawn(env: &Env, session: &str) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_clerk"));
        cmd.arg("mcp");
        Self::spawn_with(env, session, cmd, false)
    }

    /// A proxy whose parent is a shell: its Claude PID is not this
    /// process's.
    fn spawn_under_shell(env: &Env, session: &str) -> Self {
        let mut cmd = Command::new("sh");
        // `; :` keeps the shell from exec-ing the proxy.
        cmd.args(["-c", "\"$CLERK\" mcp; :"])
            .env("CLERK", env!("CARGO_BIN_EXE_clerk"))
            .process_group(0);
        Self::spawn_with(env, session, cmd, true)
    }

    fn spawn_with(env: &Env, session: &str, mut cmd: Command, group: bool) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let stderr = File::create(env.home().join(format!("proxy-{n}.err"))).unwrap();
        let mut child = cmd
            .env("CLERK_HOME", env.home())
            .env(SESSION_VAR, session)
            .current_dir(env.dir("p"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if tx.send(value).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            group,
            stdin,
            lines,
            seen: Vec::new(),
            next_id: 1,
        }
    }

    /// Send `tools/call` of `name` in 2026-07-28 framing; its id.
    fn send_tool(&mut self, name: &str, args: &Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let body = json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {
        "name": name, "arguments": args,
        "_meta": {
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientInfo": {"name": "hook-test", "version": "0"},
            "io.modelcontextprotocol/clientCapabilities": {},
        }}});
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{body}").unwrap();
        stdin.flush().unwrap();
        id
    }

    /// The response to `id`.
    fn response(&mut self, id: u64) -> Value {
        if let Some(i) = self.seen.iter().position(|m| m["id"] == json!(id)) {
            return self.seen.remove(i);
        }
        let until = Instant::now() + PATIENCE;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            let message = self
                .lines
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("no response to {id}"));
            if message["id"] == json!(id) {
                return message;
            }
            if message.get("id").is_some() {
                self.seen.push(message);
            }
        }
    }

    /// `tools/call` that must succeed; its structured content.
    fn tool(&mut self, name: &str, args: &Value) -> Value {
        let id = self.send_tool(name, args);
        let reply = self.response(id);
        let result = &reply["result"];
        assert!(result.is_object(), "{reply}");
        assert_ne!(result["isError"], json!(true), "{reply}");
        result["structuredContent"].clone()
    }

    fn kill(&mut self) {
        if self.group {
            let pgid = libc::pid_t::try_from(self.child.id()).unwrap();
            // SAFETY: kill(2) on our own child's process group.
            unsafe { libc::kill(-pgid, libc::SIGKILL) };
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.kill();
    }
}

fn agent(session: &str) -> Caller {
    Caller {
        holder: HolderRef::AgentSession {
            session_id: session.into(),
        },
        project: None,
        override_reason: None,
    }
}

fn user(tag: &str) -> Caller {
    Caller {
        holder: HolderRef::User { tag: tag.into() },
        project: None,
        override_reason: None,
    }
}

/// This test process: the Claude Code process of every proxy it spawns.
// `Some` always: it goes where an optional Claude PID is taken.
#[allow(clippy::unnecessary_wraps)]
fn me() -> Option<u32> {
    Some(std::process::id())
}

/// Create user-scoped `board` with one unit, through `p`.
fn board(p: &mut Proxy) {
    p.tool(
        "resource_create",
        &json!({"id": "board", "type": "generic", "scope": "user"}),
    );
    p.tool(
        "unit_add",
        &json!({"resource": "board", "id": "u1",
               "settings": {"type": "generic", "identity_key": "k"}}),
    );
}

/// Create `board` and acquire it through `p`; the lease id.
fn board_held_by(p: &mut Proxy) -> String {
    board(p);
    let grant = p.tool(
        "lease_acquire",
        &json!({"target": {"resource": "board"}, "for": "1h", "purpose": "flash"}),
    );
    assert_eq!(grant["outcome"], json!("granted"), "{grant}");
    grant["lease_id"].as_str().unwrap().to_owned()
}

/// The ids of `session`'s live leases.
fn leases_of(env: &Env, session: &str) -> Vec<String> {
    let mine = env
        .rpc("list_my_leases", &agent(session), json!({}))
        .unwrap();
    mine["leases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["lease_id"].as_str().unwrap().to_owned())
        .collect()
}

/// The daemon's hook brief for `session_id` from Claude Code process
/// `claude_pid`.
fn brief(env: &Env, session_id: &str, claude_pid: Option<u32>) -> Value {
    env.rpc(
        "session_brief",
        &user("hook"),
        json!({"session_id": session_id, "claude_pid": claude_pid}),
    )
    .unwrap()
}

/// Wait until this process's live proxy session is `session`.
fn wait_connected(env: &Env, session: &str) {
    let until = Instant::now() + PATIENCE;
    loop {
        let got = brief(env, session, me());
        if got["session"] == json!(session) && got["connected"] == json!(true) {
            return;
        }
        assert!(Instant::now() < until, "{session} never connected: {got}");
        thread::sleep(Duration::from_millis(20));
    }
}

/// Wait until `session`'s killed proxy is noticed gone. (In real use the
/// old Claude Code process has another PID; here every proxy shares this
/// process's, so a stream not yet noticed closed would still resolve.)
fn wait_disconnected(env: &Env, session: &str) {
    let until = Instant::now() + PATIENCE;
    loop {
        let got = brief(env, session, None);
        if got["connected"] == json!(false) {
            return;
        }
        assert!(Instant::now() < until, "{session} stayed connected: {got}");
        thread::sleep(Duration::from_millis(20));
    }
}

/// `p` (session `session`) queues for `board`, held by user `u`; then `u`
/// gives it back, so `session` gets an `offered` notice. The notice id.
fn offered_to(env: &Env, p: &mut Proxy, session: &str) -> String {
    board(p);
    let held = env
        .rpc(
            "acquire",
            &user("u"),
            json!({"target": {"resource": "board"}, "duration_ms": 60_000, "purpose": "first"}),
        )
        .unwrap();
    assert_eq!(held["outcome"], json!("granted"), "{held}");
    let queued = p.tool(
        "lease_acquire",
        &json!({"target": {"resource": "board"}, "for": "1h", "purpose": "flash"}),
    );
    assert_eq!(queued["outcome"], json!("queued"), "{queued}");
    let lease = held["lease_id"].clone();
    env.rpc("release", &user("u"), json!({"lease_id": lease}))
        .unwrap();
    env.rpc("confirm", &user("u"), json!({"lease_id": lease}))
        .unwrap();
    let until = Instant::now() + PATIENCE;
    loop {
        let pending = pending_notice_ids(env, session);
        if let Some(id) = pending.into_iter().next() {
            return id;
        }
        assert!(Instant::now() < until, "no notice for {session}");
        thread::sleep(Duration::from_millis(50));
    }
}

/// The ids of `session`'s undelivered notices, read from the database.
fn pending_notice_ids(env: &Env, session: &str) -> Vec<String> {
    let holder = serde_json::to_string(&agent(session).holder).unwrap();
    let mut store = Store::open(&env.paths().db).unwrap();
    store
        .tx(|tx| -> Result<_, StoreError> {
            let mut stmt = tx.conn().prepare(
                "SELECT notice_id FROM notices WHERE holder = ?1 \
                 AND delivered_at IS NULL ORDER BY created_at, row_id",
            )?;
            let rows = stmt
                .query_map([holder], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .unwrap()
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

                #[test]
                fn exits_0_with_no_output_within_its_budget_never_starts_a_daemon_malformed() {
                    let env = Env::new();
                    let mut p = Proxy::spawn(&env, "s1");
                    board_held_by(&mut p);
                    let oversized = format!(
                        "{{\"session_id\":\"s1\",\"pad\":\"{}\"}}",
                        "x".repeat(20 * 1024 * 1024)
                    );
                    let inputs = [
                        "not json".to_owned(),
                        String::new(),
                        "{\"hook_event_name\":\"Stop\"}".to_owned(),
                        "{\"session_id\": 7}".to_owned(),
                        "[1,2]".to_owned(),
                        oversized,
                    ];
                    for (event, budget) in EVENTS {
                        for doc in &inputs {
                            let (out, took) = run_hook(env.home(), event, doc, me());
                            let what = format!("{event} with {doc:.40}");
                            assert_silent_within(&out, took, budget, &what);
                        }
                    }
                    // Nothing happened to the session.
                    assert_eq!(leases_of(&env, "s1").len(), 1);
                    let fresh = TempDir::new().unwrap();
                    let (out, took) = run_hook(fresh.path(), "stop", "not json", me());
                    assert_silent_within(&out, took, BUDGET, "fresh home");
                    assert!(!paths_in(fresh.path()).socket.exists());
                    assert_no_daemon(fresh.path());
                }
            }

            #[test]
            fn resolves_the_session_by_claude_pid_first_then_session_id() {
                let env = Env::new();
                let mut p = Proxy::spawn(&env, "s1");
                board_held_by(&mut p);
                // By CLAUDE_PID: the hook's own id is another one (`/clear`).
                let (out, _) = run_hook(env.home(), "stop", &typical("stop", "cleared"), me());
                assert_eq!(stdout_json(&out)["decision"], json!("block"), "{out:?}");
                // Without CLAUDE_PID: by session_id.
                let (out, _) = run_hook(env.home(), "stop", &typical("stop", "s1"), None);
                assert_eq!(stdout_json(&out)["decision"], json!("block"), "{out:?}");
                let (out, _) = run_hook(env.home(), "stop", &typical("stop", "cleared"), None);
                assert!(out.stdout.is_empty(), "{out:?}");
            }
        }

        mod session_start {
            use super::*;

            mod on_source_resume {
                use super::*;

                #[test]
                fn aliases_the_proxys_start_up_id_to_the_resumed_id() {
                    let env = Env::new();
                    let mut old = Proxy::spawn(&env, "s1");
                    let lease = board_held_by(&mut old);
                    old.kill();
                    wait_disconnected(&env, "s1");
                    let mut new = Proxy::spawn(&env, "s2");
                    new.tool("resource_list", &json!({}));
                    wait_connected(&env, "s2");
                    let doc = input("session-start", "s1", &json!({"source": "resume"}));
                    let (out, took) = run_hook(env.home(), "session-start", &doc, me());
                    assert_silent_within(&out, took, BUDGET, "session-start");
                    assert_eq!(leases_of(&env, "s2"), vec![lease]);
                }

                #[test]
                fn aliases_once_the_proxy_connects_shortly_after_the_hook_starts() {
                    let env = Env::new();
                    let mut old = Proxy::spawn(&env, "s1");
                    let lease = board_held_by(&mut old);
                    old.kill();
                    wait_disconnected(&env, "s1");
                    let home = env.home().to_owned();
                    let doc = input("session-start", "s1", &json!({"source": "resume"}));
                    let hook = thread::spawn(move || run_hook(&home, "session-start", &doc, me()));
                    thread::sleep(Duration::from_millis(300));
                    let mut new = Proxy::spawn(&env, "s2");
                    new.tool("resource_list", &json!({}));
                    let (out, took) = hook.join().unwrap();
                    assert_silent_within(&out, took, BUDGET, "session-start");
                    assert_eq!(leases_of(&env, "s2"), vec![lease]);
                }

                #[test]
                fn aliases_once_the_proxy_starts_the_daemon_shortly_after_the_hook_starts() {
                    let env = Env::new();
                    let mut old = Proxy::spawn(&env, "s1");
                    let lease = board_held_by(&mut old);
                    old.kill();
                    wait_disconnected(&env, "s1");
                    env.cmd().args(["daemon", "stop"]).assert().success();
                    let home = env.home().to_owned();
                    let doc = input("session-start", "s1", &json!({"source": "resume"}));
                    let hook = thread::spawn(move || run_hook(&home, "session-start", &doc, me()));
                    thread::sleep(Duration::from_millis(300));
                    let mut new = Proxy::spawn(&env, "s2");
                    new.tool("resource_list", &json!({}));
                    let (out, took) = hook.join().unwrap();
                    assert_silent_within(&out, took, BUDGET, "session-start");
                    assert_eq!(leases_of(&env, "s2"), vec![lease]);
                }

                #[test]
                fn a_later_hook_aliases_when_the_session_start_hook_gave_up() {
                    let env = Env::new();
                    let mut old = Proxy::spawn(&env, "s1");
                    let lease = board_held_by(&mut old);
                    old.kill();
                    wait_disconnected(&env, "s1");
                    // No proxy yet: the session-start hook gives up.
                    let doc = input("session-start", "s1", &json!({"source": "resume"}));
                    let (out, took) = run_hook(env.home(), "session-start", &doc, me());
                    assert_silent_within(&out, took, BUDGET, "session-start");
                    assert_eq!(leases_of(&env, "s1"), vec![lease.clone()]);
                    let mut new = Proxy::spawn(&env, "s2");
                    new.tool("resource_list", &json!({}));
                    wait_connected(&env, "s2");
                    let (out, _) = run_hook(
                        env.home(),
                        "pre-tool-use",
                        &typical("pre-tool-use", "s1"),
                        me(),
                    );
                    assert!(out.status.success(), "{out:?}");
                    assert_eq!(leases_of(&env, "s2"), vec![lease]);
                }
            }

            mod on_source_resume_right_after_the_old_process_died {
                use super::*;

                #[test]
                fn aliases_once_the_old_proxys_end_is_noticed() {
                    let env = Env::new();
                    // The old Claude Code process: another PID.
                    let mut old = Proxy::spawn_under_shell(&env, "s1");
                    let lease = board_held_by(&mut old);
                    let mut new = Proxy::spawn(&env, "s2");
                    new.tool("resource_list", &json!({}));
                    wait_connected(&env, "s2");
                    // No wait for the old stream to be noticed closed.
                    old.kill();
                    let doc = input("session-start", "s1", &json!({"source": "resume"}));
                    let (out, took) = run_hook(env.home(), "session-start", &doc, me());
                    assert_silent_within(&out, took, BUDGET, "session-start");
                    assert_eq!(leases_of(&env, "s2"), vec![lease]);
                    let refused = env.ok(&["log", "--action", "session_alias_refused"]);
                    assert_eq!(refused, json!([]));
                }
            }

            mod on_any_other_source {
                use super::*;

                #[test]
                fn outputs_nothing_and_changes_nothing() {
                    let env = Env::new();
                    let mut old = Proxy::spawn(&env, "s1");
                    let lease = board_held_by(&mut old);
                    old.kill();
                    wait_disconnected(&env, "s1");
                    let mut new = Proxy::spawn(&env, "s2");
                    new.tool("resource_list", &json!({}));
                    wait_connected(&env, "s2");
                    for source in ["startup", "clear", "compact", "fork"] {
                        let doc = input("session-start", "s1", &json!({"source": source}));
                        let (out, took) = run_hook(env.home(), "session-start", &doc, me());
                        assert_silent_within(&out, took, BUDGET, source);
                    }
                    assert_eq!(leases_of(&env, "s1"), vec![lease]);
                    assert_eq!(leases_of(&env, "s2"), Vec::<String>::new());
                }
            }
        }

        mod stop {
            use super::*;

            mod on_a_live_lease_or_queued_request_no_open_lease_wait_stop_hook_active_false {
                use super::*;

                #[test]
                fn outputs_a_block_decision_whose_reason_names_lease_wait() {
                    let env = Env::new();
                    let mut p = Proxy::spawn(&env, "s1");
                    board_held_by(&mut p);
                    let (out, took) = run_hook(env.home(), "stop", &typical("stop", "s1"), me());
                    assert!(took < BUDGET, "took {took:?}");
                    let decision = stdout_json(&out);
                    assert_eq!(decision["decision"], json!("block"), "{decision}");
                    let reason = decision["reason"].as_str().unwrap();
                    assert!(reason.contains("lease_wait"), "{reason}");
                    // A queued request alone also counts.
                    let mut q = Proxy::spawn(&env, "s2");
                    let queued = q.tool(
                        "lease_acquire",
                        &json!({"target": {"resource": "board"}, "for": "1h", "purpose": "next"}),
                    );
                    assert_eq!(queued["outcome"], json!("queued"), "{queued}");
                    let (out, _) = run_hook(env.home(), "stop", &typical("stop", "s2"), None);
                    assert_eq!(stdout_json(&out)["decision"], json!("block"), "{out:?}");
                }
            }

            mod on_an_open_lease_wait {
                use super::*;

                #[test]
                fn outputs_nothing() {
                    let env = Env::new();
                    let mut p = Proxy::spawn(&env, "s1");
                    board_held_by(&mut p);
                    let wait = p.send_tool("lease_wait", &json!({}));
                    let until = Instant::now() + PATIENCE;
                    while brief(&env, "s1", me())["open_waits"].as_u64() < Some(1) {
                        assert!(Instant::now() < until, "the lease_wait never opened");
                        thread::sleep(Duration::from_millis(20));
                    }
                    let (out, took) = run_hook(env.home(), "stop", &typical("stop", "s1"), me());
                    assert_silent_within(&out, took, BUDGET, "stop");
                    assert!(p.seen.iter().all(|m| m["id"] != json!(wait)));
                }
            }

            mod on_stop_hook_active_true {
                use super::*;

                #[test]
                fn outputs_nothing() {
                    let env = Env::new();
                    let mut p = Proxy::spawn(&env, "s1");
                    board_held_by(&mut p);
                    let doc = input("stop", "s1", &json!({"stop_hook_active": true}));
                    let (out, took) = run_hook(env.home(), "stop", &doc, me());
                    assert_silent_within(&out, took, BUDGET, "stop");
                }
            }

            mod on_nothing_held_or_queued {
                use super::*;

                #[test]
                fn outputs_nothing() {
                    let env = Env::new();
                    let mut p = Proxy::spawn(&env, "s1");
                    board(&mut p);
                    let (out, took) = run_hook(env.home(), "stop", &typical("stop", "s1"), me());
                    assert_silent_within(&out, took, BUDGET, "stop");
                }
            }
        }

        mod pre_tool_use {
            use super::*;

            mod on_unshown_pending_notices {
                use super::*;

                #[test]
                fn outputs_them_as_additional_context_and_marks_them_shown() {
                    let env = Env::new();
                    let mut p = Proxy::spawn(&env, "s1");
                    let notice = offered_to(&env, &mut p, "s1");
                    let doc = typical("pre-tool-use", "s1");
                    let (out, took) = run_hook(env.home(), "pre-tool-use", &doc, me());
                    assert!(took < BUDGET, "took {took:?}");
                    let shown = stdout_json(&out);
                    let specific = &shown["hookSpecificOutput"];
                    assert_eq!(specific["hookEventName"], json!("PreToolUse"), "{shown}");
                    let context = specific["additionalContext"].as_str().unwrap();
                    assert!(context.contains(&notice), "{context}");
                    assert!(context.contains("lease_wait"), "{context}");
                    assert!(context.chars().count() <= 10_000);
                    // Marked shown: the next hook does not repeat it.
                    let (again, took) = run_hook(env.home(), "pre-tool-use", &doc, me());
                    assert_silent_within(&again, took, BUDGET, "second pre-tool-use");
                }

                #[test]
                fn they_remain_pending_for_lease_wait() {
                    let env = Env::new();
                    let mut p = Proxy::spawn(&env, "s1");
                    let notice = offered_to(&env, &mut p, "s1");
                    let doc = typical("pre-tool-use", "s1");
                    let (out, _) = run_hook(env.home(), "pre-tool-use", &doc, me());
                    assert!(!out.stdout.is_empty(), "{out:?}");
                    assert_eq!(pending_notice_ids(&env, "s1"), vec![notice.clone()]);
                    let batch = p.tool("lease_wait", &json!({}));
                    assert!(batch.to_string().contains(&notice), "{batch}");
                }
            }

            mod on_a_subagents_tool_call {
                use super::*;

                #[test]
                fn outputs_nothing_and_marks_nothing() {
                    let env = Env::new();
                    let mut p = Proxy::spawn(&env, "s1");
                    let notice = offered_to(&env, &mut p, "s1");
                    let doc = input(
                        "pre-tool-use",
                        "s1",
                        &json!({"tool_name": "Bash", "tool_input": {"command": "ls"},
                                "tool_use_id": "toolu_1", "agent_id": "a1",
                                "agent_type": "general-purpose"}),
                    );
                    let (out, took) = run_hook(env.home(), "pre-tool-use", &doc, me());
                    assert_silent_within(&out, took, BUDGET, "subagent pre-tool-use");
                    // The main agent's next tool call still shows it.
                    let doc = typical("pre-tool-use", "s1");
                    let (out, _) = run_hook(env.home(), "pre-tool-use", &doc, me());
                    let context = stdout_json(&out)["hookSpecificOutput"]["additionalContext"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    assert!(context.contains(&notice), "{context}");
                }
            }

            mod on_none {
                use super::*;

                #[test]
                fn outputs_nothing() {
                    let env = Env::new();
                    let mut p = Proxy::spawn(&env, "s1");
                    board_held_by(&mut p);
                    let doc = typical("pre-tool-use", "s1");
                    let (out, took) = run_hook(env.home(), "pre-tool-use", &doc, me());
                    assert_silent_within(&out, took, BUDGET, "pre-tool-use");
                }
            }
        }

        mod session_end {
            use super::*;

            mod on_reason_clear_or_resume_with_the_proxy_still_connected {
                use super::*;

                #[test]
                fn changes_nothing() {
                    let env = Env::new();
                    let mut p = Proxy::spawn(&env, "s1");
                    let lease = board_held_by(&mut p);
                    for reason in ["clear", "resume"] {
                        let doc = input("session-end", "s1", &json!({"reason": reason}));
                        let (out, took) = run_hook(env.home(), "session-end", &doc, me());
                        assert_silent_within(&out, took, END_BUDGET, reason);
                    }
                    assert_eq!(leases_of(&env, "s1"), vec![lease]);
                }
            }

            mod on_reason_clear_or_resume_with_the_proxy_gone {
                use super::*;

                #[test]
                fn ends_the_sessions_liveness_now() {
                    for reason in ["clear", "resume"] {
                        let env = Env::new();
                        let mut p = Proxy::spawn(&env, "s1");
                        board_held_by(&mut p);
                        p.kill();
                        wait_disconnected(&env, "s1");
                        let doc = input("session-end", "s1", &json!({"reason": reason}));
                        let (out, took) = run_hook(env.home(), "session-end", &doc, me());
                        assert_silent_within(&out, took, END_BUDGET, reason);
                        assert_eq!(leases_of(&env, "s1"), Vec::<String>::new(), "{reason}");
                    }
                }
            }

            mod on_any_other_reason {
                use super::*;

                #[test]
                fn ends_the_sessions_liveness_now() {
                    let env = Env::new();
                    let mut p = Proxy::spawn(&env, "s1");
                    board_held_by(&mut p);
                    let doc = input("session-end", "s1", &json!({"reason": "prompt_input_exit"}));
                    let (out, took) = run_hook(env.home(), "session-end", &doc, me());
                    assert_silent_within(&out, took, END_BUDGET, "session-end");
                    assert_eq!(leases_of(&env, "s1"), Vec::<String>::new());
                }
            }
        }
    }
}
