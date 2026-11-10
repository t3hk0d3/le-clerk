//! `clerk mcp`: the stdio MCP server Claude Code starts, relaying to the
//! daemon's `/mcp` endpoint.
//!
//! - Each stdin line is one JSON-RPC message, sent with `POST` to `/mcp` on its own
//!   connection, concurrently with the others (a blocking `lease_wait`
//!   never holds up another call). JSON and SSE responses are written back
//!   to stdout one message per line, through one writer.
//! - Every POST carries the 2026-07-28 HTTP framing taken from the message
//!   (`MCP-Protocol-Version`, `Mcp-Method`, `Mcp-Name`) and the identity
//!   headers: `X-Clerk-Session` (`CLAUDE_CODE_SESSION_ID`, or `conn:<ULID>`
//!   when unset), `X-Clerk-Connection` (this process's ULID),
//!   `X-Clerk-Project` (the start directory, percent-encoded) and
//!   `X-Clerk-Claude-Pid` (the parent process: Claude Code).
//! - A liveness stream (`GET /v1/session/live`) stays open while this
//!   process lives and is reopened after a daemon restart. Relaying starts
//!   once it is open (or after [`LIVE_FIRST_WAIT`]), so nothing is leased
//!   before the session counts as alive.
//! - The daemon is started when needed, as every clerk command does.
//! - stdin EOF ends the process (exit 0), closing the stream.
//! - stdout carries MCP JSON-RPC only; logs go to stderr.

use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::Context;
use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use leclerk::client::Client;
use leclerk::config::Paths;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};
use ulid::Ulid;

/// The variable Claude Code sets to its session id.
const SESSION_VAR: &str = "CLAUDE_CODE_SESSION_ID";
/// How long relaying waits for the first liveness stream.
const LIVE_FIRST_WAIT: Duration = Duration::from_secs(5);
/// First and longest pause between liveness reconnection attempts.
const LIVE_BACKOFF_MIN: Duration = Duration::from_millis(100);
const LIVE_BACKOFF_MAX: Duration = Duration::from_secs(1);
/// JSON-RPC: the line is not JSON.
const PARSE_ERROR: i64 = -32700;
/// JSON-RPC: the relay could not get an answer.
const INTERNAL_ERROR: i64 = -32603;

/// Run the relay until stdin ends. Errors go to stderr, never stdout.
pub fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("clerk mcp: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<()> {
    let paths = Paths::resolve()?;
    let identity = Identity::current();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(relay(paths, identity));
    // Requests still in flight are abandoned with the process.
    runtime.shutdown_background();
    result
}

/// Who this proxy speaks for, sent on every request.
struct Identity {
    session: String,
    connection: Ulid,
    /// The start directory, percent-encoded; `None` when unreadable.
    project: Option<String>,
    claude_pid: u32,
}

impl Identity {
    fn current() -> Self {
        let connection = Ulid::new();
        let session = std::env::var(SESSION_VAR)
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("conn:{connection}"));
        Self {
            session,
            connection,
            project: std::env::current_dir().ok().map(|d| percent_encode(&d)),
            claude_pid: std::os::unix::process::parent_id(),
        }
    }

    /// Add the `X-Clerk-*` headers to `request`.
    fn apply(&self, mut request: hyper::http::request::Builder) -> hyper::http::request::Builder {
        request = request
            .header("x-clerk-session", &self.session)
            .header("x-clerk-connection", self.connection.to_string())
            .header("x-clerk-claude-pid", self.claude_pid.to_string());
        if let Some(project) = &self.project {
            request = request.header("x-clerk-project", project);
        }
        request
    }
}

/// Percent-encode every byte but unreserved characters and `/`.
fn percent_encode(path: &Path) -> String {
    use std::fmt::Write as _;
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

/// What every relayed message shares.
struct Relay {
    paths: Paths,
    identity: Identity,
    /// The protocol version last seen, for messages that do not name one.
    version: Mutex<Option<String>>,
    /// Lines for stdout.
    out: mpsc::UnboundedSender<String>,
}

impl Relay {
    fn version(&self) -> Option<String> {
        self.version
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn learn_version(&self, version: &str) {
        *self.version.lock().unwrap_or_else(PoisonError::into_inner) = Some(version.to_owned());
    }

    /// Write `message` to stdout as one line; learn the version from an
    /// `initialize` result.
    fn emit(&self, message: &Value) {
        if let Some(v) = message
            .pointer("/result/protocolVersion")
            .and_then(Value::as_str)
        {
            self.learn_version(v);
        }
        // The writer only goes away with the process.
        let _ = self.out.send(message.to_string());
    }

    /// A JSON-RPC error answering `id`.
    fn emit_error(&self, id: &Value, code: i64, message: &str) {
        self.emit(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message},
        }));
    }

    /// A connection to the daemon; started when none answers.
    async fn connect(&self) -> anyhow::Result<UnixStream> {
        if let Ok(stream) = UnixStream::connect(&self.paths.socket).await {
            return Ok(stream);
        }
        Client::ensure_running(&self.paths).await?;
        UnixStream::connect(&self.paths.socket)
            .await
            .context("cannot connect to the clerk daemon")
    }
}

async fn relay(paths: Paths, identity: Identity) -> anyhow::Result<()> {
    if let Err(e) = Client::ensure_running(&paths).await {
        tracing::warn!(error = %e, "the clerk daemon is not available yet");
    }
    let (out, lines_out) = mpsc::unbounded_channel();
    let relay = Arc::new(Relay {
        paths,
        identity,
        version: Mutex::new(None),
        out,
    });
    let writer = tokio::spawn(write_stdout(lines_out));
    let (up, first_up) = oneshot::channel();
    let stream = tokio::spawn(liveness(relay.clone(), up));
    if tokio::time::timeout(LIVE_FIRST_WAIT, first_up)
        .await
        .is_err()
    {
        tracing::warn!("no liveness stream yet; relaying anyway");
    }
    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut line = Vec::new();
    loop {
        line.clear();
        if stdin.read_until(b'\n', &mut line).await? == 0 {
            break;
        }
        let text = String::from_utf8_lossy(&line).trim().to_owned();
        if text.is_empty() {
            continue;
        }
        tokio::spawn(forward(relay.clone(), text));
    }
    tracing::debug!("stdin ended; closing");
    stream.abort();
    writer.abort();
    Ok(())
}

/// Write each line to stdout, flushing after each.
async fn write_stdout(mut lines: mpsc::UnboundedReceiver<String>) {
    let mut stdout = tokio::io::stdout();
    while let Some(mut line) = lines.recv().await {
        line.push('\n');
        if stdout.write_all(line.as_bytes()).await.is_err() || stdout.flush().await.is_err() {
            tracing::warn!("stdout is gone");
            return;
        }
    }
}

/// Keep a liveness stream open, reopening it whenever it ends; `up` fires
/// at the first one.
async fn liveness(relay: Arc<Relay>, up: oneshot::Sender<()>) {
    let mut up = Some(up);
    let mut backoff = LIVE_BACKOFF_MIN;
    loop {
        match open_live(&relay).await {
            Ok(mut body) => {
                if let Some(up) = up.take() {
                    let _ = up.send(());
                }
                backoff = LIVE_BACKOFF_MIN;
                while let Some(frame) = body.frame().await {
                    if frame.is_err() {
                        break;
                    }
                }
                tracing::info!("the liveness stream ended; reconnecting");
            }
            Err(e) => tracing::debug!(error = %e, "cannot open the liveness stream"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(LIVE_BACKOFF_MAX);
    }
}

/// `GET /v1/session/live`; its body once the daemon accepted it. Never
/// starts the daemon: a stopped daemon is started by the next request.
async fn open_live(relay: &Relay) -> anyhow::Result<Incoming> {
    let stream = UnixStream::connect(&relay.paths.socket).await?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(conn);
    let request = relay
        .identity
        .apply(Request::get("/v1/session/live").header("host", "localhost"))
        .body(Empty::<Bytes>::new())?;
    let response = sender.send_request(request).await?;
    if response.status() != StatusCode::OK {
        anyhow::bail!(
            "the daemon refused the liveness stream: {}",
            response.status()
        );
    }
    Ok(response.into_body())
}

/// Relay one stdin line; a request that gets no answer is answered with a
/// JSON-RPC error.
async fn forward(relay: Arc<Relay>, line: String) {
    let message: Value = match serde_json::from_str(&line) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "stdin line is not JSON");
            relay.emit_error(&Value::Null, PARSE_ERROR, "the message is not valid JSON");
            return;
        }
    };
    let request_id = message.get("method").and(message.get("id")).cloned();
    match post(&relay, &message, line).await {
        Ok(true) => {}
        Ok(false) => {
            if let Some(id) = &request_id {
                relay.emit_error(id, INTERNAL_ERROR, "the clerk daemon sent no answer");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "relaying a message failed");
            if let Some(id) = &request_id {
                relay.emit_error(
                    id,
                    INTERNAL_ERROR,
                    &format!("the clerk daemon could not be reached: {e:#}"),
                );
            }
        }
    }
}

/// The `Mcp-Name` of `message`: the tool or prompt name, task id or
/// resource URI its method is about.
fn mcp_name(method: &str, message: &Value) -> Option<String> {
    let pointer = match method {
        "tools/call" | "prompts/get" => "/params/name",
        "tasks/get" | "tasks/update" | "tasks/cancel" | "tasks/result" => "/params/taskId",
        "resources/read" | "resources/subscribe" | "resources/unsubscribe" => "/params/uri",
        _ => return None,
    };
    message
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// POST `body` (the text of `message`) to `/mcp` and emit what comes back.
/// `Ok(false)` when the answer held no message.
async fn post(relay: &Relay, message: &Value, body: String) -> anyhow::Result<bool> {
    if let Some(v) = message
        .pointer("/params/_meta/io.modelcontextprotocol~1protocolVersion")
        .and_then(Value::as_str)
    {
        relay.learn_version(v);
    }
    let stream = relay.connect().await?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(conn);
    let mut request = Request::post("/mcp")
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    if let Some(version) = relay.version() {
        request = request.header("mcp-protocol-version", version);
    }
    if let Some(method) = message.get("method").and_then(Value::as_str) {
        request = request.header("mcp-method", method);
        if let Some(name) = mcp_name(method, message)
            && hyper::header::HeaderValue::from_str(&name).is_ok()
        {
            request = request.header("mcp-name", name);
        }
    }
    let request = relay
        .identity
        .apply(request)
        .body(Full::new(Bytes::from(body)))?;
    let response = sender.send_request(request).await?;
    let sse = response
        .headers()
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|c| c.to_str().ok())
        .is_some_and(|c| c.starts_with("text/event-stream"));
    let mut body = response.into_body();
    if sse {
        return relay_sse(relay, &mut body).await;
    }
    let bytes = body.collect().await?.to_bytes();
    Ok(emit_json(relay, &bytes))
}

/// Emit `bytes` when it is a JSON message; whether it was.
fn emit_json(relay: &Relay, bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    if text.is_empty() {
        return false;
    }
    if let Ok(message) = serde_json::from_str::<Value>(text) {
        relay.emit(&message);
        true
    } else {
        tracing::warn!(body = %text, "the daemon answered with something that is not JSON");
        false
    }
}

/// Emit each `data:` event of an SSE body as it arrives; whether any was a
/// message.
async fn relay_sse(relay: &Relay, body: &mut Incoming) -> anyhow::Result<bool> {
    let mut buffer = String::new();
    let mut any = false;
    while let Some(frame) = body.frame().await {
        let frame = frame?;
        let Some(data) = frame.data_ref() else {
            continue;
        };
        buffer.push_str(&String::from_utf8_lossy(data));
        let buffer_crlf = buffer.replace("\r\n", "\n");
        buffer = buffer_crlf;
        while let Some(end) = buffer.find("\n\n") {
            let event: String = buffer.drain(..end + 2).collect();
            let data: Vec<&str> = event
                .lines()
                .filter_map(|l| l.strip_prefix("data:"))
                .map(|l| l.strip_prefix(' ').unwrap_or(l))
                .collect();
            if !data.is_empty() {
                any |= emit_json(relay, data.join("\n").as_bytes());
            }
        }
    }
    Ok(any)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encode_keeps_unreserved_and_slashes() {
        assert_eq!(percent_encode(Path::new("/a-b_c.d~e/f")), "/a-b_c.d~e/f");
    }

    #[test]
    fn percent_encode_escapes_the_rest() {
        assert_eq!(percent_encode(Path::new("/my proj/%")), "/my%20proj/%25");
    }

    #[test]
    fn mcp_name_of_a_tool_call_is_the_tool() {
        let m = json!({"method": "tools/call", "params": {"name": "lease_wait"}});
        assert_eq!(mcp_name("tools/call", &m).as_deref(), Some("lease_wait"));
    }

    #[test]
    fn mcp_name_of_a_task_get_is_the_task_id() {
        let m = json!({"method": "tasks/get", "params": {"taskId": "t1"}});
        assert_eq!(mcp_name("tasks/get", &m).as_deref(), Some("t1"));
    }

    #[test]
    fn mcp_name_of_server_discover_is_none() {
        assert_eq!(mcp_name("server/discover", &json!({})), None);
    }
}
