//! The daemon's MCP endpoint (`/mcp`), driven over its Unix socket by rmcp's
//! own client (2025-11-25, through `initialize`) and by a minimal 2026-07-28
//! client (`server/discover`, per-request `_meta` and headers), both sending
//! the `X-Clerk-*` identity headers directly (no proxy).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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
        let dir = TempDir::new().unwrap();
        let engine = Engine::spawn(
            Store::open_in_memory().unwrap(),
            Arc::new(ManualClock::new(T0)),
            Config::default(),
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
        }
    }

    /// The same identity, speaking 2025-11-25 (no per-request `_meta`).
    fn legacy(mut self) -> Self {
        self.version = "2025-11-25";
        self
    }

    async fn request(&self, method: &str, mut params: Value) -> Reply {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if self.version == "2026-07-28" {
            params["_meta"] = json!({
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientInfo": {"name": "mcp-test", "version": "0"},
                "io.modelcontextprotocol/clientCapabilities": {},
            });
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
        if let Some(name) = params.get("name").and_then(Value::as_str) {
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
        let message = if sse {
            text.split("\n\n")
                .filter_map(|ev| {
                    let data: Vec<&str> = ev
                        .lines()
                        .filter_map(|l| l.strip_prefix("data:"))
                        .map(str::trim_start)
                        .collect();
                    serde_json::from_str::<Value>(&data.join("\n")).ok()
                })
                .find(|m| m.get("id") == Some(&json!(id)))
                .unwrap_or_else(|| panic!("no response for {id} in {text:?}"))
        } else {
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {status} {text:?}"))
        };
        Reply { status, message }
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
            let reply = first
                .request(
                    "initialize",
                    json!({
                        "protocolVersion": "2025-11-25",
                        "capabilities": {},
                        "clientInfo": {"name": "mcp-test", "version": "0"},
                    }),
                )
                .await;
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
