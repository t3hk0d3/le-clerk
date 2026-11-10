//! Waiting for a session's notices: the blocking `lease_wait`, MCP tasks
//! (SEP-2663 `io.modelcontextprotocol/tasks`) and the open-wait count.
//!
//! - **Acknowledged delivery.** MCP owns its session's notices. A wait
//!   returns them through [`EngineHandle::return_notices`], which leaves
//!   them pending; only `lease_wait`'s `ack` marks them delivered. A result
//!   the client dropped (Claude Code never cancels a call it gave up on) is
//!   therefore returned again by the next `lease_wait`.
//! - **Blocking.** With nothing pending a wait blocks, sending a progress
//!   notification every `mcp_progress_ms` when the request carries a
//!   `progressToken`, and gives up with an empty batch after
//!   `mcp_wait_cap_ms`. A client that goes away (its HTTP stream dropped)
//!   ends the wait at once; nothing is delivered.
//! - **Tasks.** Only for a request whose per-request client capabilities
//!   declare the tasks extension. A task belongs to the session that made
//!   it: another session's `tasks/get` or `tasks/cancel` reads as an unknown
//!   task.
//! - **Open waits.** A blocking wait and an open task each hold an
//!   [`OpenWait`](crate::engine::OpenWait) for the session while they run.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use rmcp::ErrorData as McpError;
use rmcp::model::{
    CallToolResult, CreateTaskResult, GetTaskResult, ProgressNotificationParam, ProgressToken,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::task_manager::{TaskExit, TaskManager, TaskOptions};

use crate::engine::{ApiError, EngineHandle};
use crate::mcp::McpIdentity;
use crate::model::{HolderRef, RequestId};
use crate::store::notices::{Notice, NoticeFilter, NoticeKind};

/// How a wait for a batch of notices ended.
#[derive(Debug)]
pub enum Batch {
    /// The session's pending notices (never empty).
    Notices(Vec<Notice>),
    /// Nothing arrived within the cap.
    Empty,
    /// The client went away; nothing was delivered.
    Gone,
}

/// What a tool call can use beyond its arguments: the live request (for
/// progress and disconnects) and, when the client declared the extension,
/// the task store.
#[derive(Clone, Default)]
pub struct Live {
    context: Option<RequestContext<RoleServer>>,
    tasks: Option<Tasks>,
}

impl Live {
    /// The request behind `context`; `tasks` only when its client
    /// capabilities declare the tasks extension.
    #[must_use]
    pub fn new(context: RequestContext<RoleServer>, tasks: &Tasks) -> Self {
        let tasks = context
            .client_capabilities()
            .is_some_and(|c| c.supports_tasks())
            .then(|| tasks.clone());
        Self {
            context: Some(context),
            tasks,
        }
    }

    /// The task store, when this request may become a task.
    #[must_use]
    pub fn tasks(&self) -> Option<&Tasks> {
        self.tasks.as_ref()
    }

    fn progress_token(&self) -> Option<ProgressToken> {
        self.context.as_ref()?.meta.get_progress_token()
    }

    /// Notification `n` of a blocked wait, when the request asked for them.
    async fn progress(&self, n: u32) {
        let (Some(context), Some(token)) = (&self.context, self.progress_token()) else {
            return;
        };
        let param = ProgressNotificationParam::new(token, f64::from(n))
            .with_message("still waiting for notices");
        if let Err(e) = context.peer.notify_progress(param).await {
            tracing::debug!(error = %e, "cannot send a progress notification");
        }
    }

    /// Resolves when the client goes away; never without a request.
    async fn gone(&self) {
        match &self.context {
            Some(context) => context.ct.cancelled().await,
            None => std::future::pending().await,
        }
    }
}

/// Wait for a batch of `holder`'s pending notices: at once when any are
/// pending (acknowledged or not), else until one arrives, the cap passes
/// or the client goes away. The notices returned stay pending.
///
/// # Errors
/// A storage failure, or `engine_unavailable`.
pub async fn next_batch(
    engine: &EngineHandle,
    holder: &HolderRef,
    live: &Live,
) -> Result<Batch, ApiError> {
    let cap = Duration::from_millis(engine.config().mcp_wait_cap_ms);
    let every = Duration::from_millis(engine.config().mcp_progress_ms);
    let deadline = Instant::now() + cap;
    let wait = async {
        let mut sent = 0;
        loop {
            let slice = deadline
                .saturating_duration_since(Instant::now())
                .min(every);
            let got = engine
                .wait_until(holder, slice, || async {
                    let notices = engine.return_notices(holder.clone(), None).await?;
                    Ok((!notices.is_empty()).then_some(notices))
                })
                .await?;
            if let Some(notices) = got {
                return Ok(Batch::Notices(notices));
            }
            if Instant::now() >= deadline {
                return Ok(Batch::Empty);
            }
            sent += 1;
            live.progress(sent).await;
        }
    };
    tokio::select! {
        batch = wait => batch,
        () = live.gone() => Ok(Batch::Gone),
    }
}

/// Wait, up to the cap, for what ends queued request `request` of `holder`:
/// its `offered` or `queue_dropped` notice (left pending). `None` when the
/// cap passes first.
///
/// # Errors
/// A storage failure, or `engine_unavailable`.
pub async fn request_outcome(
    engine: &EngineHandle,
    holder: &HolderRef,
    request: &RequestId,
) -> Result<Option<Notice>, ApiError> {
    let cap = Duration::from_millis(engine.config().mcp_wait_cap_ms);
    let filter = NoticeFilter {
        leases: Vec::new(),
        requests: vec![request.clone()],
    };
    engine
        .wait_until(holder, cap, || async {
            let notices = engine
                .return_notices(holder.clone(), Some(filter.clone()))
                .await?;
            Ok(notices
                .into_iter()
                .find(|n| matches!(n.kind, NoticeKind::Offered | NoticeKind::QueueDropped)))
        })
        .await
}

/// How long the clerk keeps a task observable after it was made.
const TASK_TTL_FACTOR: u64 = 3;

/// The session that made a task, and when.
struct Owner {
    session: String,
    at: Instant,
}

/// The daemon's MCP tasks, each tied to the session that made it.
#[derive(Clone, Default)]
pub struct Tasks {
    manager: TaskManager,
    owners: Arc<Mutex<HashMap<String, Owner>>>,
}

impl Tasks {
    fn owners(&self) -> MutexGuard<'_, HashMap<String, Owner>> {
        // A plain map; a panic while locked leaves nothing to repair.
        self.owners.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Run `work` as a task of `identity`'s session that counts as an open
    /// wait until it finishes or is cancelled; the handle to return.
    pub fn spawn(
        &self,
        engine: &EngineHandle,
        identity: &McpIdentity,
        work: impl Future<Output = CallToolResult> + Send + 'static,
    ) -> CreateTaskResult {
        let config = engine.config();
        let ttl_ms = config.mcp_wait_cap_ms.saturating_mul(TASK_TTL_FACTOR);
        let options = TaskOptions::new()
            .with_ttl_ms(ttl_ms)
            .with_poll_interval_ms(config.mcp_task_poll_ms);
        let open = engine.open_wait(&identity.caller().holder);
        let task = self.manager.spawn(options, move |context| {
            Box::pin(async move {
                let _open = open;
                tokio::select! {
                    result = work => Ok(result),
                    () = context.cancelled() => Err(TaskExit::Cancelled),
                }
            })
        });
        let mut owners = self.owners();
        let keep = Duration::from_millis(ttl_ms.saturating_mul(2));
        owners.retain(|_, o| o.at.elapsed() < keep);
        owners.insert(
            task.task_id.clone(),
            Owner {
                session: identity.session_id.clone(),
                at: Instant::now(),
            },
        );
        CreateTaskResult::new(task)
    }

    /// `tasks/get` for `identity`.
    ///
    /// # Errors
    /// An MCP error for an unknown task, or one of another session.
    pub fn get(&self, identity: &McpIdentity, task_id: &str) -> Result<GetTaskResult, McpError> {
        self.check_owner(identity, task_id)?;
        Ok(GetTaskResult::new(self.manager.get_task(task_id)?))
    }

    /// `tasks/cancel` for `identity`.
    ///
    /// # Errors
    /// An MCP error for an unknown task, or one of another session.
    pub fn cancel(&self, identity: &McpIdentity, task_id: &str) -> Result<(), McpError> {
        self.check_owner(identity, task_id)?;
        self.manager.cancel_task(task_id)
    }

    fn check_owner(&self, identity: &McpIdentity, task_id: &str) -> Result<(), McpError> {
        match self.owners().get(task_id) {
            Some(o) if o.session == identity.session_id => Ok(()),
            _ => Err(McpError::invalid_params(
                format!("there is no task {task_id:?}"),
                None,
            )),
        }
    }
}
