//! `clerk mcp`: the stdio ↔ `/mcp` relay. The real binary is spawned with
//! `CLAUDE_CODE_SESSION_ID` set (or unset) and spoken to over its stdio in
//! 2026-07-28 framing, against either a fake daemon that records what it is
//! sent, or the real daemon (Review Focus 1: a proxy killed with `SIGKILL`).

mod common;

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use leclerk::model::{Caller, HolderRef};
use serde_json::{Value, json};
use tempfile::TempDir;
use ulid::Ulid;

use common::{Env, PATIENCE, SESSION_VAR};

// ---------------------------------------------------------------------------
// The proxy under test
// ---------------------------------------------------------------------------

/// A running `clerk mcp`. Dropping it kills it.
struct Proxy {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<Result<Value, String>>,
    /// Messages read but not yet asked for.
    seen: Vec<Value>,
    next_id: u64,
}

impl Proxy {
    /// Start `clerk mcp` with `CLERK_HOME=home`, in `cwd`, as `session`
    /// (unset when `None`).
    fn spawn(home: &Path, cwd: &Path, session: Option<&str>) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let stderr = File::create(home.join(format!("proxy-{n}.err"))).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_clerk"));
        cmd.arg("mcp")
            .env("CLERK_HOME", home)
            .env_remove(SESSION_VAR)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr);
        if let Some(s) = session {
            cmd.env(SESSION_VAR, s);
        }
        let mut child = cmd.spawn().unwrap();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let parsed = serde_json::from_str::<Value>(&line).map_err(|_| line);
                if tx.send(parsed).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
            seen: Vec::new(),
            next_id: 1,
        }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn send_line(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
    }

    /// Send request `method` in 2026-07-28 framing; its id.
    fn send(&mut self, method: &str, mut params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        params["_meta"] = json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientInfo": {"name": "proxy-test", "version": "0"},
            "io.modelcontextprotocol/clientCapabilities": {},
        });
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.send_line(&body.to_string());
        id
    }

    /// The next message from stdout; panics on a line that is not JSON.
    fn next_message(&mut self, until: Instant) -> Option<Value> {
        let left = until.saturating_duration_since(Instant::now());
        match self.lines.recv_timeout(left) {
            Ok(Ok(v)) => Some(v),
            Ok(Err(line)) => panic!("stdout carried a line that is not JSON: {line:?}"),
            Err(_) => None,
        }
    }

    /// Every message up to and including the response to `id`, in order
    /// (earlier responses to other ids are kept for later).
    fn exchange_until(&mut self, id: u64, within: Duration) -> Vec<Value> {
        let until = Instant::now() + within;
        let mut got = Vec::new();
        if let Some(i) = self.seen.iter().position(|m| m["id"] == json!(id)) {
            got.push(self.seen.remove(i));
            return got;
        }
        loop {
            let message = self
                .next_message(until)
                .unwrap_or_else(|| panic!("no response to {id}; got {got:?}"));
            let done = message["id"] == json!(id);
            if message.get("id").is_some() && !done {
                self.seen.push(message);
                continue;
            }
            got.push(message);
            if done {
                return got;
            }
        }
    }

    /// The response to `id`.
    fn response(&mut self, id: u64) -> Value {
        self.exchange_until(id, PATIENCE).pop().unwrap()
    }

    /// Send `method` and wait for its response.
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params);
        self.response(id)
    }

    /// `tools/call` that must succeed; its structured content.
    fn tool(&mut self, name: &str, args: &Value) -> Value {
        let reply = self.request("tools/call", json!({"name": name, "arguments": args}));
        let result = &reply["result"];
        assert!(result.is_object(), "{reply}");
        assert_ne!(result["isError"], json!(true), "{reply}");
        result["structuredContent"].clone()
    }

    fn close_stdin(&mut self) {
        self.stdin = None;
    }

    fn wait_exit(&mut self) -> ExitStatus {
        let until = Instant::now() + PATIENCE;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < until, "the proxy did not exit");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// SIGKILL and reap.
    fn kill(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Percent-encode everything but unreserved characters and `/`.
fn percent_encode(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut out = String::new();
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

fn header<'a>(h: &'a HeaderMap, name: &str) -> &'a str {
    h.get(name)
        .unwrap_or_else(|| panic!("no {name} header in {h:?}"))
        .to_str()
        .unwrap()
}

// ---------------------------------------------------------------------------
// A fake daemon that records what the proxy sends
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Seen {
    /// Headers of each `GET /v1/session/live`.
    live: Vec<HeaderMap>,
    /// Headers and body of each `POST /mcp`.
    mcp: Vec<(HeaderMap, Value)>,
}

/// A fake daemon on `CLERK_HOME/run/clerk.sock` of a private home.
struct Fake {
    home: TempDir,
    seen: Arc<Mutex<Seen>>,
    /// Liveness streams open now.
    open: Arc<AtomicUsize>,
}

/// Counts an open liveness stream while alive.
struct OpenStream(Arc<AtomicUsize>);

impl Drop for OpenStream {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Fake {
    /// Start serving on the current tokio runtime.
    fn start() -> Self {
        let home = TempDir::new().unwrap();
        let run = home.path().join("run");
        fs::create_dir_all(&run).unwrap();
        let listener = tokio::net::UnixListener::bind(run.join("clerk.sock")).unwrap();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let open = Arc::new(AtomicUsize::new(0));
        let (live_seen, mcp_seen, live_open) = (seen.clone(), seen.clone(), open.clone());
        let app = Router::new()
            .route(
                "/v1/version",
                get(|| async { axum::Json(json!({"version": leclerk::VERSION, "pid": 1})) }),
            )
            .route(
                "/v1/session/live",
                get(move |headers: HeaderMap| {
                    let seen = live_seen.clone();
                    let open = live_open.clone();
                    async move {
                        seen.lock().unwrap().live.push(headers);
                        open.fetch_add(1, Ordering::SeqCst);
                        let guard = OpenStream(open);
                        let (mut tx, body) = http_body_util::Channel::<Bytes>::new(1);
                        tokio::spawn(async move {
                            let _guard = guard;
                            while tx.send_data(Bytes::from_static(b"\n")).await.is_ok() {
                                tokio::time::sleep(Duration::from_millis(100)).await;
                            }
                        });
                        Response::new(Body::new(body))
                    }
                }),
            )
            .route(
                "/mcp",
                post(move |headers: HeaderMap, body: Bytes| {
                    let seen = mcp_seen.clone();
                    async move {
                        let message: Value = serde_json::from_slice(&body).unwrap();
                        seen.lock().unwrap().mcp.push((headers, message.clone()));
                        fake_mcp(&message)
                    }
                }),
            );
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self { home, seen, open }
    }

    fn home(&self) -> &Path {
        self.home.path()
    }

    /// A project directory below the home, with a space in its name.
    fn project(&self) -> PathBuf {
        let dir = self.home().join("my proj");
        fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn proxy(&self, session: Option<&str>) -> Proxy {
        Proxy::spawn(self.home(), &self.project(), session)
    }

    fn live(&self) -> Vec<HeaderMap> {
        self.seen.lock().unwrap().live.clone()
    }

    /// The headers of the `/mcp` request whose JSON-RPC id is `id`.
    fn mcp_headers(&self, id: u64) -> HeaderMap {
        self.seen
            .lock()
            .unwrap()
            .mcp
            .iter()
            .find(|(_, m)| m["id"] == json!(id))
            .map_or_else(
                || panic!("no /mcp request with id {id}"),
                |(h, _)| h.clone(),
            )
    }

    fn open_streams(&self) -> usize {
        self.open.load(Ordering::SeqCst)
    }
}

/// The fake `/mcp`: `json/echo` answers with JSON, `sse/echo` with an SSE
/// stream (a progress notification, then the response), notifications get
/// 202.
fn fake_mcp(message: &Value) -> Response {
    let Some(id) = message.get("id") else {
        return StatusCode::ACCEPTED.into_response();
    };
    let method = message["method"].as_str().unwrap_or_default();
    let response = json!({"jsonrpc": "2.0", "id": id, "result": {"method": method}});
    if method == "sse/echo" {
        let note = json!({"jsonrpc": "2.0", "method": "notifications/progress",
                          "params": {"progressToken": 1, "progress": 1}});
        let body = format!("event: message\ndata: {note}\n\nevent: message\ndata: {response}\n\n");
        return ([("content-type", "text/event-stream")], body).into_response();
    }
    axum::Json(response).into_response()
}

/// Wait until `f` holds.
async fn eventually(mut f: impl FnMut() -> bool, what: &str) {
    let until = Instant::now() + PATIENCE;
    while !f() {
        assert!(Instant::now() < until, "timed out waiting until {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---------------------------------------------------------------------------
// The real daemon
// ---------------------------------------------------------------------------

/// A private clerk home whose daemon uses a `reconnect_grace` of `grace`
/// seconds.
fn real(grace: u64) -> Env {
    let env = Env::new();
    let config = env.paths().config;
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, format!("reconnect_grace = {grace}\n")).unwrap();
    env
}

fn proxy_in(env: &Env, session: &str) -> Proxy {
    Proxy::spawn(env.home(), &env.dir("p"), Some(session))
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

/// Create user-scoped `board` with one unit and acquire it through `p`;
/// the lease id.
fn board_held_by(p: &mut Proxy) -> String {
    p.tool(
        "resource_create",
        &json!({"id": "board", "type": "generic", "scope": "user"}),
    );
    p.tool(
        "unit_add",
        &json!({"resource": "board", "id": "u1",
               "settings": {"type": "generic", "identity_key": "k"}}),
    );
    let grant = p.tool(
        "lease_acquire",
        &json!({"target": {"resource": "board"}, "for": "1h", "purpose": "flash"}),
    );
    assert_eq!(grant["outcome"], json!("granted"), "{grant}");
    grant["lease_id"].as_str().unwrap().to_owned()
}

/// User `w` queues for `board`.
fn waiter_queues(env: &Env) {
    let queued = env
        .rpc(
            "acquire",
            &user("w"),
            json!({"target": {"resource": "board"}, "duration_ms": 60_000, "purpose": "next"}),
        )
        .unwrap();
    assert_eq!(queued["outcome"], json!("queued"), "{queued}");
}

/// The states of `caller`'s live leases.
fn lease_states(env: &Env, caller: &Caller) -> Vec<String> {
    let mine = env.rpc("list_my_leases", caller, json!({})).unwrap();
    mine["leases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["state"].as_str().unwrap().to_owned())
        .collect()
}

fn wait_for_states(env: &Env, caller: &Caller, want: &[&str]) {
    let until = Instant::now() + PATIENCE;
    loop {
        let got = lease_states(env, caller);
        if got == want {
            return;
        }
        assert!(
            Instant::now() < until,
            "leases stayed {got:?}, wanted {want:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ---------------------------------------------------------------------------
// proxy (clerk mcp)
// ---------------------------------------------------------------------------

mod proxy {
    use super::*;

    mod on_start {
        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn opens_the_liveness_stream_with_the_identity_headers() {
            let fake = Fake::start();
            let p = fake.proxy(Some("sess-1"));
            eventually(|| !fake.live().is_empty(), "the stream opens").await;
            let live = &fake.live()[0];
            assert_eq!(header(live, "x-clerk-session"), "sess-1");
            Ulid::from_string(header(live, "x-clerk-connection")).unwrap();
            assert_eq!(
                header(live, "x-clerk-project"),
                percent_encode(&fake.project())
            );
            assert_eq!(
                header(live, "x-clerk-claude-pid"),
                std::process::id().to_string()
            );
            assert_eq!(fake.open_streams(), 1);
            drop(p);
        }

        #[test]
        fn relays_stdin_json_rpc_to_mcp_and_responses_to_stdout() {
            let env = real(60);
            let mut p = proxy_in(&env, "s1");
            let discover = p.request("server/discover", json!({}));
            assert!(discover["result"].is_object(), "{discover}");
            let listed = p.tool("resource_list", &json!({}));
            assert!(listed.is_object() || listed.is_array(), "{listed}");
        }
    }

    mod on_each_message {
        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn sets_mcp_protocol_version_mcp_method_and_mcp_name_plus_the_x_clerk_headers() {
            let fake = Fake::start();
            let mut p = fake.proxy(Some("sess-1"));
            let id = p.send(
                "tools/call",
                json!({"name": "resource_list", "arguments": {}}),
            );
            let reply = tokio::task::spawn_blocking(move || {
                let r = p.response(id);
                (p, r)
            })
            .await
            .unwrap()
            .1;
            assert_eq!(reply["result"]["method"], json!("tools/call"));
            let h = fake.mcp_headers(id);
            assert_eq!(header(&h, "mcp-protocol-version"), "2026-07-28");
            assert_eq!(header(&h, "mcp-method"), "tools/call");
            assert_eq!(header(&h, "mcp-name"), "resource_list");
            assert_eq!(header(&h, "x-clerk-session"), "sess-1");
            assert_eq!(
                header(&h, "x-clerk-connection"),
                header(&fake.live()[0], "x-clerk-connection")
            );
            assert_eq!(
                header(&h, "x-clerk-project"),
                percent_encode(&fake.project())
            );
            assert_eq!(
                header(&h, "x-clerk-claude-pid"),
                std::process::id().to_string()
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn relays_json_and_sse_responses() {
            let fake = Fake::start();
            let mut p = fake.proxy(Some("sess-1"));
            let (json_reply, sse_messages) = tokio::task::spawn_blocking(move || {
                let json_reply = p.request("json/echo", json!({}));
                let id = p.send("sse/echo", json!({}));
                let sse = p.exchange_until(id, PATIENCE);
                (json_reply, sse)
            })
            .await
            .unwrap();
            assert_eq!(json_reply["result"]["method"], json!("json/echo"));
            assert_eq!(sse_messages.len(), 2, "{sse_messages:?}");
            assert_eq!(sse_messages[0]["method"], json!("notifications/progress"));
            assert_eq!(sse_messages[1]["result"]["method"], json!("sse/echo"));
        }

        #[test]
        fn sends_requests_concurrently_a_slow_lease_wait_never_blocks_other_calls() {
            let env = real(60);
            let mut p = proxy_in(&env, "s1");
            let wait = p.send("tools/call", json!({"name": "lease_wait", "arguments": {}}));
            // Give the wait time to reach the daemon and block there.
            std::thread::sleep(Duration::from_millis(300));
            let list = p.send(
                "tools/call",
                json!({"name": "resource_list", "arguments": {}}),
            );
            let got = p.exchange_until(list, Duration::from_secs(10));
            assert!(got.iter().all(|m| m["id"] != json!(wait)), "{got:?}");
            assert!(got.last().unwrap()["result"].is_object(), "{got:?}");
        }
    }

    mod on_claude_code_session_id_unset {
        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn uses_conn_ulid_as_the_session_id() {
            let fake = Fake::start();
            let _p = fake.proxy(None);
            eventually(|| !fake.live().is_empty(), "the stream opens").await;
            let session = header(&fake.live()[0], "x-clerk-session").to_owned();
            let ulid = session
                .strip_prefix("conn:")
                .unwrap_or_else(|| panic!("{session}"));
            Ulid::from_string(ulid).unwrap();
        }
    }

    mod on_stdin_eof {
        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn closes_the_liveness_stream_and_exits_0() {
            let fake = Fake::start();
            let mut p = fake.proxy(Some("sess-1"));
            eventually(|| fake.open_streams() == 1, "the stream opens").await;
            p.close_stdin();
            let status = tokio::task::spawn_blocking(move || p.wait_exit())
                .await
                .unwrap();
            assert_eq!(status.code(), Some(0));
            eventually(|| fake.open_streams() == 0, "the stream closes").await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn writes_the_answer_to_a_request_sent_just_before() {
            let fake = Fake::start();
            let mut p = fake.proxy(Some("sess-1"));
            eventually(|| fake.open_streams() == 1, "the stream opens").await;
            let reply = tokio::task::spawn_blocking(move || {
                let id = p.send("json/echo", json!({}));
                p.close_stdin();
                assert_eq!(p.wait_exit().code(), Some(0));
                p.response(id)
            })
            .await
            .unwrap();
            assert_eq!(reply["result"]["method"], json!("json/echo"));
        }
    }

    mod on_the_daemon_restarting {
        use super::*;

        #[test]
        fn reconnects_the_liveness_stream_and_keeps_relaying() {
            let env = real(3);
            let mut p = proxy_in(&env, "s1");
            board_held_by(&mut p);
            let restarted = env.cmd().args(["daemon", "restart"]).output().unwrap();
            assert!(restarted.status.success(), "{restarted:?}");
            // Past the grace from boot: the reconnected stream kept the lease.
            std::thread::sleep(Duration::from_secs(5));
            assert_eq!(lease_states(&env, &agent("s1")), vec!["held"]);
            let status = p.tool("lease_status", &json!({}));
            assert!(status.is_object(), "{status}");
        }
    }

    mod on_the_daemon_not_running {
        use super::*;

        #[test]
        fn starts_it() {
            let env = real(60);
            let mut p = proxy_in(&env, "s1");
            let discover = p.request("server/discover", json!({}));
            assert!(discover["result"].is_object(), "{discover}");
            let status = env.ok(&["daemon", "status"]);
            assert_eq!(status["running"], json!(true), "{status}");
        }
    }

    mod on_the_proxy_killed {
        use super::*;

        #[test]
        fn after_the_grace_its_leases_end_and_the_waiter_gets_the_unit() {
            let env = real(2);
            let mut p = proxy_in(&env, "s1");
            board_held_by(&mut p);
            waiter_queues(&env);
            p.kill();
            wait_for_states(&env, &user("w"), &["offered"]);
            assert_eq!(lease_states(&env, &agent("s1")), Vec::<String>::new());
        }

        #[test]
        fn a_new_proxy_of_the_session_inside_the_grace_keeps_them() {
            let env = real(4);
            let mut p = proxy_in(&env, "s1");
            board_held_by(&mut p);
            waiter_queues(&env);
            p.kill();
            let mut again = proxy_in(&env, "s1");
            again.request("server/discover", json!({}));
            std::thread::sleep(Duration::from_secs(6));
            assert_eq!(lease_states(&env, &agent("s1")), vec!["held"]);
            assert_eq!(lease_states(&env, &user("w")), Vec::<String>::new());
            assert_ne!(again.pid(), 0);
        }
    }

    mod while_alive {
        use super::*;

        #[test]
        fn its_leases_outlast_the_grace() {
            let env = real(1);
            let mut p = proxy_in(&env, "s1");
            board_held_by(&mut p);
            std::thread::sleep(Duration::from_secs(3));
            assert_eq!(lease_states(&env, &agent("s1")), vec!["held"]);
        }
    }
}
