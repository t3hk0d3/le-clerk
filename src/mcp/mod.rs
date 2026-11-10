//! The daemon's MCP endpoint: an rmcp Streamable HTTP service at `/mcp`.
//!
//! - Served statelessly: Claude Code speaks MCP 2026-07-28 (`server/discover`,
//!   no `initialize`, no MCP session); clients that negotiate 2025-11-25
//!   through `initialize` are served too, one request at a time.
//! - Identity comes from every request's own headers, never from an MCP
//!   session and never from tool arguments:
//!   - `X-Clerk-Session` (required): the agent session id
//!     (`CLAUDE_CODE_SESSION_ID`, or `conn:<ULID>`);
//!   - `X-Clerk-Connection`: the proxy process's ULID;
//!   - `X-Clerk-Project`: the proxy's start directory, absolute and
//!     percent-encoded, resolved with [`ProjectResolver::resolve`];
//!   - `X-Clerk-Claude-Pid`: the Claude Code process.
//!
//!   A request without `X-Clerk-Session`, or with a malformed `X-Clerk-*`
//!   header, is refused with HTTP 400 and a JSON-RPC `-32600` error before
//!   rmcp sees it.
//! - Tools live in [`tools`].

pub mod tools;

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData as McpError, ServerHandler};
use serde_json::{Value, json};
use thiserror::Error;
use ulid::Ulid;

use crate::engine::EngineHandle;
use crate::model::{Caller, HolderRef, ProjectKey};
use crate::scope::ProjectResolver;

/// The agent session id (required).
pub const SESSION_HEADER: &str = "x-clerk-session";
/// The proxy process's connection ULID.
pub const CONNECTION_HEADER: &str = "x-clerk-connection";
/// The proxy's start directory, absolute and percent-encoded.
pub const PROJECT_HEADER: &str = "x-clerk-project";
/// The Claude Code process id (the proxy's parent).
pub const CLAUDE_PID_HEADER: &str = "x-clerk-claude-pid";

/// The most of a refused request's body read to find its JSON-RPC id.
const REFUSAL_BODY_LIMIT: usize = 1024 * 1024;

/// What the server tells clients at `initialize` / `server/discover`.
const INSTRUCTIONS: &str = "Le Clerk leases shared resources (boards, devices, services) \
    between agents. Call resource_list to see what is registered and lease_acquire before \
    using anything. While you hold or wait for a lease, keep a lease_wait call running.";

/// Who an MCP request acts as, built from its own `X-Clerk-*` headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpIdentity {
    /// The agent session the request acts for (after alias resolution).
    pub session_id: String,
    /// The proxy process, when it said.
    pub connection: Option<Ulid>,
    /// The project of the proxy's start directory, when one resolves.
    pub project: Option<ProjectKey>,
    /// The Claude Code process, when the proxy said.
    pub claude_pid: Option<u32>,
}

impl McpIdentity {
    /// The engine caller: the agent session in its project.
    #[must_use]
    pub fn caller(&self) -> Caller {
        Caller {
            holder: HolderRef::AgentSession {
                session_id: self.session_id.clone(),
            },
            project: self.project.clone(),
            override_reason: None,
        }
    }
}

/// Why a request's identity headers were refused.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IdentityError {
    /// No (or an empty) `X-Clerk-Session`.
    #[error(
        "the request has no X-Clerk-Session header; agents reach the clerk through `clerk mcp`, \
         which sets it"
    )]
    MissingSession,
    /// A header is present but malformed.
    #[error("the {header} header is malformed: {reason}")]
    BadHeader {
        /// The header name.
        header: &'static str,
        /// What is wrong with it.
        reason: &'static str,
    },
}

/// The identity a request claims, before the project and any session alias
/// are resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Claimed {
    session_id: String,
    connection: Option<Ulid>,
    project_dir: Option<PathBuf>,
    claude_pid: Option<u32>,
}

impl Claimed {
    /// Read the `X-Clerk-*` headers.
    fn parse(headers: &HeaderMap) -> Result<Self, IdentityError> {
        let text = |header: &'static str| -> Result<Option<&str>, IdentityError> {
            headers
                .get(header)
                .map(|v| {
                    v.to_str().map_err(|_| IdentityError::BadHeader {
                        header,
                        reason: "not visible ASCII",
                    })
                })
                .transpose()
        };
        let session_id = text(SESSION_HEADER)?
            .filter(|s| !s.is_empty())
            .ok_or(IdentityError::MissingSession)?
            .to_owned();
        let connection = text(CONNECTION_HEADER)?
            .map(|c| {
                Ulid::from_string(c).map_err(|_| IdentityError::BadHeader {
                    header: CONNECTION_HEADER,
                    reason: "not a ULID",
                })
            })
            .transpose()?;
        let claude_pid = text(CLAUDE_PID_HEADER)?
            .map(|p| {
                p.parse().map_err(|_| IdentityError::BadHeader {
                    header: CLAUDE_PID_HEADER,
                    reason: "not a process id",
                })
            })
            .transpose()?;
        let project_dir = text(PROJECT_HEADER)?.map(project_dir).transpose()?;
        Ok(Self {
            session_id,
            connection,
            project_dir,
            claude_pid,
        })
    }

    /// Resolve the project directory and the session id.
    async fn resolve(self, engine: &EngineHandle) -> McpIdentity {
        let project = match self.project_dir {
            Some(dir) => tokio::task::spawn_blocking(move || resolve_project(&dir))
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "project resolution did not finish");
                    None
                }),
            None => None,
        };
        McpIdentity {
            session_id: resolve_session(engine, self.session_id).await,
            connection: self.connection,
            project,
            claude_pid: self.claude_pid,
        }
    }
}

/// The directory an `X-Clerk-Project` value names; it must be absolute.
fn project_dir(value: &str) -> Result<PathBuf, IdentityError> {
    let bad = |reason| IdentityError::BadHeader {
        header: PROJECT_HEADER,
        reason,
    };
    let bytes = percent_decode(value).ok_or(bad("bad percent-encoding"))?;
    let dir = PathBuf::from(OsString::from_vec(bytes));
    if dir.is_absolute() {
        Ok(dir)
    } else {
        Err(bad("not an absolute path"))
    }
}

/// The project of `dir`, or none when it does not resolve (as for the CLI).
fn resolve_project(dir: &std::path::Path) -> Option<ProjectKey> {
    match ProjectResolver::resolve(dir) {
        Ok(project) => Some(project),
        Err(e) => {
            tracing::debug!(error = %e, "no project for the MCP client's directory");
            None
        }
    }
}

/// The session a request's `X-Clerk-Session` stands for.
///
/// The single seam for session aliases: a `SessionStart` hook with
/// `source: "resume"` will alias the proxy's start-up id to the resumed id
/// (phase 3, Task 4). Until then every id stands for itself.
// Async and taking the engine because alias resolution will ask it.
#[allow(clippy::unused_async)]
async fn resolve_session(_engine: &EngineHandle, session_id: String) -> String {
    session_id
}

/// Decode `%XX` escapes; `None` on a malformed escape.
fn percent_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            let hex = std::str::from_utf8(hex).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(out)
}

/// The `/mcp` route for `engine`, to merge into the daemon's router.
pub fn router(engine: EngineHandle) -> Router {
    let handler = ClerkMcp {
        engine: engine.clone(),
    };
    let service = StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(NeverSessionManager::default()),
        StreamableHttpServerConfig::default().with_legacy_session_mode(false),
    );
    Router::new()
        .route_service("/mcp", service)
        .layer(middleware::from_fn_with_state(engine, identify))
}

/// Build the request's [`McpIdentity`] into its extensions, or refuse it.
async fn identify(
    State(engine): State<EngineHandle>,
    mut request: Request,
    next: Next,
) -> Response {
    match Claimed::parse(request.headers()) {
        Ok(claimed) => {
            let identity = claimed.resolve(&engine).await;
            request.extensions_mut().insert(identity);
            next.run(request).await
        }
        Err(e) => refuse(request.into_body(), &e).await,
    }
}

/// HTTP 400 with a JSON-RPC `-32600` error answering the request's id.
async fn refuse(body: Body, error: &IdentityError) -> Response {
    let id = axum::body::to_bytes(body, REFUSAL_BODY_LIMIT)
        .await
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v.get("id").cloned())
        .unwrap_or(Value::Null);
    let message = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": -32600, "message": error.to_string()},
    });
    (StatusCode::BAD_REQUEST, axum::Json(message)).into_response()
}

/// The identity [`identify`] attached to the HTTP request behind `context`.
fn identity_of(context: &RequestContext<RoleServer>) -> Result<McpIdentity, McpError> {
    context
        .extensions
        .get::<axum::http::request::Parts>()
        .and_then(|parts| parts.extensions.get::<McpIdentity>())
        .cloned()
        .ok_or_else(|| McpError::invalid_request(IdentityError::MissingSession.to_string(), None))
}

/// The rmcp handler: tools only.
#[derive(Clone)]
struct ClerkMcp {
    engine: EngineHandle,
}

impl ServerHandler for ClerkMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tasks()
                .build(),
        )
        .with_server_info(Implementation::new("clerk", crate::VERSION))
        .with_instructions(INSTRUCTIONS)
    }

    // The trait method is async; the catalog needs no awaiting.
    #[allow(clippy::unused_async_trait_impl)]
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        identity_of(&context)?;
        Ok(ListToolsResult::with_all_items(tools::catalog()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let identity = identity_of(&context)?;
        let args = request.arguments.map_or_else(|| json!({}), Value::Object);
        tools::call(&self.engine, &identity, &request.name, args)
            .await
            .map(CallToolResponse::Complete)
            .ok_or_else(|| {
                McpError::invalid_params(format!("there is no tool named {:?}", request.name), None)
            })
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn percent_decode_decodes_escapes() {
        assert_eq!(percent_decode("/a%20b/%2Fc").unwrap(), b"/a b//c");
    }

    #[test]
    fn percent_decode_refuses_a_truncated_escape() {
        assert_eq!(percent_decode("/a%2"), None);
    }

    #[test]
    fn parse_requires_a_session() {
        assert_eq!(
            Claimed::parse(&headers(&[(PROJECT_HEADER, "/tmp")])),
            Err(IdentityError::MissingSession)
        );
    }

    #[test]
    fn parse_refuses_an_empty_session() {
        assert_eq!(
            Claimed::parse(&headers(&[(SESSION_HEADER, "")])),
            Err(IdentityError::MissingSession)
        );
    }

    #[test]
    fn parse_refuses_a_relative_project() {
        let err = Claimed::parse(&headers(&[(SESSION_HEADER, "s"), (PROJECT_HEADER, "rel")]));
        assert!(
            matches!(err, Err(IdentityError::BadHeader { header, .. }) if header == PROJECT_HEADER)
        );
    }

    #[test]
    fn parse_refuses_a_bad_connection() {
        let err = Claimed::parse(&headers(&[(SESSION_HEADER, "s"), (CONNECTION_HEADER, "x")]));
        assert!(
            matches!(err, Err(IdentityError::BadHeader { header, .. }) if header == CONNECTION_HEADER)
        );
    }

    #[test]
    fn parse_reads_every_header() {
        let ulid = Ulid::new();
        let claimed = Claimed::parse(&headers(&[
            (SESSION_HEADER, "s1"),
            (CONNECTION_HEADER, &ulid.to_string()),
            (PROJECT_HEADER, "/home/me/my%20repo"),
            (CLAUDE_PID_HEADER, "42"),
        ]))
        .unwrap();
        assert_eq!(
            claimed,
            Claimed {
                session_id: "s1".into(),
                connection: Some(ulid),
                project_dir: Some(PathBuf::from("/home/me/my repo")),
                claude_pid: Some(42),
            }
        );
    }
}
