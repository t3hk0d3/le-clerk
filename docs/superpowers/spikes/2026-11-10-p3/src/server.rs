//! Throwaway spike: rmcp Streamable HTTP server nested in an axum router,
//! served over a Unix socket. One introspection tool (`whoami`) and one slow
//! tool (`slow`) that sends progress notifications; `slow` materialises a
//! SEP-2663 task when the client declares the tasks extension.
//!
//! Usage: spike-server <socket> <log-file> [--legacy]
use std::{fs::OpenOptions, io::Write, sync::Arc, time::Duration};

use axum::{Router, extract::Request, middleware::Next, response::Response, routing::get};
use rmcp::{
    ErrorData as McpError, ServerHandler,
    model::*,
    service::{RequestContext, RoleServer},
    task_manager::{TaskManager, TaskOptions},
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde_json::{Value, json};

static LOG: std::sync::OnceLock<std::sync::Mutex<std::fs::File>> = std::sync::OnceLock::new();

fn log(line: impl AsRef<str>) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let mut f = LOG.get().unwrap().lock().unwrap();
    let _ = writeln!(f, "{now:.3} {}", line.as_ref());
}

#[derive(Clone)]
struct Spike {
    tasks: TaskManager,
}

fn headers_of(ctx: &RequestContext<RoleServer>) -> Value {
    let Some(parts) = ctx.extensions.get::<http::request::Parts>() else {
        return json!(null);
    };
    let mut m = serde_json::Map::new();
    for (k, v) in &parts.headers {
        m.insert(k.to_string(), json!(v.to_str().unwrap_or("?")));
    }
    Value::Object(m)
}

fn schema(v: Value) -> Arc<JsonObject> {
    Arc::new(v.as_object().unwrap().clone())
}

impl ServerHandler for Spike {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tasks()
                .build(),
        )
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, McpError> {
        log(format!(
            "initialize params={} headers={}",
            serde_json::to_string(&request).unwrap(),
            headers_of(&context)
        ));
        context.peer.set_peer_info(request.clone());
        self.negotiate_initialize(&request)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        log(format!(
            "tools/list proto={:?} caps={} headers={}",
            context.protocol_version(),
            serde_json::to_string(&context.client_capabilities()).unwrap(),
            headers_of(&context)
        ));
        Ok(ListToolsResult::with_all_items(vec![
            Tool::new(
                "whoami",
                "Report what the server sees about this connection.",
                schema(json!({"type":"object","properties":{}})),
            ),
            Tool::new(
                "slow",
                "Wait `seconds` seconds, sending a progress notification every `every` seconds (0 = none), then return 'done'.",
                schema(json!({"type":"object","properties":{
                    "seconds":{"type":"number"},"every":{"type":"number"},"block":{"type":"boolean","description":"true = never run as a task"}},
                    "required":["seconds"]})),
            ),
        ]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let caps = context.client_capabilities();
        let tasks = caps.as_ref().is_some_and(|c| c.supports_tasks());
        log(format!(
            "tools/call name={} args={} proto={:?} client_tasks={} meta={} headers={}",
            request.name,
            serde_json::to_string(&request.arguments).unwrap(),
            context.protocol_version(),
            tasks,
            serde_json::to_string(&context.meta).unwrap(),
            headers_of(&context)
        ));
        match request.name.as_ref() {
            "whoami" => {
                let body = json!({
                    "headers": headers_of(&context),
                    "protocol_version": format!("{:?}", context.protocol_version()),
                    "client_info": context.client_info(),
                    "client_capabilities": caps,
                });
                Ok(CallToolResponse::Complete(CallToolResult::success(vec![
                    ContentBlock::text(body.to_string()),
                ])))
            }
            "slow" => {
                let args = request.arguments.clone().unwrap_or_default();
                let secs = args.get("seconds").and_then(Value::as_f64).unwrap_or(5.0);
                let every = args.get("every").and_then(Value::as_f64).unwrap_or(0.0);
                let force_block = matches!(args.get("block"), Some(Value::Bool(true))) || args.get("block").and_then(Value::as_str) == Some("true");
                if tasks && !force_block {
                    let task = self.tasks.spawn(
                        TaskOptions::new().with_ttl_ms(3_600_000u64),
                        move |_ctx| {
                            Box::pin(async move {
                                log(format!("task started secs={secs}"));
                                tokio::time::sleep(Duration::from_secs_f64(secs)).await;
                                log("task done");
                                Ok(CallToolResult::success(vec![ContentBlock::text(
                                    format!("done (task) after {secs}s"),
                                )]))
                            })
                        },
                    );
                    log(format!("created task {}", task.task_id));
                    return Ok(CallToolResponse::Task(CreateTaskResult::new(task)));
                }
                let token = context.meta.get_progress_token();
                log(format!("slow blocking secs={secs} every={every} token={token:?}"));
                let start = tokio::time::Instant::now();
                let end = start + Duration::from_secs_f64(secs);
                let mut n = 0.0;
                loop {
                    let now = tokio::time::Instant::now();
                    if now >= end {
                        break;
                    }
                    let step = if every > 0.0 {
                        Duration::from_secs_f64(every).min(end - now)
                    } else {
                        end - now
                    };
                    tokio::select! {
                        () = tokio::time::sleep(step) => {}
                        () = context.ct.cancelled() => {
                            log("slow cancelled by client");
                            return Err(McpError::internal_error("cancelled", None));
                        }
                    }
                    if every > 0.0
                        && let Some(t) = &token
                    {
                        n += 1.0;
                        let r = context
                            .peer
                            .notify_progress(
                                ProgressNotificationParam::new(t.clone(), n)
                                    .with_message(format!("waiting {n}")),
                            )
                            .await;
                        log(format!("progress {n} sent: {r:?}"));
                    }
                }
                log(format!("slow done after {:?}", start.elapsed()));
                Ok(CallToolResponse::Complete(CallToolResult::success(vec![
                    ContentBlock::text(format!("done after {secs}s")),
                ])))
            }
            _ => Err(McpError::invalid_params("unknown tool", None)),
        }
    }

    async fn get_task(
        &self,
        request: GetTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetTaskResult, McpError> {
        let t = self.tasks.get_task(&request.task_id);
        log(format!("tasks/get {} -> {:?}", request.task_id, t.as_ref().map(|t| t.status())));
        Ok(GetTaskResult::new(t?))
    }

    async fn cancel_task(
        &self,
        request: CancelTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        log(format!("tasks/cancel {}", request.task_id));
        self.tasks.cancel_task(&request.task_id)
    }
}

async fn log_http(req: Request, next: Next) -> Response {
    let mut hs = String::new();
    for (k, v) in req.headers() {
        hs.push_str(&format!("{k}={} ", v.to_str().unwrap_or("?")));
    }
    log(format!("HTTP {} {} {hs}", req.method(), req.uri()));
    let resp = next.run(req).await;
    log(format!(
        "HTTP -> {} ct={:?} sid={:?}",
        resp.status(),
        resp.headers().get("content-type"),
        resp.headers().get("mcp-session-id")
    ));
    resp
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let sock = args[1].clone();
    LOG.set(std::sync::Mutex::new(
        OpenOptions::new().create(true).append(true).open(&args[2])?,
    ))
    .unwrap();
    let legacy = args.iter().any(|a| a == "--legacy");
    let spike = Spike { tasks: TaskManager::new() };
    let mcp = StreamableHttpService::new(
        move || Ok(spike.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default().with_legacy_session_mode(legacy),
    );
    let app = Router::new()
        .route("/v1/version", get(|| async { "spike" }))
        .nest_service("/mcp", mcp)
        .layer(axum::middleware::from_fn(log_http));
    let _ = std::fs::remove_file(&sock);
    let listener = tokio::net::UnixListener::bind(&sock)?;
    log(format!("listening on {sock} legacy={legacy}"));
    axum::serve(listener, app).await?;
    Ok(())
}
