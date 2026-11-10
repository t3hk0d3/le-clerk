//! The daemon's MCP endpoint (`/mcp`), driven over its Unix socket by rmcp's
//! own client (2025-11-25, through `initialize`) and by a minimal 2026-07-28
//! client (`server/discover`, per-request `_meta` and headers), both sending
//! the `X-Clerk-*` identity headers directly (no proxy).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use leclerk::clock::ManualClock;
use leclerk::config::Config;
use leclerk::daemon::server as http_server;
use leclerk::engine::{Command, Engine, EngineHandle};
use leclerk::health::HealthResult;
use leclerk::model::{Caller, HealthSpec, HolderRef, ResourceId, UnitId, UnitSettings};
use leclerk::scheduler::HealthProbe;
use leclerk::store::Store;
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ClientCapabilities, ClientConfig, Implementation};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::{Value, json};
use tempfile::TempDir;

const T0: i64 = 1_000_000;

/// The 21 tools of the phase 3 Global Constraints.
const TOOL_NAMES: &[&str] = &[
    "type_list",
    "resource_create",
    "resource_update",
    "resource_delete",
    "resource_list",
    "resource_get",
    "unit_add",
    "unit_update",
    "unit_remove",
    "unit_retire",
    "lease_acquire",
    "lease_status",
    "lease_extend",
    "lease_release",
    "lease_confirm",
    "lease_set_interruptible",
    "lease_yield_refuse",
    "lease_reclaim",
    "lease_cancel",
    "lease_claim",
    "lease_wait",
];

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Every unit is healthy.
struct Healthy;

impl HealthProbe for Healthy {
    fn check(
        &self,
        _spec: &HealthSpec,
        _resource: &ResourceId,
        _unit: &UnitId,
        _settings: &UnitSettings,
    ) -> HealthResult {
        HealthResult::Healthy
    }
}

/// An engine served by the daemon router on a private Unix socket, plus two
/// project directories.
struct Daemon {
    dir: TempDir,
    engine: EngineHandle,
}

impl Daemon {
    fn start() -> Self {
        Self::start_with(Config::default())
    }

    fn start_with(config: Config) -> Self {
        let dir = TempDir::new().unwrap();
        let engine = Engine::spawn(
            Store::open_in_memory().unwrap(),
            Arc::new(ManualClock::new(T0)),
            config,
            Arc::new(Healthy),
        )
        .unwrap();
        let listener = tokio::net::UnixListener::bind(dir.path().join("s")).unwrap();
        let app = http_server::router(engine.clone());
        tokio::spawn(async move { axum::serve(listener, app).await });
        for name in ["a", "b"] {
            std::fs::create_dir(dir.path().join(name)).unwrap();
        }
        Self { dir, engine }
    }

    fn socket(&self) -> PathBuf {
        self.dir.path().join("s")
    }

    /// Project directory `name` ("a" or "b"), canonical.
    fn project(&self, name: &str) -> PathBuf {
        self.dir.path().join(name).canonicalize().unwrap()
    }

    /// A 2026-07-28 client acting as `session` in project `project`.
    fn session(&self, session: &str, project: &str) -> Raw {
        Raw::new(self, Some(session), Some(&self.project(project)))
    }

    /// A 2026-07-28 client acting as `session` with no project header.
    fn session_without_project(&self, session: &str) -> Raw {
        Raw::new(self, Some(session), None)
    }

    /// A 2026-07-28 client that sends no `X-Clerk-Session`.
    fn anonymous(&self, project: &str) -> Raw {
        Raw::new(self, None, Some(&self.project(project)))
    }

    /// rmcp's own client (2025-11-25 through `initialize`).
    async fn rmcp(
        &self,
        session: &str,
        project: &str,
    ) -> rmcp::service::RunningService<rmcp::RoleClient, ClientConfig> {
        let mut headers = HashMap::new();
        for (k, v) in identity_headers(Some(session), Some(&self.project(project))) {
            headers.insert(
                axum::http::HeaderName::from_static(k),
                axum::http::HeaderValue::from_str(&v).unwrap(),
            );
        }
        let config = StreamableHttpClientTransportConfig::with_uri("http://localhost/mcp")
            .custom_headers(headers);
        let transport = StreamableHttpClientTransport::from_unix_socket_with_config(
            self.socket().to_str().unwrap(),
            config,
        );
        ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("mcp-test", "0"),
        )
        .serve(transport)
        .await
        .unwrap()
    }

    /// Engine call as the user (all scopes), for checking state.
    async fn admin(&self, method: &str, args: Value) -> Value {
        let caller = Caller {
            holder: HolderRef::User { tag: "test".into() },
            project: None,
            override_reason: None,
        };
        self.engine
            .call(Command::parse(method, caller, args).unwrap())
            .await
            .unwrap()
    }

    /// Lease `lease` of agent session `session` (in project "a"), as the
    /// engine reports it.
    async fn lease_of(&self, session: &str, lease: &Value) -> Value {
        self.as_session(session, "get_lease", json!({ "lease_id": lease }))
            .await
            .unwrap()
    }

    /// Engine call `method` as agent session `session` in project "a",
    /// straight to the engine (no MCP).
    async fn as_session(
        &self,
        session: &str,
        method: &str,
        args: Value,
    ) -> Result<Value, leclerk::engine::ApiError> {
        let caller = Caller {
            holder: HolderRef::AgentSession {
                session_id: session.into(),
            },
            project: Some(leclerk::model::ProjectKey(self.project("a"))),
            override_reason: None,
        };
        self.engine
            .call(Command::parse(method, caller, args).unwrap())
            .await
    }

    /// Every resource, in every scope.
    async fn all_resources(&self) -> Vec<Value> {
        let v = self.admin("list_resources", json!({"view": "all"})).await;
        v.as_array().unwrap().clone()
    }

    /// The actor of the latest audit entry with `action`.
    async fn actor_of(&self, action: &str) -> Value {
        let v = self.admin("list_audit", json!({"action": action})).await;
        v.as_array().unwrap().last().unwrap()["actor"].clone()
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

fn identity_headers(session: Option<&str>, project: Option<&Path>) -> Vec<(&'static str, String)> {
    let mut h = vec![
        ("x-clerk-connection", ulid::Ulid::new().to_string()),
        ("x-clerk-claude-pid", "4242".to_owned()),
    ];
    if let Some(s) = session {
        h.push(("x-clerk-session", s.to_owned()));
    }
    if let Some(p) = project {
        h.push(("x-clerk-project", percent_encode(p)));
    }
    h
}

/// A minimal MCP client speaking 2026-07-28 framing (or 2025-11-25 when
/// told), one HTTP request per message.
struct Raw {
    socket: PathBuf,
    headers: Vec<(&'static str, String)>,
    version: &'static str,
    next_id: AtomicU64,
    /// Send `_meta.progressToken` with every request.
    progress: bool,
    /// Declare the tasks extension in the per-request client capabilities.
    tasks: bool,
}

/// What came back for one request.
struct Reply {
    status: u16,
    /// The JSON-RPC response message.
    message: Value,
}

impl Raw {
    fn new(d: &Daemon, session: Option<&str>, project: Option<&Path>) -> Self {
        Self {
            socket: d.socket(),
            headers: identity_headers(session, project),
            version: "2026-07-28",
            next_id: AtomicU64::new(1),
            progress: false,
            tasks: false,
        }
    }

    /// Ask for progress notifications (a `progressToken` per request).
    fn with_progress(mut self) -> Self {
        self.progress = true;
        self
    }

    /// Declare `io.modelcontextprotocol/tasks` per request.
    fn with_tasks(mut self) -> Self {
        self.tasks = true;
        self
    }

    /// Send `value` as header `name` instead of what the identity says.
    fn with_header(mut self, name: &'static str, value: &str) -> Self {
        self.headers.retain(|(k, _)| *k != name);
        self.headers.push((name, value.to_owned()));
        self
    }

    /// The same identity, speaking 2025-11-25 (no per-request `_meta`).
    fn legacy(mut self) -> Self {
        self.version = "2025-11-25";
        self
    }

    async fn request(&self, method: &str, params: Value) -> Reply {
        let (status, id, messages) = self.exchange(method, params).await;
        let message = messages
            .into_iter()
            .find(|m| m.get("id") == Some(&json!(id)))
            .unwrap_or_else(|| panic!("no response for {id}"));
        Reply { status, message }
    }

    /// Every message of one request's response (notifications first, as
    /// sent), with the HTTP status and the request id.
    async fn exchange(&self, method: &str, mut params: Value) -> (u16, u64, Vec<Value>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if self.version == "2026-07-28" {
            let capabilities = if self.tasks {
                json!({"extensions": {"io.modelcontextprotocol/tasks": {}}})
            } else {
                json!({})
            };
            params["_meta"] = json!({
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientInfo": {"name": "mcp-test", "version": "0"},
                "io.modelcontextprotocol/clientCapabilities": capabilities,
            });
            if self.progress {
                params["_meta"]["progressToken"] = json!(id);
            }
        }
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let stream = tokio::net::UnixStream::connect(&self.socket).await.unwrap();
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap();
        tokio::spawn(conn);
        let mut req = hyper::Request::post("/mcp")
            .header("host", "localhost")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", self.version)
            .header("mcp-method", method);
        if let Some(name) = params
            .get("name")
            .or_else(|| params.get("taskId"))
            .and_then(Value::as_str)
        {
            req = req.header("mcp-name", name);
        }
        for (k, v) in &self.headers {
            req = req.header(*k, v);
        }
        let resp = sender
            .send_request(req.body(Full::new(Bytes::from(body.to_string()))).unwrap())
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let sse = resp
            .headers()
            .get("content-type")
            .and_then(|c| c.to_str().ok())
            .is_some_and(|c| c.starts_with("text/event-stream"));
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let messages = if sse {
            text.split("\n\n")
                .filter_map(|ev| {
                    let data: Vec<&str> = ev
                        .lines()
                        .filter_map(|l| l.strip_prefix("data:"))
                        .map(str::trim_start)
                        .collect();
                    serde_json::from_str::<Value>(&data.join("\n")).ok()
                })
                .collect()
        } else {
            vec![serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {status} {text:?}"))]
        };
        (status, id, messages)
    }

    /// Start `tools/call` and drop the connection after `after`, as a dying
    /// proxy would.
    async fn call_and_disconnect(&self, tool: &str, args: Value, after: Duration) {
        let call = self.exchange("tools/call", json!({"name": tool, "arguments": args}));
        assert!(
            tokio::time::timeout(after, call).await.is_err(),
            "{tool} answered before the disconnect"
        );
    }

    /// `tools/call`; the JSON-RPC `result` (a `CallToolResult`).
    async fn call(&self, tool: &str, args: Value) -> Value {
        let reply = self
            .request("tools/call", json!({"name": tool, "arguments": args}))
            .await;
        reply
            .message
            .get("result")
            .unwrap_or_else(|| panic!("no result: {}", reply.message))
            .clone()
    }

    /// `tools/call` that must succeed; its structured content.
    async fn ok(&self, tool: &str, args: Value) -> Value {
        let result = self.call(tool, args).await;
        assert_ne!(result["isError"], json!(true), "{result}");
        result["structuredContent"].clone()
    }

    /// `tools/call` that must fail as a tool error; the error object.
    async fn err(&self, tool: &str, args: Value) -> Value {
        let result = self.call(tool, args).await;
        assert_eq!(result["isError"], json!(true), "{result}");
        result["structuredContent"]["error"].clone()
    }
}

/// Assert `reply` is an HTTP 400 JSON-RPC `-32600` refusal whose message
/// contains `needle`.
fn assert_refused(reply: &Reply, needle: &str) {
    assert_eq!(reply.status, 400, "{}", reply.message);
    assert!(reply.message.get("result").is_none(), "{}", reply.message);
    let error = &reply.message["error"];
    assert_eq!(error["code"], json!(-32600), "{error}");
    assert!(
        error["message"].as_str().unwrap().contains(needle),
        "{error}"
    );
}

/// `initialize` params of a 2025-11-25 client.
fn initialize_params() -> Value {
    json!({
        "protocolVersion": "2025-11-25",
        "capabilities": {},
        "clientInfo": {"name": "mcp-test", "version": "0"},
    })
}

/// The text block of a tool result.
fn text_of(result: &Value) -> String {
    result["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["type"] == "text")
        .unwrap()["text"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// A generic resource `id` with one unit `u1` of identity key `key`.
async fn seed(s: &Raw, id: &str, key: &str, tags: &[&str]) -> Value {
    s.ok(
        "resource_create",
        json!({"id": id, "type": "generic", "description": "a thing", "tags": tags}),
    )
    .await;
    s.ok(
        "unit_add",
        json!({"resource": id, "id": "u1", "settings": {"type": "generic", "identity_key": key}}),
    )
    .await
}

// ---------------------------------------------------------------------------
// mcp::server
// ---------------------------------------------------------------------------

mod server {
    use super::*;

    mod any_request {
        use super::*;

        mod on_x_clerk_headers {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn acts_as_agent_session_with_the_resolved_project() {
                let d = Daemon::start();
                d.session("s1", "a")
                    .ok("resource_create", json!({"id": "board", "type": "generic"}))
                    .await;
                assert_eq!(
                    d.actor_of("resource_created").await,
                    json!({"kind": "agent_session", "session_id": "s1"})
                );
                let all = d.all_resources().await;
                assert_eq!(
                    all[0]["scope"],
                    json!({"project": d.project("a").to_str().unwrap()})
                );
            }
        }

        mod on_missing_x_clerk_session {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn refuses_the_request_with_an_mcp_error() {
                let d = Daemon::start();
                let reply = d
                    .anonymous("a")
                    .request(
                        "tools/call",
                        json!({"name": "resource_create", "arguments": {"id": "x", "type": "generic"}}),
                    )
                    .await;
                assert_eq!(reply.status, 400);
                assert!(reply.message.get("result").is_none(), "{}", reply.message);
                let error = &reply.message["error"];
                assert_eq!(error["code"], json!(-32600));
                assert!(
                    error["message"]
                        .as_str()
                        .unwrap()
                        .contains("X-Clerk-Session"),
                    "{error}"
                );
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn changes_nothing() {
                let d = Daemon::start();
                d.anonymous("a")
                    .request(
                        "tools/call",
                        json!({"name": "resource_create", "arguments": {"id": "x", "type": "generic"}}),
                    )
                    .await;
                assert_eq!(d.all_resources().await, Vec::<Value>::new());
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn refuses_server_discover() {
                let d = Daemon::start();
                let reply = d.anonymous("a").request("server/discover", json!({})).await;
                assert_refused(&reply, "X-Clerk-Session");
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn refuses_tools_list() {
                let d = Daemon::start();
                let reply = d.anonymous("a").request("tools/list", json!({})).await;
                assert_refused(&reply, "X-Clerk-Session");
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn refuses_initialize() {
                let d = Daemon::start();
                let reply = d
                    .anonymous("a")
                    .legacy()
                    .request("initialize", initialize_params())
                    .await;
                assert_refused(&reply, "X-Clerk-Session");
            }
        }

        mod on_a_relative_x_clerk_project {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn refuses_the_request_with_an_mcp_error() {
                let d = Daemon::start();
                let reply = d
                    .session("s1", "a")
                    .with_header("x-clerk-project", "relative/dir")
                    .request("tools/list", json!({}))
                    .await;
                assert_refused(&reply, "X-Clerk-Project");
            }
        }

        mod on_an_x_clerk_project_that_does_not_resolve {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn refuses_the_request_naming_the_path() {
                let d = Daemon::start();
                let gone = d.dir.path().join("gone");
                let reply = d
                    .session("s1", "a")
                    .with_header("x-clerk-project", &percent_encode(&gone))
                    .request(
                        "tools/call",
                        json!({"name": "resource_create", "arguments": {"id": "x", "type": "generic"}}),
                    )
                    .await;
                assert_refused(&reply, gone.to_str().unwrap());
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn changes_nothing() {
                let d = Daemon::start();
                let gone = d.dir.path().join("gone");
                d.session("s1", "a")
                    .with_header("x-clerk-project", &percent_encode(&gone))
                    .request(
                        "tools/call",
                        json!({"name": "resource_create", "arguments": {"id": "x", "type": "generic"}}),
                    )
                    .await;
                assert_eq!(d.all_resources().await, Vec::<Value>::new());
            }
        }

        mod on_a_project_directory_that_goes_away_after_first_use {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn keeps_resolving_to_the_same_project() {
                let d = Daemon::start();
                let dir = d.dir.path().join("c");
                std::fs::create_dir(&dir).unwrap();
                let s = Raw::new(&d, Some("s1"), Some(&dir));
                let created = s
                    .ok("resource_create", json!({"id": "board", "type": "generic"}))
                    .await;
                std::fs::remove_dir(&dir).unwrap();
                let listed = s.ok("resource_list", json!({})).await;
                assert_eq!(
                    listed["resources"][0]["scope"],
                    created["resource"]["scope"]
                );
            }
        }
    }

    mod server_discover {
        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn answers_with_the_tools_capability_and_the_tasks_extension() {
            let d = Daemon::start();
            let reply = d
                .session("s1", "a")
                .request("server/discover", json!({}))
                .await;
            let result = &reply.message["result"];
            assert!(
                result["supportedVersions"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("2026-07-28")),
                "{result}"
            );
            assert!(result["capabilities"]["tools"].is_object(), "{result}");
            assert!(
                result["capabilities"]["extensions"]["io.modelcontextprotocol/tasks"].is_object(),
                "{result}"
            );
        }
    }

    mod initialize {
        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn succeeds() {
            let d = Daemon::start();
            let client = d.rmcp("s1", "a").await;
            assert_eq!(
                client.peer_info().unwrap().protocol_version.as_str(),
                "2025-11-25"
            );
            let result = client
                .call_tool(CallToolRequestParams::new("type_list"))
                .await
                .unwrap();
            assert_ne!(result.is_error, Some(true));
            client.cancel().await.unwrap();
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn later_requests_still_take_identity_from_their_own_headers() {
            let d = Daemon::start();
            let first = d.session("s1", "a").legacy();
            let reply = first.request("initialize", initialize_params()).await;
            assert!(reply.message.get("result").is_some(), "{}", reply.message);
            d.session("s2", "b")
                .legacy()
                .ok("resource_create", json!({"id": "board", "type": "generic"}))
                .await;
            assert_eq!(
                d.actor_of("resource_created").await,
                json!({"kind": "agent_session", "session_id": "s2"})
            );
            assert_eq!(
                d.all_resources().await[0]["scope"],
                json!({"project": d.project("b").to_str().unwrap()})
            );
        }
    }

    mod tools_list {
        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn lists_exactly_the_tool_names_each_with_an_input_schema() {
            let d = Daemon::start();
            let client = d.rmcp("s1", "a").await;
            let tools = client.list_all_tools().await.unwrap();
            let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
            names.sort_unstable();
            let mut expected = TOOL_NAMES.to_vec();
            expected.sort_unstable();
            assert_eq!(names, expected);
            for tool in &tools {
                assert_eq!(tool.input_schema["type"], json!("object"), "{}", tool.name);
            }
            client.cancel().await.unwrap();
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn lists_no_admin_audit_promotion_list_or_holder_lookup_tool() {
            let d = Daemon::start();
            let reply = d.session("s1", "a").request("tools/list", json!({})).await;
            let tools = reply.message["result"]["tools"].as_array().unwrap().clone();
            assert_ne!(tools, Vec::<Value>::new());
            for tool in tools {
                let name = tool["name"].as_str().unwrap();
                for banned in ["admin", "audit", "promotion", "holder", "approve"] {
                    assert!(!name.contains(banned), "{name}");
                }
            }
        }
    }

    mod any_tool {
        use super::*;

        mod on_an_unknown_argument {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn returns_a_tool_error() {
                let d = Daemon::start();
                let s = d.session("s1", "a");
                for field in ["holder", "session", "project"] {
                    let error = s
                        .err(
                            "resource_create",
                            json!({"id": "board", "type": "generic", field: "someone-else"}),
                        )
                        .await;
                    assert_eq!(error["code"], json!("invalid_request"), "{field}: {error}");
                    assert!(
                        error["message"].as_str().unwrap().contains(field),
                        "{field}: {error}"
                    );
                }
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn changes_nothing() {
                let d = Daemon::start();
                let s = d.session("s1", "a");
                for field in ["holder", "session", "project"] {
                    s.call(
                        "resource_create",
                        json!({"id": "board", "type": "generic", field: "someone-else"}),
                    )
                    .await;
                }
                assert_eq!(d.all_resources().await, Vec::<Value>::new());
            }
        }

        mod on_an_engine_error {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn returns_a_tool_error_carrying_code_message_and_details() {
                let d = Daemon::start();
                let s = d.session("s1", "a");
                let result = s.call("resource_get", json!({"id": "nope"})).await;
                assert_eq!(result["isError"], json!(true));
                let error = &result["structuredContent"]["error"];
                assert_eq!(error["code"], json!("not_found"));
                assert_ne!(error["message"].as_str().unwrap(), "");
                assert_eq!(error["details"], json!({"kind": "resource", "id": "nope"}));
                assert!(text_of(&result).contains("not_found"));
            }
        }

        mod on_success {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn returns_structured_content_plus_a_text_block_ending_in_the_next_step() {
                let d = Daemon::start();
                let s = d.session("s1", "a");
                let result = s
                    .call("resource_create", json!({"id": "board", "type": "generic"}))
                    .await;
                assert_eq!(
                    result["structuredContent"]["resource"]["id"],
                    json!("board")
                );
                let text = text_of(&result);
                assert!(
                    text.ends_with("Call unit_add to register its units."),
                    "{text}"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// mcp::tools inventory
// ---------------------------------------------------------------------------

mod tools {
    use super::*;

    mod resource_create {
        use super::*;

        mod on_no_scope_with_a_resolved_project {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn creates_in_project_scope() {
                let d = Daemon::start();
                let created = d
                    .session("s1", "a")
                    .ok("resource_create", json!({"id": "board", "type": "generic"}))
                    .await;
                assert_eq!(
                    created["resource"]["scope"],
                    json!({"project": d.project("a").to_str().unwrap()})
                );
            }
        }

        mod on_no_project {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn creates_in_user_scope() {
                let d = Daemon::start();
                let created = d
                    .session_without_project("s1")
                    .ok("resource_create", json!({"id": "board", "type": "generic"}))
                    .await;
                assert_eq!(created["resource"]["scope"], json!("user"));
            }
        }

        mod on_a_similar_resource_in_another_project {
            use super::*;

            /// Project b already has a unit with key `k`; session s1 in
            /// project a registers the same thing. The warning comes with the
            /// unit, where the clerk can tell the two apart.
            async fn add_similar(d: &Daemon) -> Value {
                seed(&d.session("s2", "b"), "theirs", "k", &[]).await;
                let s = d.session("s1", "a");
                s.ok("resource_create", json!({"id": "mine", "type": "generic"}))
                    .await;
                s.call(
                    "unit_add",
                    json!({"resource": "mine", "id": "m1", "settings": {"type": "generic", "identity_key": "k"}}),
                )
                .await
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn result_carries_the_similar_in_other_project_warning() {
                let d = Daemon::start();
                let result = add_similar(&d).await;
                let warnings = result["structuredContent"]["warnings"].as_array().unwrap();
                assert_eq!(warnings.len(), 1, "{result}");
                assert_eq!(warnings[0]["kind"], json!("similar_in_other_project"));
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn result_says_to_tell_the_user() {
                let d = Daemon::start();
                let text = text_of(&add_similar(&d).await);
                assert!(text.contains("Tell the user"), "{text}");
                assert!(text.contains("clerk approve"), "{text}");
            }
        }
    }

    mod unit_add {
        use super::*;

        mod on_an_unknown_key_inside_settings {
            use super::*;

            async fn add_with_extra(s: &Raw) -> Value {
                s.ok("resource_create", json!({"id": "svc", "type": "service"}))
                    .await;
                s.call(
                    "unit_add",
                    json!({"resource": "svc", "id": "u1", "settings": {
                        "type": "service", "mode": "connect", "host": "localhost", "port": 8080,
                        "sevice_id": "typo"}}),
                )
                .await
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn returns_a_tool_error() {
                let d = Daemon::start();
                let result = add_with_extra(&d.session("s1", "a")).await;
                assert_eq!(result["isError"], json!(true), "{result}");
                let error = &result["structuredContent"]["error"];
                assert_eq!(error["code"], json!("invalid_request"), "{error}");
                assert!(
                    error["message"].as_str().unwrap().contains("sevice_id"),
                    "{error}"
                );
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn adds_no_unit() {
                let d = Daemon::start();
                let s = d.session("s1", "a");
                add_with_extra(&s).await;
                let got = s.ok("resource_get", json!({"id": "svc"})).await;
                assert_eq!(got["resource"]["units"], json!([]));
            }
        }
    }

    mod resource_list {
        use super::*;

        mod on_tags {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn returns_only_visible_resources_carrying_all_of_them() {
                let d = Daemon::start();
                let s = d.session("s1", "a");
                seed(&s, "both", "k1", &["esp32", "usb"]).await;
                seed(&s, "one", "k2", &["esp32"]).await;
                seed(&d.session("s2", "b"), "elsewhere", "k3", &["esp32", "usb"]).await;
                let listed = s
                    .ok("resource_list", json!({"tags": ["esp32", "usb"]}))
                    .await;
                let ids: Vec<&str> = listed["resources"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|r| r["id"].as_str().unwrap())
                    .collect();
                assert_eq!(ids, vec!["both"]);
            }
        }

        mod on_another_projects_resource {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn is_never_returned() {
                let d = Daemon::start();
                seed(&d.session("s2", "b"), "theirs", "k", &[]).await;
                let listed = d.session("s1", "a").ok("resource_list", json!({})).await;
                assert_eq!(listed["resources"], json!([]));
            }
        }
    }

    mod resource_get_and_unit_tools {
        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn map_to_the_engine_command_with_view_caller() {
            let d = Daemon::start();
            let s = d.session("s1", "a");
            seed(&s, "board", "k", &[]).await;
            let units = |got: &Value| got["resource"]["units"].as_array().unwrap().clone();
            s.ok(
                "unit_update",
                json!({"resource": "board", "unit": "u1", "description": "the first"}),
            )
            .await;
            let got = s.ok("resource_get", json!({"id": "board"})).await;
            assert_eq!(units(&got)[0]["description"], json!("the first"));
            s.ok(
                "unit_add",
                json!({"resource": "board", "id": "u2", "settings": {"type": "generic", "identity_key": "k2"}}),
            )
            .await;
            assert_eq!(
                units(&s.ok("resource_get", json!({"id": "board"})).await).len(),
                2
            );
            s.ok("unit_remove", json!({"resource": "board", "unit": "u2"}))
                .await;
            assert_eq!(
                units(&s.ok("resource_get", json!({"id": "board"})).await).len(),
                1
            );
            s.ok("unit_retire", json!({"resource": "board", "unit": "u1"}))
                .await;
            assert_eq!(
                units(&s.ok("resource_get", json!({"id": "board"})).await),
                Vec::<Value>::new()
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn other_projects_read_as_not_found() {
            let d = Daemon::start();
            seed(&d.session("s2", "b"), "theirs", "k", &[]).await;
            let s = d.session("s1", "a");
            let calls = [
                ("resource_get", json!({"id": "theirs"})),
                (
                    "resource_update",
                    json!({"id": "theirs", "description": "mine now"}),
                ),
                ("resource_delete", json!({"id": "theirs"})),
                (
                    "unit_add",
                    json!({"resource": "theirs", "id": "u9", "settings": {"type": "generic", "identity_key": "k9"}}),
                ),
                (
                    "unit_update",
                    json!({"resource": "theirs", "unit": "u1", "description": "x"}),
                ),
                ("unit_retire", json!({"resource": "theirs", "unit": "u1"})),
                ("unit_remove", json!({"resource": "theirs", "unit": "u1"})),
            ];
            for (tool, args) in calls {
                let error = s.err(tool, args).await;
                assert_eq!(error["code"], json!("not_found"), "{tool}: {error}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// mcp::tools leasing
// ---------------------------------------------------------------------------

/// The holder of agent session `id`.
fn agent(id: &str) -> HolderRef {
    HolderRef::AgentSession {
        session_id: id.to_owned(),
    }
}

/// Waits short enough for tests: progress every 50 ms, an empty batch after
/// 400 ms.
fn quick() -> Config {
    Config {
        mcp_progress_ms: 50,
        mcp_wait_cap_ms: 400,
        ..Config::default()
    }
}

/// `lease_acquire` of any unit of `resource` for an hour; its structured
/// content.
async fn acquire(s: &Raw, resource: &str) -> Value {
    acquire_with(
        s,
        json!({"target": {"resource": resource}, "for": "1h", "purpose": "flash"}),
    )
    .await
}

async fn acquire_with(s: &Raw, args: Value) -> Value {
    s.ok("lease_acquire", args).await
}

/// The sentence every `lease_wait` result ends with, for `ids`.
fn re_arm(ids: &[String]) -> String {
    format!(
        "Call lease_wait again with ack={} while you hold or wait for anything.",
        serde_json::to_string(ids).unwrap()
    )
}

/// The ids of a `lease_wait` result's notices.
fn notice_ids(structured: &Value) -> Vec<String> {
    structured["notices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_str().unwrap().to_owned())
        .collect()
}

/// Give `s` a `lease_ended` notice: it leases `resource`, and the user ends
/// the lease. Returns the lease id.
async fn ended_notice(d: &Daemon, s: &Raw, resource: &str) -> String {
    let grant = acquire(s, resource).await;
    let lease = grant["lease_id"].as_str().unwrap().to_owned();
    d.admin("admin_release", json!({"lease_id": lease})).await;
    lease
}

/// The undelivered notice ids of `session`, as the engine sees them.
async fn pending_ids(d: &Daemon, session: &str) -> Vec<String> {
    d.engine
        .pending_notices(agent(session))
        .await
        .unwrap()
        .iter()
        .map(|n| n.id.as_str().to_owned())
        .collect()
}

/// Poll until `f` holds, for at most 5 s.
async fn eventually(mut f: impl FnMut() -> bool, what: &str) {
    let until = Instant::now() + Duration::from_secs(5);
    while !f() {
        assert!(Instant::now() < until, "timed out waiting until {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// `s1` holds the single unit of `board`; `s2` queues for it. Returns the
/// lease and the request.
async fn held_pool(s1: &Raw, s2: &Raw) -> (String, Value) {
    seed(s1, "board", "k", &[]).await;
    let grant = acquire(s1, "board").await;
    let queued = acquire(s2, "board").await;
    (grant["lease_id"].as_str().unwrap().to_owned(), queued)
}

/// `s1` releases `lease` and confirms it clean, so the unit goes to the
/// next waiter.
async fn hand_over(s1: &Raw, lease: &str) {
    s1.ok("lease_release", json!({"lease_id": lease})).await;
    s1.ok("lease_confirm", json!({"lease_id": lease})).await;
}

mod leasing {
    use super::*;

    mod lease_acquire {
        use super::*;

        mod on_a_free_unit {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn returns_the_grant_with_descriptions_tags_and_rendered_usage_next_step_names_lease_release()
             {
                let d = Daemon::start();
                let s = d.session("s1", "a");
                s.ok(
                    "resource_create",
                    json!({"id": "board", "type": "generic", "description": "an nRF board",
                           "usage": "Flash {unit.id} of {resource.id}.", "tags": ["nrf"]}),
                )
                .await;
                s.ok(
                    "unit_add",
                    json!({"resource": "board", "id": "u1", "description": "the left one",
                           "settings": {"type": "generic", "identity_key": "k"}}),
                )
                .await;
                let result = s
                    .call(
                        "lease_acquire",
                        json!({"target": {"resource": "board"}, "for": "30m", "purpose": "flash"}),
                    )
                    .await;
                let got = &result["structuredContent"];
                assert_eq!(got["outcome"], json!("granted"), "{result}");
                assert_eq!(got["resource_description"], json!("an nRF board"));
                assert_eq!(got["unit_description"], json!("the left one"));
                assert_eq!(got["resource_tags"], json!(["nrf"]));
                assert_eq!(got["usage"], json!("Flash u1 of board."));
                let text = text_of(&result);
                assert!(text.contains("Flash u1 of board."), "{text}");
                let next = text.rsplit(". ").next().unwrap();
                assert!(next.contains("lease_release"), "{text}");
            }
        }

        mod on_a_held_pool {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn returns_queued_with_position_next_step_is_lease_wait() {
                let d = Daemon::start();
                let (s1, s2) = (d.session("s1", "a"), d.session("s2", "a"));
                seed(&s1, "board", "k", &[]).await;
                acquire(&s1, "board").await;
                let result = s2
                    .call(
                        "lease_acquire",
                        json!({"target": {"resource": "board"}, "for": "1h", "purpose": "test"}),
                    )
                    .await;
                let got = &result["structuredContent"];
                assert_eq!(got["outcome"], json!("queued"), "{result}");
                assert_eq!(got["position"], json!(1));
                let text = text_of(&result);
                let next = text.rsplit(". ").next().unwrap();
                assert!(next.contains("lease_wait"), "{text}");
            }
        }

        mod on_a_resource_with_no_units {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn returns_tool_error_no_units() {
                let d = Daemon::start();
                let s = d.session("s1", "a");
                s.ok("resource_create", json!({"id": "empty", "type": "generic"}))
                    .await;
                let error = s
                    .err(
                        "lease_acquire",
                        json!({"target": {"resource": "empty"}, "for": "1h", "purpose": "x"}),
                    )
                    .await;
                assert_eq!(error["code"], json!("no_units"), "{error}");
            }
        }
    }

    mod lease_wait {
        use super::*;

        mod on_pending_notices {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn returns_them_at_once_listing_their_ids_in_the_text_block() {
                let d = Daemon::start();
                let s = d.session("s1", "a");
                seed(&s, "board", "k", &[]).await;
                let lease = ended_notice(&d, &s, "board").await;
                let started = Instant::now();
                let result = s.call("lease_wait", json!({})).await;
                assert!(started.elapsed() < Duration::from_secs(5));
                let got = &result["structuredContent"];
                let ids = notice_ids(got);
                assert!(
                    got["notices"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|n| n["kind"] == json!("lease_ended") && n["lease_id"] == json!(lease)),
                    "{result}"
                );
                let text = text_of(&result);
                for id in &ids {
                    assert!(text.contains(id.as_str()), "{id} not in {text}");
                }
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn does_not_mark_them_delivered() {
                let d = Daemon::start();
                let s = d.session("s1", "a");
                seed(&s, "board", "k", &[]).await;
                ended_notice(&d, &s, "board").await;
                let ids = notice_ids(&s.ok("lease_wait", json!({})).await);
                assert_ne!(ids, Vec::<String>::new());
                assert_eq!(pending_ids(&d, "s1").await, ids);
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn returns_only_this_sessions_notices() {
                let d = Daemon::start_with(quick());
                let s1 = d.session("s1", "a");
                seed(&s1, "board", "k", &[]).await;
                ended_notice(&d, &s1, "board").await;
                let got = d.session("s2", "a").ok("lease_wait", json!({})).await;
                assert_eq!(got["notices"], json!([]), "{got}");
            }
        }

        mod on_ack_with_ids_from_the_previous_result {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn marks_exactly_those_delivered_they_are_not_returned_again() {
                let d = Daemon::start_with(quick());
                let s = d.session("s1", "a");
                seed(&s, "board", "k", &[]).await;
                seed(&s, "probe", "k2", &[]).await;
                ended_notice(&d, &s, "board").await;
                ended_notice(&d, &s, "probe").await;
                let first = notice_ids(&s.ok("lease_wait", json!({})).await);
                assert!(first.len() >= 2, "{first:?}");
                let (acked, kept) = first.split_at(1);
                let again = notice_ids(&s.ok("lease_wait", json!({"ack": acked})).await);
                assert_eq!(again, kept);
                assert_eq!(pending_ids(&d, "s1").await, kept);
            }
        }

        mod on_a_previous_result_never_acknowledged {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn returns_its_notices_again() {
                let d = Daemon::start();
                let s = d.session("s1", "a");
                seed(&s, "board", "k", &[]).await;
                ended_notice(&d, &s, "board").await;
                let first = notice_ids(&s.ok("lease_wait", json!({})).await);
                let again = notice_ids(&s.ok("lease_wait", json!({})).await);
                assert_ne!(first, Vec::<String>::new());
                assert_eq!(again, first);
            }
        }

        mod on_an_offered_notice {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn claims_it_in_the_same_call_and_returns_the_grant_brief() {
                let d = Daemon::start();
                let (s1, s2) = (d.session("s1", "a"), d.session("s2", "a"));
                let (lease, _) = held_pool(&s1, &s2).await;
                hand_over(&s1, &lease).await;
                let result = s2.call("lease_wait", json!({})).await;
                let got = &result["structuredContent"];
                let offered = got["notices"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|n| n["kind"] == json!("offered"))
                    .unwrap_or_else(|| panic!("no offer in {result}"))
                    .clone();
                let grant = &got["grants"][0];
                assert_eq!(grant["lease_id"], offered["lease_id"], "{result}");
                assert_eq!(grant["state"], json!("held"), "{result}");
                assert_eq!(grant["resource_description"], json!("a thing"));
                let now = d.lease_of("s2", &offered["lease_id"]).await;
                assert_eq!(now["state"], json!("held"));
                assert!(text_of(&result).contains("lease_release"), "{result}");
            }
        }

        mod on_the_same_offer_returned_twice {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn yields_one_grant_of_the_same_lease_and_no_claim_error() {
                let d = Daemon::start();
                let (s1, s2) = (d.session("s1", "a"), d.session("s2", "a"));
                let (lease, _) = held_pool(&s1, &s2).await;
                hand_over(&s1, &lease).await;
                let first = s2.ok("lease_wait", json!({})).await;
                let again = s2.ok("lease_wait", json!({})).await;
                for got in [&first, &again] {
                    assert_eq!(got["grants"].as_array().unwrap().len(), 1, "{got}");
                    assert_eq!(got["claim_errors"], json!([]), "{got}");
                }
                assert_eq!(
                    again["grants"][0]["lease_id"],
                    first["grants"][0]["lease_id"]
                );
            }
        }

        mod on_none_pending {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn sends_a_progress_notification_every_interval() {
                let d = Daemon::start_with(quick());
                let s = d.session("s1", "a").with_progress();
                let (_, id, messages) = s
                    .exchange("tools/call", json!({"name": "lease_wait", "arguments": {}}))
                    .await;
                let progress: Vec<&Value> = messages
                    .iter()
                    .filter(|m| m["method"] == json!("notifications/progress"))
                    .collect();
                // 400 ms at one every 50 ms: about 7, allowing for slow runs.
                assert!(
                    (3..=8).contains(&progress.len()),
                    "{} progress notifications: {messages:?}",
                    progress.len()
                );
                for p in progress {
                    assert_eq!(p["params"]["progressToken"], json!(id), "{p}");
                }
            }

            #[test]
            fn the_default_progress_interval_is_30_s() {
                assert_eq!(Config::default().mcp_progress_ms, 30_000);
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn returns_an_empty_batch_after_the_cap() {
                let d = Daemon::start_with(quick());
                let s = d.session("s1", "a");
                let started = Instant::now();
                let got = s.ok("lease_wait", json!({})).await;
                let waited = started.elapsed();
                assert_eq!(got["notices"], json!([]), "{got}");
                assert_eq!(got["timed_out"], json!(true), "{got}");
                assert!(waited >= Duration::from_millis(400), "{waited:?}");
                assert!(waited < Duration::from_secs(5), "{waited:?}");
            }

            #[test]
            fn the_default_cap_is_20_min() {
                assert_eq!(Config::default().mcp_wait_cap_ms, 20 * 60 * 1000);
            }
        }

        mod on_the_proxy_disconnecting_mid_wait {
            use super::*;

            /// Start a `lease_wait` for `s1`, see it open, and drop the
            /// connection.
            async fn wait_and_disconnect(d: &Daemon) {
                let s = d.session("s1", "a");
                let call = tokio::spawn(async move {
                    s.call_and_disconnect("lease_wait", json!({}), Duration::from_millis(300))
                        .await;
                });
                let engine = d.engine.clone();
                eventually(|| engine.open_waits(&agent("s1")) == 1, "the wait is open").await;
                call.await.unwrap();
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn the_wait_ends() {
                let d = Daemon::start();
                wait_and_disconnect(&d).await;
                let engine = d.engine.clone();
                eventually(|| engine.open_waits(&agent("s1")) == 0, "the wait ends").await;
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn unacknowledged_notices_stay_pending() {
                let d = Daemon::start();
                let s = d.session("s1", "a");
                seed(&s, "board", "k", &[]).await;
                wait_and_disconnect(&d).await;
                ended_notice(&d, &s, "board").await;
                let pending = pending_ids(&d, "s1").await;
                assert_ne!(pending, Vec::<String>::new());
                let got = notice_ids(&s.ok("lease_wait", json!({})).await);
                assert_eq!(got, pending);
            }
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn every_result_ends_with_the_re_arm_sentence() {
            let d = Daemon::start_with(quick());
            let s = d.session("s1", "a");
            let empty = s.call("lease_wait", json!({})).await;
            assert!(text_of(&empty).ends_with(&re_arm(&[])), "{empty}");
            seed(&s, "board", "k", &[]).await;
            ended_notice(&d, &s, "board").await;
            let full = s.call("lease_wait", json!({})).await;
            let ids = notice_ids(&full["structuredContent"]);
            assert_ne!(ids, Vec::<String>::new());
            assert!(text_of(&full).ends_with(&re_arm(&ids)), "{full}");
        }
    }

    mod lease_status {
        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn returns_this_sessions_leases_queue_positions_and_pending_notice_count_only() {
            let d = Daemon::start();
            let (s1, s2) = (d.session("s1", "a"), d.session("s2", "a"));
            seed(&s1, "board", "k", &[]).await;
            seed(&s1, "probe", "k2", &[]).await;
            seed(&s1, "spare", "k3", &[]).await;
            let mine = acquire(&s1, "board").await;
            acquire(&s2, "probe").await;
            let queued = acquire(&s1, "probe").await;
            ended_notice(&d, &s1, "spare").await;
            let got = s1.ok("lease_status", json!({})).await;
            let leases: Vec<&Value> = got["leases"].as_array().unwrap().iter().collect();
            assert_eq!(leases.len(), 1, "{got}");
            assert_eq!(leases[0]["lease_id"], mine["lease_id"]);
            let requests = got["requests"].as_array().unwrap();
            assert_eq!(requests.len(), 1, "{got}");
            assert_eq!(requests[0]["request_id"], queued["request_id"]);
            assert_eq!(requests[0]["position"], json!(1));
            assert_eq!(
                got["pending_notices"],
                json!(pending_ids(&d, "s1").await.len()),
                "{got}"
            );
            let theirs = s2.ok("lease_status", json!({})).await;
            assert_eq!(theirs["requests"], json!([]), "{theirs}");
            assert_eq!(theirs["pending_notices"], json!(0), "{theirs}");
        }
    }

    /// `lease_extend` / `lease_release` / `lease_confirm` /
    /// `lease_set_interruptible` / `lease_yield_refuse` / `lease_reclaim` /
    /// `lease_cancel` / `lease_claim`: map 1:1 to the engine; the next step
    /// text matches the resulting state.
    mod lease_lifecycle {
        use super::*;

        /// The last sentence of a tool result's text block.
        fn next_step(result: &Value) -> String {
            text_of(result).rsplit(". ").next().unwrap().to_owned()
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn lease_extend_moves_the_end_from_now_next_step_lease_release() {
            let d = Daemon::start();
            let s = d.session("s1", "a");
            seed(&s, "board", "k", &[]).await;
            let lease = acquire(&s, "board").await["lease_id"].clone();
            let result = s
                .call("lease_extend", json!({"lease_id": lease, "for": "2h"}))
                .await;
            assert_eq!(
                result["structuredContent"]["ends_at"],
                json!(T0 + 2 * 3_600_000),
                "{result}"
            );
            assert!(next_step(&result).contains("lease_release"), "{result}");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn lease_release_starts_cleanup_next_step_lease_confirm() {
            let d = Daemon::start();
            let s = d.session("s1", "a");
            seed(&s, "board", "k", &[]).await;
            let lease = acquire(&s, "board").await["lease_id"].clone();
            let result = s.call("lease_release", json!({"lease_id": lease})).await;
            assert_eq!(
                result["structuredContent"]["state"],
                json!("cleaning"),
                "{result}"
            );
            assert!(next_step(&result).contains("lease_confirm"), "{result}");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn lease_release_failed_ends_it_dirty_next_step_lease_status() {
            let d = Daemon::start();
            let s = d.session("s1", "a");
            seed(&s, "board", "k", &[]).await;
            let lease = acquire(&s, "board").await["lease_id"].clone();
            let result = s
                .call("lease_release", json!({"lease_id": lease, "failed": true}))
                .await;
            assert_eq!(result["structuredContent"]["state"], json!("ended"));
            assert!(next_step(&result).contains("lease_status"), "{result}");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn lease_confirm_ends_the_lease_next_step_lease_status() {
            let d = Daemon::start();
            let s = d.session("s1", "a");
            seed(&s, "board", "k", &[]).await;
            let lease = acquire(&s, "board").await["lease_id"].clone();
            s.ok("lease_release", json!({"lease_id": lease})).await;
            let result = s.call("lease_confirm", json!({"lease_id": lease})).await;
            assert_ne!(result["isError"], json!(true), "{result}");
            let now = d.lease_of("s1", &lease).await;
            assert_eq!(now["state"], json!("ended"));
            assert!(next_step(&result).contains("lease_status"), "{result}");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn lease_set_interruptible_sets_it_next_step_lease_wait() {
            let d = Daemon::start();
            let s = d.session("s1", "a");
            seed(&s, "board", "k", &[]).await;
            let lease = acquire(&s, "board").await["lease_id"].clone();
            let result = s
                .call(
                    "lease_set_interruptible",
                    json!({"lease_id": lease, "interruptible": true}),
                )
                .await;
            assert_eq!(result["structuredContent"]["interruptible"], json!(true));
            assert!(next_step(&result).contains("lease_wait"), "{result}");
        }

        /// `s1` holds `board` interruptibly; `s2` asks for it at high
        /// priority, so `s1` is asked to yield. Returns s1's lease.
        async fn asked_to_yield(s1: &Raw, s2: &Raw) -> Value {
            seed(s1, "board", "k", &[]).await;
            let lease = acquire_with(
                s1,
                json!({"target": {"resource": "board"}, "for": "1h", "purpose": "long",
                       "interruptible": true}),
            )
            .await["lease_id"]
                .clone();
            acquire_with(
                s2,
                json!({"target": {"resource": "board"}, "for": "5m", "purpose": "urgent",
                       "priority": "high"}),
            )
            .await;
            lease
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn lease_yield_refuse_withdraws_the_ask_next_step_lease_release() {
            let d = Daemon::start();
            let (s1, s2) = (d.session("s1", "a"), d.session("s2", "a"));
            let lease = asked_to_yield(&s1, &s2).await;
            let asked = d.lease_of("s1", &lease).await;
            assert!(asked["yield_deadline"].is_i64(), "{asked}");
            let result = s1
                .call("lease_yield_refuse", json!({"lease_id": lease}))
                .await;
            assert_ne!(result["isError"], json!(true), "{result}");
            let now = d.lease_of("s1", &lease).await;
            assert_eq!(now["yield_deadline"], json!(null), "{now}");
            assert!(next_step(&result).contains("lease_release"), "{result}");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn lease_reclaim_returns_the_engines_answer() {
            let d = Daemon::start();
            let (s1, s2) = (d.session("s1", "a"), d.session("s2", "a"));
            let lease = asked_to_yield(&s1, &s2).await;
            s1.ok(
                "lease_confirm",
                json!({"lease_id": lease, "how": "yielded"}),
            )
            .await;
            s2.ok("lease_wait", json!({})).await;
            let error = s1.err("lease_reclaim", json!({"lease_id": lease})).await;
            assert_eq!(error["code"], json!("borrower_not_overrun"), "{error}");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn lease_acquire_by_a_paused_holder_overriding_reclaims_next_step_lease_wait() {
            let d = Daemon::start();
            let (s1, s2) = (d.session("s1", "a"), d.session("s2", "a"));
            let lease = asked_to_yield(&s1, &s2).await;
            s1.ok(
                "lease_confirm",
                json!({"lease_id": lease, "how": "yielded"}),
            )
            .await;
            s2.ok("lease_wait", json!({})).await;
            let result = s1
                .call(
                    "lease_acquire",
                    json!({"target": {"resource": "board"}, "for": "1h", "purpose": "back",
                           "override_reason": "the user needs it now"}),
                )
                .await;
            assert_eq!(
                result["structuredContent"]["outcome"],
                json!("reclaiming"),
                "{result}"
            );
            assert!(text_of(&result).contains("comes back to you"), "{result}");
            assert!(next_step(&result).contains("lease_wait"), "{result}");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn lease_cancel_drops_the_request_next_step_lease_status() {
            let d = Daemon::start();
            let (s1, s2) = (d.session("s1", "a"), d.session("s2", "a"));
            let (_, queued) = held_pool(&s1, &s2).await;
            let result = s2
                .call("lease_cancel", json!({"request_id": queued["request_id"]}))
                .await;
            assert_ne!(result["isError"], json!(true), "{result}");
            let status = s2.ok("lease_status", json!({})).await;
            assert_eq!(status["requests"], json!([]), "{status}");
            assert!(next_step(&result).contains("lease_status"), "{result}");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn lease_claim_takes_the_offer_next_step_lease_release() {
            let d = Daemon::start();
            let (s1, s2) = (d.session("s1", "a"), d.session("s2", "a"));
            let (lease, _) = held_pool(&s1, &s2).await;
            hand_over(&s1, &lease).await;
            let offered = d
                .engine
                .pending_notices(agent("s2"))
                .await
                .unwrap()
                .into_iter()
                .find_map(|n| n.lease_id)
                .unwrap();
            let result = s2
                .call("lease_claim", json!({"lease_id": offered.as_str()}))
                .await;
            assert_eq!(
                result["structuredContent"]["state"],
                json!("held"),
                "{result}"
            );
            assert!(next_step(&result).contains("lease_release"), "{result}");
        }
    }

    mod tasks {
        use super::*;

        /// Poll `tasks/get` until the task leaves `working`; its result.
        async fn finished(s: &Raw, task: &str) -> Value {
            let until = Instant::now() + Duration::from_secs(5);
            loop {
                let reply = s.request("tasks/get", json!({"taskId": task})).await;
                let got = reply.message["result"].clone();
                if got["status"] != json!("working") {
                    return got;
                }
                assert!(Instant::now() < until, "task {task} never finished");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        mod on_a_request_whose_client_capabilities_declare_tasks {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn lease_acquire_on_a_held_pool_returns_a_task_at_once_that_completes_with_the_grant()
             {
                let d = Daemon::start();
                let (s1, s2) = (d.session("s1", "a"), d.session("s2", "a").with_tasks());
                seed(&s1, "board", "k", &[]).await;
                let lease = acquire(&s1, "board").await["lease_id"].clone();
                let started = Instant::now();
                let handle = s2
                    .call(
                        "lease_acquire",
                        json!({"target": {"resource": "board"}, "for": "1h", "purpose": "x"}),
                    )
                    .await;
                assert!(started.elapsed() < Duration::from_secs(2));
                assert_eq!(handle["resultType"], json!("task"), "{handle}");
                assert_eq!(handle["pollIntervalMs"], json!(5000), "{handle}");
                hand_over(&s1, lease.as_str().unwrap()).await;
                let done = finished(&s2, handle["taskId"].as_str().unwrap()).await;
                assert_eq!(done["status"], json!("completed"), "{done}");
                let grant = &done["result"]["structuredContent"];
                assert_eq!(grant["state"], json!("held"), "{done}");
                assert!(
                    done["result"]["content"][0]["text"]
                        .as_str()
                        .unwrap()
                        .contains("lease_release"),
                    "{done}"
                );
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn an_open_task_counts_as_an_open_wait() {
                let d = Daemon::start();
                let (s1, s2) = (d.session("s1", "a"), d.session("s2", "a").with_tasks());
                seed(&s1, "board", "k", &[]).await;
                let lease = acquire(&s1, "board").await["lease_id"].clone();
                let handle = s2
                    .call(
                        "lease_acquire",
                        json!({"target": {"resource": "board"}, "for": "1h", "purpose": "x"}),
                    )
                    .await;
                assert_eq!(handle["resultType"], json!("task"), "{handle}");
                assert_eq!(d.engine.open_waits(&agent("s2")), 1);
                hand_over(&s1, lease.as_str().unwrap()).await;
                finished(&s2, handle["taskId"].as_str().unwrap()).await;
                let engine = d.engine.clone();
                eventually(|| engine.open_waits(&agent("s2")) == 0, "the task closes").await;
            }
        }

        /// `lease_wait` as a task of `s` (which declares tasks); the
        /// handle.
        async fn wait_task(s: &Raw) -> Value {
            let handle = s.call("lease_wait", json!({})).await;
            assert_eq!(handle["resultType"], json!("task"), "{handle}");
            handle
        }

        mod lease_wait_as_a_task {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn completes_with_the_same_result_shape_as_the_blocking_path() {
                let d = Daemon::start();
                let s = d.session("s1", "a");
                seed(&s, "board", "k", &[]).await;
                ended_notice(&d, &s, "board").await;
                let t = d.session("s1", "a").with_tasks();
                let handle = wait_task(&t).await;
                let done = finished(&t, handle["taskId"].as_str().unwrap()).await;
                assert_eq!(done["status"], json!("completed"), "{done}");
                let blocking = s.call("lease_wait", json!({})).await;
                assert_eq!(
                    done["result"]["structuredContent"],
                    blocking["structuredContent"]
                );
                assert_eq!(text_of(&done["result"]), text_of(&blocking));
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn a_never_fetched_result_leaves_its_notices_pending() {
                let d = Daemon::start();
                let s = d.session("s1", "a");
                seed(&s, "board", "k", &[]).await;
                ended_notice(&d, &s, "board").await;
                let pending = pending_ids(&d, "s1").await;
                wait_task(&d.session("s1", "a").with_tasks()).await;
                let engine = d.engine.clone();
                eventually(|| engine.open_waits(&agent("s1")) == 0, "the task ends").await;
                assert_eq!(pending_ids(&d, "s1").await, pending);
                let again = notice_ids(&s.ok("lease_wait", json!({})).await);
                assert_eq!(again, pending);
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn tasks_cancel_closes_its_open_wait() {
                let d = Daemon::start();
                let s = d.session("s1", "a").with_tasks();
                let handle = wait_task(&s).await;
                assert_eq!(d.engine.open_waits(&agent("s1")), 1);
                let reply = s
                    .request("tasks/cancel", json!({"taskId": handle["taskId"]}))
                    .await;
                assert!(reply.message.get("error").is_none(), "{}", reply.message);
                let engine = d.engine.clone();
                eventually(|| engine.open_waits(&agent("s1")) == 0, "the task closes").await;
            }
        }

        mod on_an_acquire_task_whose_offer_cannot_be_claimed {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn the_tool_error_ends_with_lease_wait_as_the_next_step() {
                let d = Daemon::start();
                let (s1, s2) = (d.session("s1", "a"), d.session("s2", "a").with_tasks());
                seed(&s1, "board", "k", &[]).await;
                let lease = acquire(&s1, "board").await["lease_id"].clone();
                let handle = s2
                    .call(
                        "lease_acquire",
                        json!({"target": {"resource": "board"}, "for": "1h", "purpose": "x"}),
                    )
                    .await;
                assert_eq!(handle["resultType"], json!("task"), "{handle}");
                s1.ok("lease_release", json!({"lease_id": lease})).await;
                // Both jobs reach the engine before the task wakes: the
                // confirm makes the offer, the eviction ends it unclaimed.
                let (confirmed, evicted) = tokio::join!(
                    d.as_session("s1", "confirm", json!({"lease_id": lease})),
                    d.admin("admin_evict", json!({"holder": "session:s2"})),
                );
                confirmed.unwrap();
                assert!(evicted.is_object(), "{evicted}");
                let done = finished(&s2, handle["taskId"].as_str().unwrap()).await;
                assert_eq!(done["status"], json!("completed"), "{done}");
                assert_eq!(done["result"]["isError"], json!(true), "{done}");
                let text = text_of(&done["result"]);
                let next = text.rsplit(". ").next().unwrap();
                assert!(next.starts_with("Call lease_wait"), "{text}");
            }
        }

        mod on_another_sessions_task {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn tasks_get_reads_as_unknown() {
                let d = Daemon::start();
                let (s1, s2) = (d.session("s1", "a"), d.session("s2", "a").with_tasks());
                held_pool(&s1, &d.session("s3", "a")).await;
                let handle = s2
                    .call(
                        "lease_acquire",
                        json!({"target": {"resource": "board"}, "for": "1h", "purpose": "x"}),
                    )
                    .await;
                let task = handle["taskId"].as_str().unwrap();
                let reply = d
                    .session("s9", "a")
                    .request("tasks/get", json!({"taskId": task}))
                    .await;
                assert!(reply.message.get("result").is_none(), "{}", reply.message);
                assert!(reply.message["error"].is_object(), "{}", reply.message);
            }
        }

        mod on_a_request_without_the_capability {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn behaves_exactly_as_the_blocking_path() {
                let d = Daemon::start_with(quick());
                let (s1, s2) = (d.session("s1", "a"), d.session("s2", "a"));
                let (_, queued) = held_pool(&s1, &s2).await;
                assert_eq!(queued["outcome"], json!("queued"), "{queued}");
                let waited = s2.call("lease_wait", json!({})).await;
                assert_ne!(waited["resultType"], json!("task"), "{waited}");
                assert!(
                    waited["structuredContent"]["notices"].is_array(),
                    "{waited}"
                );
            }
        }
    }
}
