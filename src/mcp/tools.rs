//! The MCP tool table: names, input schemas, engine mapping and the text
//! block that ends every result with the next step the clerk expects.
//!
//! - Arguments use the engine's wire names and refuse unknown fields, so no
//!   tool takes a holder, session or project: identity comes only from
//!   [`McpIdentity`].
//! - Every engine call runs as [`McpIdentity::caller`], whose view is always
//!   `caller`; the engine refuses agent sessions `view: all`, explicit
//!   scopes and holder lookups on its own.
//! - Results carry the engine's JSON as structured content (always an
//!   object) plus one text block. Engine errors become tool errors whose
//!   structured content is `{"error": {"code", "message", "details"}}`.
//! - `lease_wait`, and `lease_acquire` when it queues, may answer with a
//!   task instead (see [`crate::mcp::wait`]).

use std::fmt::Write as _;
use std::sync::Arc;

use rmcp::handler::server::common::schema_for_input;
use rmcp::model::{
    CallToolResponse, CallToolResult, ContentBlock, CreateTaskResult, JsonObject, Tool,
};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

use crate::duration::parse_duration;
use crate::engine::{ApiError, Command, EngineHandle};
use crate::mcp::McpIdentity;
use crate::mcp::wait::{self, Batch, Live};
use crate::model::{
    BuiltinCheck, HealthSpec, LeaseId, NoticeId, Priority, RequestId, ResourceType, Scope,
    UnitSettings, allowed_builtins, placeholder_keys,
};
use crate::store::notices::{Notice, NoticeKind};

/// What a tool produced: structured content and the text block.
struct Outcome {
    structured: Value,
    text: String,
}

impl Outcome {
    /// `structured`, described by `summary` lines and ending in `next`.
    fn new(structured: Value, summary: &[String], next: &str) -> Self {
        let mut text = String::new();
        for line in summary {
            text.push_str(line);
            text.push(' ');
        }
        text.push_str(next);
        Self { structured, text }
    }

    /// The tool result.
    fn into_result(self) -> CallToolResult {
        let mut result = CallToolResult::success(vec![ContentBlock::text(self.text)]);
        result.structured_content = Some(self.structured);
        result
    }
}

/// What a tool answers: a result now, or a task to poll.
enum Answer {
    Done(Outcome),
    Task(CreateTaskResult),
}

impl From<Outcome> for Answer {
    fn from(o: Outcome) -> Self {
        Self::Done(o)
    }
}

/// What a tool call runs with.
struct Ctx<'a> {
    engine: &'a EngineHandle,
    identity: &'a McpIdentity,
    live: &'a Live,
}

impl Ctx<'_> {
    /// Run engine `method` as this identity.
    async fn engine(&self, method: &str, args: Value) -> Result<Value, ApiError> {
        let command = Command::parse(method, self.identity.caller(), args)?;
        self.engine.call(command).await
    }
}

/// Declares the tool table once: the catalog and the dispatch cannot drift
/// apart.
macro_rules! tools {
    ($( $name:literal => $handler:ident($args:ty), $description:literal; )*) => {
        /// Every tool name, in catalog order.
        pub const NAMES: &[&str] = &[$( $name, )*];

        /// Every tool with its description and input schema.
        #[must_use]
        pub fn catalog() -> Vec<Tool> {
            vec![$( describe::<$args>($name, $description), )*]
        }

        /// Run tool `name`; `None` when there is no such tool.
        async fn dispatch(
            ctx: &Ctx<'_>,
            name: &str,
            args: Value,
        ) -> Option<Result<Answer, ApiError>> {
            Some(match name {
                $( $name => match parse::<$args>($name, args) {
                    Ok(a) => $handler(ctx, a).await.map(Answer::from),
                    Err(e) => Err(e),
                }, )*
                _ => return None,
            })
        }
    };
}

tools! {
    "type_list" => type_list(NoArgs),
        "List the built-in resource types, the unit settings each takes, its health checks \
         and its usage-template placeholders.";
    "resource_create" => resource_create(ResourceCreateArgs),
        "Register a resource (a kind of thing agents lease). Lands in this project unless \
         scope is \"user\". Add its units with unit_add.";
    "resource_update" => resource_update(ResourceUpdateArgs),
        "Change a resource's description, usage template, health checks or tags.";
    "resource_delete" => resource_delete(ResourceIdArgs),
        "Delete a resource that has no leases or queued requests.";
    "resource_list" => resource_list(ResourceListArgs),
        "List the resources visible here (user scope and this project), optionally only \
         those carrying every given tag.";
    "resource_get" => resource_get(ResourceIdArgs),
        "Show one resource with its units, settings and health.";
    "unit_add" => unit_add(UnitAddArgs),
        "Add a unit (one leasable instance) to a resource. Reports units elsewhere that look \
         like the same thing.";
    "unit_update" => unit_update(UnitUpdateArgs),
        "Change a unit's settings or description.";
    "unit_remove" => unit_remove(UnitArgs),
        "Remove a free unit.";
    "unit_retire" => unit_retire(UnitArgs),
        "Retire a unit: removed now when free, otherwise never granted again and removed once \
         its lease ends.";
    "lease_acquire" => lease_acquire(LeaseAcquireArgs),
        "Lease a unit of a resource for a while, or queue for one. A queued request is \
         offered the unit later: keep lease_wait running, it claims the offer for you.";
    "lease_status" => lease_status(NoArgs),
        "This session's leases, queued requests and pending notices.";
    "lease_extend" => lease_extend(LeaseExtendArgs),
        "Extend a lease you hold.";
    "lease_release" => lease_release(LeaseReleaseArgs),
        "Release a lease: its cleanup window starts.";
    "lease_confirm" => lease_confirm(LeaseConfirmArgs),
        "Confirm a released or yielded unit is clean.";
    "lease_set_interruptible" => lease_set_interruptible(LeaseInterruptibleArgs),
        "Say whether others may borrow your unit while you pause.";
    "lease_yield_refuse" => lease_yield_refuse(LeaseIdArgs),
        "Refuse a request to yield your unit.";
    "lease_reclaim" => lease_reclaim(LeaseIdArgs),
        "Take your unit back from a borrower who overran.";
    "lease_cancel" => lease_cancel(RequestIdArgs),
        "Cancel a queued request.";
    "lease_claim" => lease_claim(LeaseIdArgs),
        "Claim a unit offered to you.";
    "lease_wait" => lease_wait(LeaseWaitArgs),
        "Wait for this session's notices (offers are claimed for you). Returns at once while \
         any are unacknowledged; pass the previous result's notice ids as ack. Keep one \
         running while you hold or wait for anything.";
}

/// Run tool `name` with `args` as `identity` in request `live`; `None` when
/// there is no such tool. Engine and argument errors come back as tool
/// errors.
pub async fn call(
    engine: &EngineHandle,
    identity: &McpIdentity,
    live: &Live,
    name: &str,
    args: Value,
) -> Option<CallToolResponse> {
    let ctx = Ctx {
        engine,
        identity,
        live,
    };
    Some(match dispatch(&ctx, name, args).await? {
        Ok(Answer::Done(outcome)) => CallToolResponse::Complete(outcome.into_result()),
        Ok(Answer::Task(task)) => CallToolResponse::Task(task),
        Err(e) => CallToolResponse::Complete(tool_error(&e)),
    })
}

/// A task result: the outcome, or the error as a tool error.
fn task_result(outcome: Result<Outcome, ApiError>) -> CallToolResult {
    outcome.map_or_else(|e| tool_error(&e), Outcome::into_result)
}

/// A tool error carrying the engine's code, message and details.
fn tool_error(e: &ApiError) -> CallToolResult {
    tool_error_then(e, None)
}

/// [`tool_error`] whose text ends in `next`, when given.
fn tool_error_then(e: &ApiError, next: Option<&str>) -> CallToolResult {
    let mut text = format!("{}: {}", e.code, e.message);
    if let Some(details) = &e.details {
        let _ = write!(text, " ({details})");
    }
    if let Some(next) = next {
        let _ = write!(text, ". {next}");
    }
    let mut result = CallToolResult::error(vec![ContentBlock::text(text)]);
    result.structured_content = Some(json!({ "error": e }));
    result
}

/// The tool `name` taking `A`.
fn describe<A: JsonSchema + 'static>(name: &'static str, description: &'static str) -> Tool {
    // Invariant: every argument type is a struct, so its schema is an object
    // (the unit tests check each one).
    let schema: Arc<JsonObject> =
        schema_for_input::<A>().expect("tool argument schemas are objects");
    Tool::new(name, description, schema)
}

/// `args` as `A`; unknown fields (`holder`, `session`, `project`, …) are
/// refused.
fn parse<A: DeserializeOwned>(tool: &str, args: Value) -> Result<A, ApiError> {
    serde_json::from_value(args)
        .map_err(|e| ApiError::invalid_request(format!("{tool} arguments are not valid: {e}")))
}

/// Encode an argument struct as engine args.
fn engine_args(args: &impl Serialize) -> Result<Value, ApiError> {
    serde_json::to_value(args)
        .map_err(|e| ApiError::internal(format!("cannot encode arguments: {e}")))
}

/// `Some(value)` for a present field, even when `null`.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

/// No arguments.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoArgs {}

/// Where `resource_create` puts the resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScopeChoice {
    /// Every project of this user.
    User,
    /// This session's project only (the default when one resolves).
    Project,
}

/// `resource_create`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResourceCreateArgs {
    /// Resource id: lowercase letters, digits and dashes.
    pub id: String,
    /// The resource type (see `type_list`).
    #[serde(rename = "type")]
    pub ty: ResourceType,
    /// "project" (default when this session has a project) or "user".
    #[serde(default)]
    pub scope: Option<ScopeChoice>,
    /// One line saying what the resource is.
    #[serde(default)]
    pub description: String,
    /// Markdown usage template with {resource.*} / {unit.*} placeholders.
    #[serde(default)]
    pub usage: String,
    /// Health checks, e.g. [{"builtin": "exists"}, {"custom": "cmd"}]; absent
    /// means the type default.
    #[serde(default)]
    pub health: Option<Value>,
    /// Search tags.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// `resource_update`. Absent fields are left alone.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResourceUpdateArgs {
    /// Resource id.
    pub id: String,
    /// New one-line description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// New usage template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<String>,
    /// New health checks; null resets them to the type default.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<Value>")]
    pub health: Option<Option<Value>>,
    /// Replaces the whole tag set ([] clears it); not with `add_tags` / `remove_tags`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    /// Tags to add.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub add_tags: Vec<String>,
    /// Tags to remove.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove_tags: Vec<String>,
}

/// `resource_get`, `resource_delete`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResourceIdArgs {
    /// Resource id.
    pub id: String,
}

/// `resource_list`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResourceListArgs {
    /// Only resources carrying every one of these tags.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// `unit_add`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UnitAddArgs {
    /// Resource id.
    pub resource: String,
    /// Unit id: lowercase letters, digits and dashes.
    pub id: String,
    /// Settings of the resource's type (see `type_list`).
    pub settings: UnitSettings,
    /// One line saying which instance this is.
    #[serde(default)]
    pub description: String,
}

/// `unit_update`. Absent fields are left alone.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UnitUpdateArgs {
    /// Resource id.
    pub resource: String,
    /// Unit id.
    pub unit: String,
    /// New settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<UnitSettings>,
    /// New one-line description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// `unit_remove`, `unit_retire`.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UnitArgs {
    /// Resource id.
    pub resource: String,
    /// Unit id.
    pub unit: String,
}

/// What `lease_acquire` asks for.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetArgs {
    /// Resource id.
    pub resource: String,
    /// A specific unit; absent means any free unit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

/// `lease_acquire`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LeaseAcquireArgs {
    /// What to lease.
    pub target: TargetArgs,
    /// How long you expect to use it: <n>s|m|h|d, e.g. "30m" (at most 30d).
    #[serde(rename = "for")]
    pub duration: String,
    /// What you will do with it, shown to others who wait.
    pub purpose: String,
    /// Others may borrow the unit while you pause.
    #[serde(default)]
    pub interruptible: bool,
    /// Queue priority (default normal).
    #[serde(default)]
    pub priority: Option<Priority>,
    /// The user's request this serves, when it overrides the queue.
    #[serde(default)]
    pub override_reason: Option<String>,
}

/// Tools acting on one lease.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LeaseIdArgs {
    /// The lease id from the grant.
    pub lease_id: String,
}

/// `lease_extend`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LeaseExtendArgs {
    /// The lease id from the grant.
    pub lease_id: String,
    /// The lease then ends this long from now: <n>s|m|h|d, e.g. "15m".
    #[serde(rename = "for")]
    pub duration: String,
}

/// `lease_release`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LeaseReleaseArgs {
    /// The lease id from the grant.
    pub lease_id: String,
    /// The work failed: end now and mark the unit dirty.
    #[serde(default)]
    pub failed: bool,
}

/// How a unit was left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmChoice {
    /// The unit is clean.
    Clean,
    /// The unit was yielded to a borrower.
    Yielded,
}

/// `lease_confirm`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LeaseConfirmArgs {
    /// The lease id from the grant.
    pub lease_id: String,
    /// "clean" (default) or "yielded".
    #[serde(default)]
    pub how: Option<ConfirmChoice>,
}

/// `lease_set_interruptible`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LeaseInterruptibleArgs {
    /// The lease id from the grant.
    pub lease_id: String,
    /// Whether others may borrow the unit while you pause.
    pub interruptible: bool,
}

/// `lease_cancel`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestIdArgs {
    /// The request id from the queued result.
    pub request_id: String,
}

/// `lease_wait`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LeaseWaitArgs {
    /// The notice ids of the previous `lease_wait` result, once handled.
    #[serde(default)]
    pub ack: Vec<String>,
}

// ---------------------------------------------------------------------------
// Inventory tools
// ---------------------------------------------------------------------------

/// The settings fields of each type, as (name, required, meaning).
fn settings_fields(ty: ResourceType) -> &'static [(&'static str, bool, &'static str)] {
    match ty {
        ResourceType::Device => &[("path", true, "filesystem path of the device node")],
        ResourceType::Service => &[
            (
                "mode",
                true,
                "\"connect\" to a running server or \"listen\" on a reserved port",
            ),
            ("host", true, "host name or address"),
            ("port", true, "TCP port, not 0"),
            ("scheme", false, "URL scheme, e.g. \"http\""),
            ("service_id", false, "service-specific identifier"),
        ],
        ResourceType::Generic => &[("identity_key", true, "a key naming the thing")],
    }
}

// Async to fit the tool table, like every handler.
#[allow(clippy::unused_async)]
async fn type_list(_ctx: &Ctx<'_>, NoArgs {}: NoArgs) -> Result<Outcome, ApiError> {
    let types: Vec<Value> = [
        (
            ResourceType::Device,
            "a device node, e.g. a USB serial board",
        ),
        (
            ResourceType::Service,
            "a network service to connect to or listen as",
        ),
        (ResourceType::Generic, "anything else, named by a key"),
    ]
    .into_iter()
    .map(|(ty, description)| {
        let mut checks: Vec<&str> = allowed_builtins(ty)
            .iter()
            .map(|c: &BuiltinCheck| c.as_str())
            .collect();
        checks.push("custom");
        let fields: Vec<Value> = settings_fields(ty)
            .iter()
            .map(|(name, required, meaning)| {
                json!({"name": name, "required": required, "description": meaning})
            })
            .collect();
        json!({
            "type": ty,
            "description": description,
            "settings": fields,
            "health_checks": checks,
            "default_health": HealthSpec::default_for(ty, None),
            "usage_placeholders": placeholder_keys(ty),
        })
    })
    .collect();
    let names: Vec<&str> = [
        ResourceType::Device,
        ResourceType::Service,
        ResourceType::Generic,
    ]
    .iter()
    .map(|t| t.as_str())
    .collect();
    Ok(Outcome::new(
        json!({ "types": types }),
        &[format!("Types: {}.", names.join(", "))],
        "Call resource_create with one of these types, then unit_add with settings of that \
         type (\"type\" names it).",
    ))
}

async fn resource_create(ctx: &Ctx<'_>, a: ResourceCreateArgs) -> Result<Outcome, ApiError> {
    let project = ctx.identity.project.clone();
    let scope = match (a.scope, project) {
        (Some(ScopeChoice::User), _) | (None, None) => Scope::User,
        (Some(ScopeChoice::Project) | None, Some(p)) => Scope::Project(p),
        (Some(ScopeChoice::Project), None) => {
            return Err(ApiError::invalid_argument(
                "this session has no project; leave scope out or use \"user\"",
            ));
        }
    };
    let mut args = json!({
        "id": a.id,
        "type": a.ty,
        "scope": scope,
        "description": a.description,
        "usage": a.usage,
        "tags": a.tags,
    });
    if let Some(health) = a.health {
        args["health"] = health;
    }
    let created = ctx.engine("create_resource", args).await?;
    let where_ = match scope {
        Scope::User => "user scope (every project)".to_owned(),
        Scope::Project(p) => format!("project {}", p.0.display()),
    };
    let mut summary = vec![format!(
        "Created resource {} in {where_}.",
        str_at(&created, "/resource/id")
    )];
    summary.extend(warning_lines(&created["warnings"]));
    Ok(Outcome::new(
        created,
        &summary,
        "Call unit_add to register its units.",
    ))
}

async fn resource_update(ctx: &Ctx<'_>, a: ResourceUpdateArgs) -> Result<Outcome, ApiError> {
    let resource = ctx.engine("update_resource", engine_args(&a)?).await?;
    Ok(Outcome::new(
        json!({ "resource": resource }),
        &[format!("Updated resource {}.", a.id)],
        "Call resource_get to review it.",
    ))
}

async fn resource_delete(ctx: &Ctx<'_>, a: ResourceIdArgs) -> Result<Outcome, ApiError> {
    ctx.engine("delete_resource", engine_args(&a)?).await?;
    Ok(Outcome::new(
        json!({ "deleted": { "resource": a.id } }),
        &[format!("Deleted resource {}.", a.id)],
        "Call resource_list to see what remains.",
    ))
}

async fn resource_list(ctx: &Ctx<'_>, a: ResourceListArgs) -> Result<Outcome, ApiError> {
    let filtered = !a.tags.is_empty();
    let resources = ctx.engine("list_resources", engine_args(&a)?).await?;
    let count = resources.as_array().map_or(0, Vec::len);
    let next = match (count, filtered) {
        (0, true) => "Call resource_list with fewer tags, or resource_create to register one.",
        (0, false) => "Call type_list, then resource_create to register one.",
        _ => "Call resource_get for a resource's units, or lease_acquire to use one.",
    };
    Ok(Outcome::new(
        json!({ "resources": resources }),
        &[format!("{count} resource(s) visible.")],
        next,
    ))
}

async fn resource_get(ctx: &Ctx<'_>, a: ResourceIdArgs) -> Result<Outcome, ApiError> {
    let resource = ctx.engine("get_resource", engine_args(&a)?).await?;
    let units = resource["units"].as_array().map_or(0, Vec::len);
    let next = if units == 0 {
        "Call unit_add to register its units.".to_owned()
    } else {
        format!(
            "Call lease_acquire with target {{\"resource\": \"{}\"}} before using it.",
            a.id
        )
    };
    Ok(Outcome::new(
        json!({ "resource": resource }),
        &[format!("Resource {} has {units} unit(s).", a.id)],
        &next,
    ))
}

async fn unit_add(ctx: &Ctx<'_>, a: UnitAddArgs) -> Result<Outcome, ApiError> {
    let created = ctx.engine("add_unit", engine_args(&a)?).await?;
    let mut summary = vec![format!("Added unit {}/{}.", a.resource, a.id)];
    summary.extend(warning_lines(&created["warnings"]));
    Ok(Outcome::new(
        created,
        &summary,
        &format!(
            "Call lease_acquire with target {{\"resource\": \"{}\"}} before using it, or \
             unit_add for another unit.",
            a.resource
        ),
    ))
}

async fn unit_update(ctx: &Ctx<'_>, a: UnitUpdateArgs) -> Result<Outcome, ApiError> {
    let updated = ctx.engine("update_unit", engine_args(&a)?).await?;
    let mut summary = vec![format!("Updated unit {}/{}.", a.resource, a.unit)];
    summary.extend(warning_lines(&updated["warnings"]));
    Ok(Outcome::new(
        updated,
        &summary,
        "Call resource_get to review the resource.",
    ))
}

async fn unit_remove(ctx: &Ctx<'_>, a: UnitArgs) -> Result<Outcome, ApiError> {
    ctx.engine("remove_unit", engine_args(&a)?).await?;
    Ok(Outcome::new(
        json!({ "removed": { "resource": a.resource, "unit": a.unit } }),
        &[format!("Removed unit {}/{}.", a.resource, a.unit)],
        "Call resource_get to review the resource.",
    ))
}

async fn unit_retire(ctx: &Ctx<'_>, a: UnitArgs) -> Result<Outcome, ApiError> {
    ctx.engine("retire_unit", engine_args(&a)?).await?;
    Ok(Outcome::new(
        json!({ "retired": { "resource": a.resource, "unit": a.unit } }),
        &[format!(
            "Retired unit {}/{}: removed now if it was free, otherwise never granted again \
             and removed when its lease ends.",
            a.resource, a.unit
        )],
        "Call resource_get to review the resource.",
    ))
}

// ---------------------------------------------------------------------------
// Leasing tools
// ---------------------------------------------------------------------------

/// The sentence every `lease_wait` result ends with.
fn re_arm(ids: &[String]) -> String {
    // Encoding a list of strings cannot fail.
    let list = serde_json::to_string(ids).unwrap_or_else(|_| "[]".to_owned());
    format!("Call lease_wait again with ack={list} while you hold or wait for anything.")
}

/// `for` as milliseconds.
fn duration_ms(text: &str) -> Result<u64, ApiError> {
    let d = parse_duration(text).map_err(|e| ApiError::invalid_argument(e.to_string()))?;
    Ok(u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// "in 59 min" (or "now") for `at`, unix ms, seen at `now`.
fn ends_in(at: i64, now: i64) -> String {
    let mins = (at.saturating_sub(now) + 59_999) / 60_000;
    if mins <= 0 {
        "now".to_owned()
    } else {
        format!("in {mins} min")
    }
}

/// The text brief of a grant or held lease (both carry the same fields).
fn grant_brief(v: &Value, now: i64) -> String {
    let mut brief = format!(
        "You hold {}/{} (lease {}), ending {}: {}",
        str_at(v, "/resource"),
        str_at(v, "/unit"),
        str_at(v, "/lease_id"),
        v["ends_at"]
            .as_i64()
            .map_or_else(|| "later".to_owned(), |at| ends_in(at, now)),
        str_at(v, "/resource_description"),
    );
    let unit = str_at(v, "/unit_description");
    if !unit.is_empty() {
        let _ = write!(brief, " ({unit})");
    }
    brief.push('.');
    if let Some(tags) = v["resource_tags"].as_array().filter(|t| !t.is_empty()) {
        let tags: Vec<&str> = tags.iter().filter_map(Value::as_str).collect();
        let _ = write!(brief, " Tags: {}.", tags.join(", "));
    }
    if v["dirty"].is_object() {
        brief.push_str(" It was handed over without confirmed cleanup; check it first.");
    }
    let usage = str_at(v, "/usage");
    if !usage.is_empty() {
        let _ = write!(brief, " Usage: {usage}");
    }
    brief
}

/// The next step once a unit is held.
const HELD_NEXT: &str = "Call lease_release when you are done with it.";

/// The next step while a request waits in the queue.
const QUEUED_NEXT: &str = "Call lease_wait to wait for the offer; it claims the unit for you.";

/// Claim offered lease `lease`; when it is already this session's (an
/// earlier, dropped `lease_wait` claimed it), its current state instead.
async fn claim_offer(ctx: &Ctx<'_>, lease: &LeaseId) -> Result<Value, ApiError> {
    let id = json!({ "lease_id": lease });
    match ctx.engine("claim", id.clone()).await {
        Ok(v) => Ok(v),
        Err(e) => match ctx.engine("get_lease", id).await {
            Ok(v) if matches!(str_at(&v, "/state"), "held" | "paused") => Ok(v),
            _ => Err(e),
        },
    }
}

async fn lease_acquire(ctx: &Ctx<'_>, a: LeaseAcquireArgs) -> Result<Answer, ApiError> {
    let mut args = json!({
        "target": a.target,
        "duration_ms": duration_ms(&a.duration)?,
        "interruptible": a.interruptible,
        "purpose": a.purpose,
    });
    if let Some(p) = a.priority {
        args["priority"] = json!(p);
    }
    if let Some(r) = a.override_reason {
        args["override_reason"] = json!(r);
    }
    let got = ctx.engine("acquire", args).await?;
    let now = ctx.engine.now_ms();
    match str_at(&got, "/outcome") {
        "granted" => Ok(Outcome::new(got.clone(), &[grant_brief(&got, now)], HELD_NEXT).into()),
        "queued" => {
            let request = RequestId::parse(str_at(&got, "/request_id"))
                .map_err(|e| ApiError::internal(format!("bad request id from acquire: {e}")))?;
            if let Some(tasks) = ctx.live.tasks() {
                let (engine, identity) = (ctx.engine.clone(), ctx.identity.clone());
                let work = async move {
                    let live = Live::default();
                    let ctx = Ctx {
                        engine: &engine,
                        identity: &identity,
                        live: &live,
                    };
                    queued_outcome(&ctx, &request)
                        .await
                        .unwrap_or_else(Failure::into_result)
                };
                return Ok(Answer::Task(tasks.spawn(ctx.engine, ctx.identity, work)));
            }
            Ok(Outcome::new(
                got.clone(),
                &[format!(
                    "Queued for {} at position {} (request {}).",
                    a.target.resource,
                    got["position"],
                    str_at(&got, "/request_id"),
                )],
                QUEUED_NEXT,
            )
            .into())
        }
        "reclaiming" => Ok(reclaiming_outcome(got).into()),
        other => Err(ApiError::internal(format!(
            "acquire answered with an unknown outcome {other:?}"
        ))),
    }
}

/// `lease_acquire` answered `reclaiming`: a paused holder's override of its
/// own borrower.
fn reclaiming_outcome(got: Value) -> Outcome {
    let summary = format!(
        "Your borrower is being ended so lease {} comes back to you.",
        str_at(&got, "/lease_id")
    );
    Outcome::new(
        got,
        &[summary],
        "Call lease_wait to hear when the unit is back.",
    )
}

/// The next step after a queued request's offer could not be claimed.
const CLAIM_FAILED_NEXT: &str = "Call lease_wait to see what became of the offer, or \
     lease_acquire again if you still need the unit.";

/// A task that ended in a tool error, and the next step its text ends in.
struct Failure {
    error: ApiError,
    next: Option<&'static str>,
}

impl From<ApiError> for Failure {
    fn from(error: ApiError) -> Self {
        Self { error, next: None }
    }
}

impl Failure {
    fn into_result(self) -> CallToolResult {
        tool_error_then(&self.error, self.next)
    }
}

/// What a queued request came to, within the wait cap (a task's result).
async fn queued_outcome(ctx: &Ctx<'_>, request: &RequestId) -> Result<CallToolResult, Failure> {
    let holder = ctx.identity.caller().holder;
    let Some(notice) = wait::request_outcome(ctx.engine, &holder, request).await? else {
        return Ok(Outcome::new(
            json!({ "outcome": "queued", "request_id": request }),
            &[format!("Request {request} is still queued.")],
            QUEUED_NEXT,
        )
        .into_result());
    };
    let outcome = match (notice.kind, &notice.lease_id) {
        (NoticeKind::Offered, Some(lease)) => {
            let lease = claim_offer(ctx, lease).await.map_err(|error| Failure {
                error,
                next: Some(CLAIM_FAILED_NEXT),
            })?;
            let brief = grant_brief(&lease, ctx.engine.now_ms());
            Outcome::new(lease, &[brief], HELD_NEXT)
        }
        _ => Outcome::new(
            json!({ "outcome": "dropped", "request_id": request, "notice": notice }),
            std::slice::from_ref(&notice.action),
            "Call lease_acquire again if you still need it.",
        ),
    };
    Ok(outcome.into_result())
}

async fn lease_status(ctx: &Ctx<'_>, NoArgs {}: NoArgs) -> Result<Outcome, ApiError> {
    let mut mine = ctx.engine("list_my_leases", json!({})).await?;
    let pending = ctx
        .engine
        .pending_notices(ctx.identity.caller().holder)
        .await?
        .len();
    mine["pending_notices"] = json!(pending);
    let leases = mine["leases"].as_array().map_or(0, Vec::len);
    let requests = mine["requests"].as_array().map_or(0, Vec::len);
    let mut summary = vec![format!(
        "{leases} lease(s), {requests} queued request(s), {pending} pending notice(s)."
    )];
    if let Some(list) = mine["requests"].as_array() {
        for r in list {
            summary.push(format!(
                "Request {} for {} is at position {}.",
                str_at(r, "/request_id"),
                str_at(r, "/resource"),
                r["position"]
            ));
        }
    }
    let next = if pending > 0 {
        "Call lease_wait to read the pending notices."
    } else if leases + requests > 0 {
        "Keep lease_wait running while you hold or wait for anything."
    } else {
        "Call lease_acquire before using a resource."
    };
    Ok(Outcome::new(mine, &summary, next))
}

async fn lease_extend(ctx: &Ctx<'_>, a: LeaseExtendArgs) -> Result<Outcome, ApiError> {
    let ms = i64::try_from(duration_ms(&a.duration)?).unwrap_or(i64::MAX);
    let now = ctx.engine.now_ms();
    let lease = ctx
        .engine(
            "extend",
            json!({ "lease_id": a.lease_id, "ends_at": now.saturating_add(ms) }),
        )
        .await?;
    let ends = lease["ends_at"]
        .as_i64()
        .map_or_else(|| "later".to_owned(), |at| ends_in(at, now));
    Ok(Outcome::new(
        lease,
        &[format!("Lease {} now ends {ends}.", a.lease_id)],
        HELD_NEXT,
    ))
}

async fn lease_release(ctx: &Ctx<'_>, a: LeaseReleaseArgs) -> Result<Outcome, ApiError> {
    let lease = ctx
        .engine(
            "release",
            json!({ "lease_id": a.lease_id, "failed": a.failed }),
        )
        .await?;
    let (summary, next) = if str_at(&lease, "/state") == "cleaning" {
        (
            format!(
                "Released lease {}; its cleanup window ends {}.",
                a.lease_id,
                lease["cleanup_deadline"]
                    .as_i64()
                    .map_or_else(|| "soon".to_owned(), |at| ends_in(at, ctx.engine.now_ms()))
            ),
            "Call lease_confirm when the unit is clean.",
        )
    } else {
        (
            format!("Lease {} ended; the unit is marked dirty.", a.lease_id),
            "Call lease_status to see what you still hold.",
        )
    };
    Ok(Outcome::new(lease, &[summary], next))
}

async fn lease_confirm(ctx: &Ctx<'_>, a: LeaseConfirmArgs) -> Result<Outcome, ApiError> {
    let mut args = json!({ "lease_id": a.lease_id });
    if let Some(how) = a.how {
        args["how"] = json!(how);
    }
    ctx.engine("confirm", args).await?;
    let yielded = a.how == Some(ConfirmChoice::Yielded);
    let (summary, next) = if yielded {
        (
            format!(
                "Lease {} is paused and lent out; your time is kept.",
                a.lease_id
            ),
            "Call lease_wait to hear when your unit is back.",
        )
    } else {
        (
            format!("Lease {} is over; the unit is clean.", a.lease_id),
            "Call lease_status to see what you still hold.",
        )
    };
    Ok(Outcome::new(
        json!({ "confirmed": { "lease_id": a.lease_id, "how": a.how.unwrap_or(ConfirmChoice::Clean) } }),
        &[summary],
        next,
    ))
}

async fn lease_set_interruptible(
    ctx: &Ctx<'_>,
    a: LeaseInterruptibleArgs,
) -> Result<Outcome, ApiError> {
    let lease = ctx
        .engine(
            "set_interruptible",
            json!({ "lease_id": a.lease_id, "interruptible": a.interruptible }),
        )
        .await?;
    let (summary, next) = if a.interruptible {
        (
            format!("Others may now ask to borrow lease {}'s unit.", a.lease_id),
            "Keep lease_wait running to hear of yield requests, and call lease_release when you \
             are done.",
        )
    } else {
        (
            format!("Lease {} is no longer interruptible.", a.lease_id),
            HELD_NEXT,
        )
    };
    Ok(Outcome::new(lease, &[summary], next))
}

async fn lease_yield_refuse(ctx: &Ctx<'_>, a: LeaseIdArgs) -> Result<Outcome, ApiError> {
    ctx.engine("refuse_yield", json!({ "lease_id": a.lease_id }))
        .await?;
    Ok(Outcome::new(
        json!({ "refused_yield": { "lease_id": a.lease_id } }),
        &[format!(
            "You keep lease {}; the requester will not ask you again.",
            a.lease_id
        )],
        HELD_NEXT,
    ))
}

async fn lease_reclaim(ctx: &Ctx<'_>, a: LeaseIdArgs) -> Result<Outcome, ApiError> {
    ctx.engine("reclaim", json!({ "lease_id": a.lease_id }))
        .await?;
    Ok(Outcome::new(
        json!({ "reclaiming": { "lease_id": a.lease_id } }),
        &[format!(
            "The borrower of lease {}'s unit is being ended.",
            a.lease_id
        )],
        "Call lease_wait to hear when the unit is back.",
    ))
}

async fn lease_cancel(ctx: &Ctx<'_>, a: RequestIdArgs) -> Result<Outcome, ApiError> {
    ctx.engine("cancel", json!({ "request_id": a.request_id }))
        .await?;
    Ok(Outcome::new(
        json!({ "cancelled": { "request_id": a.request_id } }),
        &[format!("Cancelled request {}.", a.request_id)],
        "Call lease_status to see what you still hold.",
    ))
}

async fn lease_claim(ctx: &Ctx<'_>, a: LeaseIdArgs) -> Result<Outcome, ApiError> {
    let lease = ctx
        .engine("claim", json!({ "lease_id": a.lease_id }))
        .await?;
    let brief = grant_brief(&lease, ctx.engine.now_ms());
    Ok(Outcome::new(lease, &[brief], HELD_NEXT))
}

async fn lease_wait(ctx: &Ctx<'_>, a: LeaseWaitArgs) -> Result<Answer, ApiError> {
    let ack = a
        .ack
        .iter()
        .map(|id| NoticeId::parse(id).map_err(|e| ApiError::invalid_argument(e.to_string())))
        .collect::<Result<Vec<_>, _>>()?;
    let holder = ctx.identity.caller().holder;
    let acknowledged = if ack.is_empty() {
        0
    } else {
        ctx.engine.ack_notices(holder.clone(), ack).await?
    };
    if let Some(tasks) = ctx.live.tasks() {
        let (engine, identity) = (ctx.engine.clone(), ctx.identity.clone());
        let work = async move {
            let live = Live::default();
            let ctx = Ctx {
                engine: &engine,
                identity: &identity,
                live: &live,
            };
            task_result(wait_outcome(&ctx, acknowledged).await)
        };
        return Ok(Answer::Task(tasks.spawn(ctx.engine, ctx.identity, work)));
    }
    let _open = ctx.engine.open_wait(&holder);
    wait_outcome(ctx, acknowledged).await.map(Answer::from)
}

/// One `lease_wait` batch, with its offers claimed.
async fn wait_outcome(ctx: &Ctx<'_>, acknowledged: usize) -> Result<Outcome, ApiError> {
    let holder = ctx.identity.caller().holder;
    let (notices, timed_out) = match wait::next_batch(ctx.engine, &holder, ctx.live).await? {
        Batch::Notices(n) => (n, false),
        Batch::Empty => (Vec::new(), true),
        // Nobody reads this result; the notices stay pending.
        Batch::Gone => (Vec::new(), false),
    };
    let now = ctx.engine.now_ms();
    let mut summary = Vec::new();
    let mut grants = Vec::new();
    let mut claim_errors = Vec::new();
    if notices.is_empty() {
        summary.push("No notices arrived.".to_owned());
    } else {
        summary.push(format!("{} notice(s):", notices.len()));
    }
    for n in &notices {
        summary.push(notice_line(n));
        if let (NoticeKind::Offered, Some(lease)) = (n.kind, &n.lease_id) {
            match claim_offer(ctx, lease).await {
                Ok(lease) => {
                    summary.push(format!("Claimed it for you. {}", grant_brief(&lease, now)));
                    summary.push(HELD_NEXT.to_owned());
                    grants.push(lease);
                }
                Err(e) => {
                    summary.push(format!("Could not claim lease {lease}: {}.", e.message));
                    claim_errors.push(json!({ "lease_id": lease, "error": e }));
                }
            }
        }
    }
    let ids: Vec<String> = notices.iter().map(|n| n.id.as_str().to_owned()).collect();
    Ok(Outcome::new(
        json!({
            "notices": notices,
            "grants": grants,
            "claim_errors": claim_errors,
            "acknowledged": acknowledged,
            "timed_out": timed_out,
        }),
        &summary,
        &re_arm(&ids),
    ))
}

/// One notice as a text line, naming its id.
fn notice_line(n: &Notice) -> String {
    let kind = serde_json::to_value(n.kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    format!("[{}] {kind}: {}", n.id, n.action)
}

/// The string at `pointer` in `v`, or "".
fn str_at<'a>(v: &'a Value, pointer: &str) -> &'a str {
    v.pointer(pointer).and_then(Value::as_str).unwrap_or("")
}

/// One sentence per inventory warning.
fn warning_lines(warnings: &Value) -> Vec<String> {
    let Some(list) = warnings.as_array() else {
        return Vec::new();
    };
    list.iter()
        .map(|w| {
            let s = |k: &str| str_at(w, &format!("/{k}")).to_owned();
            match str_at(w, "/kind") {
                "shadows_user_resource" => format!(
                    "Inside this project it hides the user-scope resource {}.",
                    s("id")
                ),
                "shadowed_by_project" => format!(
                    "Inside project {} a project resource {} hides it.",
                    s("project"),
                    s("id")
                ),
                "duplicate_of_user_unit" => format!(
                    "It looks like the user-scope unit {}/{} ({}); the clerk linked the two.",
                    s("resource"),
                    s("unit"),
                    s("unit_description")
                ),
                "duplicate_of_project_unit" => format!(
                    "It looks like this project's unit {}/{} ({}); the clerk linked the two.",
                    s("resource"),
                    s("unit"),
                    s("unit_description")
                ),
                "similar_in_other_project" => format!(
                    "Tell the user: project {} has a similar unit; to share one resource they \
                     can approve promotion {} with `clerk approve {}`.",
                    s("project"),
                    s("promotion_id"),
                    s("promotion_id")
                ),
                other => format!("Warning: {other}."),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_lists_every_name_once() {
        let names: Vec<String> = catalog().iter().map(|t| t.name.to_string()).collect();
        assert_eq!(names, NAMES);
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), names.len());
    }

    /// Every object schema in `schema` (with properties) that allows extra
    /// properties, by JSON pointer.
    fn open_objects(schema: &Value, at: &str, found: &mut Vec<String>) {
        match schema {
            Value::Object(m) => {
                if m.contains_key("properties")
                    && m.get("additionalProperties") != Some(&json!(false))
                {
                    found.push(at.to_owned());
                }
                for (k, v) in m {
                    open_objects(v, &format!("{at}/{k}"), found);
                }
            }
            Value::Array(items) => {
                for (i, v) in items.iter().enumerate() {
                    open_objects(v, &format!("{at}/{i}"), found);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn every_schema_is_an_object() {
        for tool in catalog() {
            assert_eq!(tool.input_schema["type"], json!("object"), "{}", tool.name);
        }
    }

    #[test]
    fn every_object_schema_refuses_extra_properties_at_any_depth() {
        let mut found = Vec::new();
        for tool in catalog() {
            let schema = Value::Object((*tool.input_schema).clone());
            open_objects(&schema, &tool.name, &mut found);
        }
        assert_eq!(found, Vec::<String>::new());
    }

    #[test]
    fn unit_settings_schema_variants_refuse_extra_properties() {
        let tool = catalog()
            .into_iter()
            .find(|t| t.name == "unit_add")
            .unwrap();
        let schema = Value::Object((*tool.input_schema).clone());
        let text = schema.to_string();
        assert!(text.contains("identity_key"), "{text}");
        let mut found = Vec::new();
        open_objects(&schema, "unit_add", &mut found);
        assert_eq!(found, Vec::<String>::new());
    }

    #[test]
    fn unit_settings_refuse_an_unknown_key() {
        let r = parse::<UnitAddArgs>(
            "unit_add",
            json!({"resource": "r", "id": "u", "settings": {"type": "generic", "identity_key": "k", "holder": "x"}}),
        );
        assert!(r.is_err());
    }

    #[test]
    fn update_keeps_an_explicit_null_health() {
        let a: ResourceUpdateArgs = parse("t", json!({"id": "r", "health": null})).unwrap();
        assert_eq!(engine_args(&a).unwrap(), json!({"id": "r", "health": null}));
    }

    #[test]
    fn update_leaves_out_an_absent_health() {
        let a: ResourceUpdateArgs = parse("t", json!({"id": "r"})).unwrap();
        assert_eq!(engine_args(&a).unwrap(), json!({"id": "r"}));
    }

    #[test]
    fn reclaiming_outcome_names_the_lease_and_lease_wait() {
        let o = reclaiming_outcome(json!({"outcome": "reclaiming", "lease_id": "lse_1"}));
        assert!(o.text.contains("lease lse_1 comes back"), "{}", o.text);
        assert!(
            o.text
                .ends_with("Call lease_wait to hear when the unit is back.")
        );
    }

    #[test]
    fn a_failed_claim_ends_in_lease_wait() {
        let failure = Failure {
            error: ApiError::internal("gone"),
            next: Some(CLAIM_FAILED_NEXT),
        };
        let result = failure.into_result();
        let text = serde_json::to_value(&result.content).unwrap()[0]["text"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(text.ends_with(CLAIM_FAILED_NEXT), "{text}");
        assert!(text.starts_with("internal_error: gone"), "{text}");
    }

    #[test]
    fn outcome_text_ends_in_the_next_step() {
        let o = Outcome::new(Value::Null, &["One.".into(), "Two.".into()], "Call x.");
        assert_eq!(o.text, "One. Two. Call x.");
    }
}
