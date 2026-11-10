//! Throwaway spike: stdio <-> Streamable HTTP relay over a Unix socket.
//! Each stdin line is POSTed to /mcp; JSON or SSE responses are written back
//! to stdout one message per line. Adds X-Clerk-* identity headers, echoes
//! Mcp-Session-Id (legacy) and sets MCP-Protocol-Version / Mcp-Method /
//! Mcp-Name (SEP-2243) from the message itself. Logs everything.
//!
//! Usage: spike-relay <socket> <log-dir>
use std::{
    io::Write,
    sync::{Arc, Mutex},
};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};

struct State {
    sock: String,
    session: String,
    connection: String,
    project: String,
    proto: Mutex<Option<String>>,
    mcp_sid: Mutex<Option<String>>,
    log: Mutex<std::fs::File>,
    out: Mutex<std::io::Stdout>,
}

impl State {
    fn log(&self, s: &str) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        let _ = writeln!(self.log.lock().unwrap(), "{now:.3} {s}");
    }
    fn emit(&self, line: &str) {
        // Learn the negotiated version from an initialize result.
        if let Ok(v) = serde_json::from_str::<Value>(line)
            && let Some(p) = v.pointer("/result/protocolVersion").and_then(Value::as_str)
        {
            *self.proto.lock().unwrap() = Some(p.to_owned());
        }
        self.log(&format!("OUT {line}"));
        let mut o = self.out.lock().unwrap();
        let _ = writeln!(o, "{line}");
        let _ = o.flush();
    }
}

async fn forward(st: Arc<State>, line: String) -> anyhow::Result<()> {
    let v: Value = serde_json::from_str(&line)?;
    let method = v.get("method").and_then(Value::as_str).map(str::to_owned);
    if let Some(p) = v
        .pointer("/params/_meta/io.modelcontextprotocol~1protocolVersion")
        .and_then(Value::as_str)
    {
        *st.proto.lock().unwrap() = Some(p.to_owned());
    }
    let stream = tokio::net::UnixStream::connect(&st.sock).await?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(conn);
    let mut b = Request::post("/mcp")
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("x-clerk-session", &st.session)
        .header("x-clerk-connection", &st.connection)
        .header("x-clerk-project", &st.project);
    if let Some(p) = st.proto.lock().unwrap().clone() {
        b = b.header("mcp-protocol-version", p);
    }
    if let Some(s) = st.mcp_sid.lock().unwrap().clone() {
        b = b.header("mcp-session-id", s);
    }
    if let Some(m) = &method {
        b = b.header("mcp-method", m.as_str());
        let name = match m.as_str() {
            "tools/call" | "prompts/get" => v.pointer("/params/name"),
            "tasks/get" | "tasks/update" | "tasks/cancel" => v.pointer("/params/taskId"),
            "resources/read" | "resources/subscribe" | "resources/unsubscribe" => {
                v.pointer("/params/uri")
            }
            _ => None,
        };
        if let Some(n) = name.and_then(Value::as_str) {
            b = b.header("mcp-name", n);
        }
    }
    let resp = sender.send_request(b.body(Full::new(Bytes::from(line)))?).await?;
    let status = resp.status();
    if let Some(sid) = resp.headers().get("mcp-session-id") {
        *st.mcp_sid.lock().unwrap() = Some(sid.to_str()?.to_owned());
    }
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|c| c.to_str().ok())
        .unwrap_or("")
        .to_owned();
    st.log(&format!("HTTP {status} ct={ct} for {method:?}"));
    let mut body = resp.into_body();
    if ct.starts_with("text/event-stream") {
        let mut buf = String::new();
        while let Some(frame) = body.frame().await {
            let frame = frame?;
            if let Some(d) = frame.data_ref() {
                buf.push_str(&String::from_utf8_lossy(d));
                while let Some(i) = buf.find("\n\n") {
                    let ev: String = buf.drain(..i + 2).collect();
                    let data: Vec<&str> = ev
                        .lines()
                        .filter_map(|l| l.strip_prefix("data:"))
                        .map(str::trim_start)
                        .collect();
                    let data = data.join("\n");
                    if data.is_empty() {
                        st.log(&format!("SSE non-data event {ev:?}"));
                    } else {
                        st.emit(&data);
                    }
                }
            }
        }
    } else {
        let bytes = body.collect().await?.to_bytes();
        let text = String::from_utf8_lossy(&bytes).trim().to_owned();
        if !text.is_empty() {
            if text.starts_with('{') || text.starts_with('[') {
                st.emit(&text);
            } else {
                st.log(&format!("non-JSON body {text:?}"));
            }
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let pid = std::process::id();
    let dir = &args[2];
    let env_line = serde_json::json!({
        "pid": pid,
        "ppid": std::os::unix::process::parent_id(),
        "CLAUDE_CODE_SESSION_ID": std::env::var("CLAUDE_CODE_SESSION_ID").ok(),
        "CLAUDE_PID": std::env::var("CLAUDE_PID").ok(),
        "cwd": std::env::current_dir()?.display().to_string(),
        "claude_env": std::env::vars().filter(|(k,_)| k.starts_with("CLAUDE")).map(|(k,_)| k).collect::<Vec<_>>(),
    });
    {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(format!("{dir}/relay-env.jsonl"))?;
        writeln!(f, "{env_line}")?;
    }
    let st = Arc::new(State {
        sock: args[1].clone(),
        session: std::env::var("CLAUDE_CODE_SESSION_ID").unwrap_or_else(|_| format!("conn:{pid}")),
        connection: format!("conn-{pid}"),
        project: std::env::current_dir()?.display().to_string(),
        proto: Mutex::new(None),
        mcp_sid: Mutex::new(None),
        log: Mutex::new(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(format!("{dir}/relay-{pid}.log"))?,
        ),
        out: Mutex::new(std::io::stdout()),
    });
    st.log(&format!("start {env_line}"));
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut first = true;
    while let Some(line) = lines.next_line().await? {
        st.log(&format!("IN {line}"));
        let st2 = st.clone();
        // Serialise the very first message (initialize) so the session id /
        // version is known before anything else goes out.
        if first {
            first = false;
            if let Err(e) = forward(st2, line).await {
                st.log(&format!("forward error {e:#}"));
            }
            continue;
        }
        tokio::spawn(async move {
            if let Err(e) = forward(st2.clone(), line).await {
                st2.log(&format!("forward error {e:#}"));
            }
        });
    }
    st.log("stdin EOF");
    Ok(())
}
