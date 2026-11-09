//! The JSON RPC surface over HTTP (axum), served on any listener.
//!
//! - `GET /v1/version` → `{"version": "<semver>", "pid": <daemon pid>}`
//! - `POST /v1/rpc/{method}` with `{"caller": Caller, "args": {...}}` →
//!   `200 {"result": ...}` or an error status with
//!   `{"error": {"code": "...", "message": "..."}}`
//! - `GET /v1/notices?holder=<url-encoded HolderRef JSON>&wait=<secs, max 60>`
//!   → `200 {"result": [Notice, ...]}` (the long-poll). Optional
//!   `leases=<id,id…>` and `requests=<id,id…>` narrow it to notices naming
//!   one of those; the holder's other notices stay pending.
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

use crate::engine::{ApiError, Command, EngineHandle, ErrorKind, MAX_WAIT};
use crate::model::{Caller, HolderRef};
use crate::store::notices::NoticeFilter;

/// What `GET /v1/version` reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    /// The version the server reports (the crate version, unless a test
    /// overrides it).
    pub version: String,
    /// The serving process.
    pub pid: u32,
}

impl ServerInfo {
    /// This process at the crate version.
    #[must_use]
    pub fn current() -> Self {
        Self {
            version: crate::VERSION.to_owned(),
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
    let version = json!({ "version": info.version, "pid": info.pid });
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
}

async fn rpc(
    State(engine): State<EngineHandle>,
    Path(method): Path<String>,
    body: Bytes,
) -> Response {
    if !Command::METHODS.contains(&method.as_str()) {
        return ApiError::unknown_method(&method).into_response();
    }
    let request: RpcRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return ApiError::invalid_request(format!("request body is not valid: {e}"))
                .into_response();
        }
    };
    let command = match Command::parse(&method, request.caller, request.args) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    match engine.call(command).await {
        Ok(result) => ok(result),
        Err(e) => e.into_response(),
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
    let wait = Duration::from_secs(query.wait).min(MAX_WAIT);
    match engine.wait_notices_matching(holder, filter, wait).await {
        Ok(list) => ok(list),
        Err(e) => e.into_response(),
    }
}
