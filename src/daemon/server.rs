//! The JSON RPC surface over HTTP (axum), served on any listener.
//!
//! - `GET /v1/version` → `{"version": "<semver>", "protocol": <n>, "pid":
//!   <daemon pid>}` (see [`crate::PROTOCOL`])
//! - `POST /v1/rpc/{method}` with `{"caller": Caller, "args": {...}}` →
//!   `200 {"result": ...}` or an error status with
//!   `{"error": {"code": "...", "message": "..."}}`. The hook methods
//!   ([`HookCommand::METHODS`]) take the session from their arguments.
//! - `GET /v1/notices?holder=<url-encoded HolderRef JSON>&wait=<secs, max 60>`
//!   → `200 {"result": [Notice, ...]}` (the long-poll). Optional
//!   `leases=<id,id…>` and `requests=<id,id…>` narrow it to notices naming
//!   one of those; the holder's other notices stay pending.
//!
//! - `GET /v1/agent/wait?session=<id>&host=<Host JSON>&wait=<secs, max
//!   60>&first=<bool>&arm=<token>` → `200 {"result": {"status": "notices", "notices":
//!   [...]} | {"status": "timeout" | "armed" | "ended"}}`: one step of an
//!   agent session's `clerk wait` (see [`EngineHandle::agent_wait`]); the
//!   notices stay pending until acknowledged.
//! - `POST /v1/agent/ack` with `{"session": "<id>", "ids": [...]}` →
//!   `200 {"result": <count marked delivered>}`.
//!
//! Status mapping: 404 not found or unknown method, 409 conflict/refused,
//! 422 invalid request or argument, 500 internal, 503 engine stopped.

use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::engine::{ApiError, Command, EngineHandle, ErrorKind, HookCommand, MAX_WAIT};
use crate::model::{Caller, HolderRef, Host, NoticeId};
use crate::store::notices::NoticeFilter;

/// What `GET /v1/version` reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    /// The version the server reports (the crate version, unless a test
    /// overrides it).
    pub version: String,
    /// The protocol the server reports ([`crate::PROTOCOL`], unless a test
    /// overrides it; `None` leaves it out, as daemons before protocols).
    pub protocol: Option<u32>,
    /// The serving process.
    pub pid: u32,
}

impl ServerInfo {
    /// This process at the crate version.
    #[must_use]
    pub fn current() -> Self {
        Self {
            version: crate::VERSION.to_owned(),
            protocol: Some(crate::PROTOCOL),
            pid: std::process::id(),
        }
    }
}

/// The router for `engine`, reporting [`ServerInfo::current`].
pub fn router(engine: EngineHandle) -> Router {
    router_with(engine, &ServerInfo::current())
}

/// The router for `engine`, reporting `info` on `/v1/version`.
pub fn router_with(engine: EngineHandle, info: &ServerInfo) -> Router {
    let mut version = json!({ "version": info.version, "pid": info.pid });
    if let Some(protocol) = info.protocol {
        version["protocol"] = json!(protocol);
    }
    Router::new()
        .route(
            "/v1/version",
            get(move || {
                let body = version.clone();
                async move { axum::Json(body).into_response() }
            }),
        )
        .route("/v1/rpc/{method}", post(rpc))
        .route("/v1/notices", get(notices))
        .route("/v1/agent/wait", get(agent_wait))
        .route("/v1/agent/ack", post(agent_ack))
        .with_state(engine)
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self.kind {
            ErrorKind::NotFound => StatusCode::NOT_FOUND,
            ErrorKind::Conflict => StatusCode::CONFLICT,
            ErrorKind::Invalid => StatusCode::UNPROCESSABLE_ENTITY,
            ErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            ErrorKind::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        };
        (status, axum::Json(json!({ "error": self }))).into_response()
    }
}

fn ok(result: impl serde::Serialize) -> Response {
    (StatusCode::OK, axum::Json(json!({ "result": result }))).into_response()
}

/// The body of `POST /v1/rpc/{method}`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RpcRequest {
    caller: Caller,
    #[serde(default)]
    args: Value,
    /// The process running the calling agent session, if it is one.
    #[serde(default)]
    host: Option<Host>,
}

async fn rpc(
    State(engine): State<EngineHandle>,
    Path(method): Path<String>,
    body: Bytes,
) -> Response {
    let hook = HookCommand::METHODS.contains(&method.as_str());
    if !hook && !Command::METHODS.contains(&method.as_str()) {
        return ApiError::unknown_method(&method).into_response();
    }
    let request: RpcRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return ApiError::invalid_request(format!("request body is not valid: {e}"))
                .into_response();
        }
    };
    if hook {
        // Hooks answer about the session their arguments resolve to; the
        // caller is not used.
        let result = match HookCommand::parse(&method, request.args) {
            Ok(command) => engine.hook(command).await,
            Err(e) => Err(e),
        };
        return match result {
            Ok(value) => ok(value),
            Err(e) => e.into_response(),
        };
    }
    if let Err(e) = report(&engine, &request.caller.holder, request.host).await {
        return e.into_response();
    }
    let command = match Command::parse(&method, request.caller, request.args) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    match engine.call(command).await {
        Ok(result) => ok(result),
        Err(e) => e.into_response(),
    }
}

/// Record `host` as the process running `holder`, when both are an agent
/// session and its host.
async fn report(
    engine: &EngineHandle,
    holder: &HolderRef,
    host: Option<Host>,
) -> Result<(), ApiError> {
    match (holder, host) {
        (HolderRef::AgentSession { session_id }, Some(host)) => {
            engine.report_host(session_id.clone(), host).await
        }
        _ => Ok(()),
    }
}

/// The query of `GET /v1/notices`.
#[derive(Deserialize)]
struct NoticesQuery {
    /// The holder's JSON encoding.
    holder: String,
    /// Seconds to wait; capped at 60.
    #[serde(default)]
    wait: u64,
    /// Comma-separated lease ids: with `requests`, the filter.
    #[serde(default)]
    leases: Option<String>,
    /// Comma-separated request ids: with `leases`, the filter.
    #[serde(default)]
    requests: Option<String>,
    /// The JSON encoding of the waiting agent session's host.
    #[serde(default)]
    host: Option<String>,
}

/// Comma-separated ids; empty entries are skipped.
fn ids<T: serde::de::DeserializeOwned>(list: Option<&str>) -> Result<Vec<T>, ApiError> {
    list.into_iter()
        .flat_map(|l| l.split(','))
        .filter(|id| !id.is_empty())
        .map(|id| {
            serde_json::from_value(Value::String(id.to_owned()))
                .map_err(|e| ApiError::invalid_request(format!("bad id {id:?}: {e}")))
        })
        .collect()
}

/// The filter a notices query asks for; `None` without `leases` and
/// `requests`.
fn filter_of(query: &NoticesQuery) -> Result<Option<NoticeFilter>, ApiError> {
    if query.leases.is_none() && query.requests.is_none() {
        return Ok(None);
    }
    Ok(Some(NoticeFilter {
        leases: ids(query.leases.as_deref())?,
        requests: ids(query.requests.as_deref())?,
    }))
}

async fn notices(
    State(engine): State<EngineHandle>,
    query: Result<Query<NoticesQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(q) => q,
        Err(e) => return ApiError::invalid_request(e.body_text()).into_response(),
    };
    let holder: HolderRef = match serde_json::from_str(&query.holder) {
        Ok(h) => h,
        Err(e) => {
            return ApiError::invalid_request(format!("holder is not a valid HolderRef: {e}"))
                .into_response();
        }
    };
    let filter = match filter_of(&query) {
        Ok(f) => f,
        Err(e) => return e.into_response(),
    };
    let host: Option<Host> = match query.host.as_deref().map(serde_json::from_str).transpose() {
        Ok(h) => h,
        Err(e) => {
            return ApiError::invalid_request(format!("host is not a valid Host: {e}"))
                .into_response();
        }
    };
    if let Err(e) = report(&engine, &holder, host).await {
        return e.into_response();
    }
    let wait = Duration::from_secs(query.wait).min(MAX_WAIT);
    match engine.wait_notices_matching(holder, filter, wait).await {
        Ok(list) => ok(list),
        Err(e) => e.into_response(),
    }
}

/// The query of `GET /v1/agent/wait`.
#[derive(Deserialize)]
struct AgentWaitQuery {
    session: String,
    /// The JSON encoding of the waiting process.
    host: String,
    #[serde(default)]
    wait: u64,
    #[serde(default)]
    first: bool,
    /// The wait's arm token.
    #[serde(default)]
    arm: String,
}

async fn agent_wait(
    State(engine): State<EngineHandle>,
    query: Result<Query<AgentWaitQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(q) => q,
        Err(e) => return ApiError::invalid_request(e.body_text()).into_response(),
    };
    let host: Host = match serde_json::from_str(&query.host) {
        Ok(h) => h,
        Err(e) => {
            return ApiError::invalid_request(format!("host is not a valid Host: {e}"))
                .into_response();
        }
    };
    let wait = Duration::from_secs(query.wait);
    match engine
        .agent_wait(query.session, host, wait, query.first, query.arm)
        .await
    {
        Ok(answer) => ok(answer),
        Err(e) => e.into_response(),
    }
}

/// The body of `POST /v1/agent/ack`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentAck {
    session: String,
    ids: Vec<NoticeId>,
}

async fn agent_ack(State(engine): State<EngineHandle>, body: Bytes) -> Response {
    let ack: AgentAck = match serde_json::from_slice(&body) {
        Ok(a) => a,
        Err(e) => {
            return ApiError::invalid_request(format!("request body is not valid: {e}"))
                .into_response();
        }
    };
    let holder = HolderRef::AgentSession {
        session_id: ack.session,
    };
    match engine.ack_notices(holder, ack.ids).await {
        Ok(n) => ok(n),
        Err(e) => e.into_response(),
    }
}
