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

use std::fmt::Write as _;
use std::sync::Arc;

use rmcp::handler::server::common::schema_for_input;
use rmcp::model::{CallToolResult, ContentBlock, JsonObject, Tool};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

use crate::engine::{ApiError, Command, EngineHandle, ErrorKind};
use crate::mcp::McpIdentity;
use crate::model::{
    BuiltinCheck, HealthSpec, Priority, ResourceType, Scope, UnitSettings, allowed_builtins,
    placeholder_keys,
};

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
}

/// What a tool call runs with.
struct Ctx<'a> {
    engine: &'a EngineHandle,
    identity: &'a McpIdentity,
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
        ) -> Option<Result<Outcome, ApiError>> {
            Some(match name {
                $( $name => match parse::<$args>($name, args) {
                    Ok(a) => $handler(ctx, a).await,
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
    "lease_acquire" => not_yet(LeaseAcquireArgs),
        "Lease a unit of a resource for a while, or queue for one.";
    "lease_status" => not_yet(NoArgs),
        "This session's leases, queued requests and pending notices.";
    "lease_extend" => not_yet(LeaseExtendArgs),
        "Extend a lease you hold.";
    "lease_release" => not_yet(LeaseReleaseArgs),
        "Release a lease: its cleanup window starts.";
    "lease_confirm" => not_yet(LeaseConfirmArgs),
        "Confirm a released or yielded unit is clean.";
    "lease_set_interruptible" => not_yet(LeaseInterruptibleArgs),
        "Say whether others may borrow your unit while you pause.";
    "lease_yield_refuse" => not_yet(LeaseIdArgs),
        "Refuse a request to yield your unit.";
    "lease_reclaim" => not_yet(LeaseIdArgs),
        "Take your unit back from a borrower who overran.";
    "lease_cancel" => not_yet(RequestIdArgs),
        "Cancel a queued request.";
    "lease_claim" => not_yet(LeaseIdArgs),
        "Claim a unit offered to you.";
    "lease_wait" => not_yet(LeaseWaitArgs),
        "Wait for this session's notices; acknowledge the previous batch with ack.";
}

/// Run tool `name` with `args` as `identity`; `None` when there is no such
/// tool. Engine and argument errors come back as tool errors.
pub async fn call(
    engine: &EngineHandle,
    identity: &McpIdentity,
    name: &str,
    args: Value,
) -> Option<CallToolResult> {
    let ctx = Ctx { engine, identity };
    Some(match dispatch(&ctx, name, args).await? {
        Ok(outcome) => {
            let mut result = CallToolResult::success(vec![ContentBlock::text(outcome.text)]);
            result.structured_content = Some(outcome.structured);
            result
        }
        Err(e) => tool_error(&e),
    })
}

/// A tool error carrying the engine's code, message and details.
fn tool_error(e: &ApiError) -> CallToolResult {
    let mut text = format!("{}: {}", e.code, e.message);
    if let Some(details) = &e.details {
        let _ = write!(text, " ({details})");
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
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetArgs {
    /// Resource id.
    pub resource: String,
    /// A specific unit; absent means any free unit.
    #[serde(default)]
    pub unit: Option<String>,
}

/// `lease_acquire` (provisional until the leasing tools land).
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LeaseAcquireArgs {
    /// What to lease.
    pub target: TargetArgs,
    /// How long you expect to use it, e.g. "30m".
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

/// `lease_extend` (provisional).
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LeaseExtendArgs {
    /// The lease id from the grant.
    pub lease_id: String,
    /// How much longer you need it, e.g. "15m".
    #[serde(rename = "for")]
    pub duration: String,
}

/// `lease_release` (provisional).
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmChoice {
    /// The unit is clean.
    Clean,
    /// The unit was yielded to a borrower.
    Yielded,
}

/// `lease_confirm` (provisional).
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LeaseConfirmArgs {
    /// The lease id from the grant.
    pub lease_id: String,
    /// "clean" (default) or "yielded".
    #[serde(default)]
    pub how: Option<ConfirmChoice>,
}

/// `lease_set_interruptible` (provisional).
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LeaseInterruptibleArgs {
    /// The lease id from the grant.
    pub lease_id: String,
    /// Whether others may borrow the unit while you pause.
    pub interruptible: bool,
}

/// `lease_cancel` (provisional).
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestIdArgs {
    /// The request id from the queued result.
    pub request_id: String,
}

/// `lease_wait` (provisional).
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LeaseWaitArgs {
    /// Notice ids from the previous `lease_wait` result.
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

/// A leasing tool whose handler is not in this build yet.
// Async and generic to fit the tool table; the leasing tools replace it.
#[allow(clippy::unused_async)]
async fn not_yet<A>(_ctx: &Ctx<'_>, _args: A) -> Result<Outcome, ApiError> {
    Err(ApiError {
        kind: ErrorKind::Unavailable,
        code: "not_implemented",
        message: "this leasing tool is not available yet; use the clerk CLI".to_owned(),
        details: None,
    })
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

    #[test]
    fn every_schema_is_an_object_refusing_extra_properties() {
        for tool in catalog() {
            assert_eq!(tool.input_schema["type"], json!("object"), "{}", tool.name);
            assert_eq!(
                tool.input_schema["additionalProperties"],
                json!(false),
                "{}",
                tool.name
            );
        }
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
    fn outcome_text_ends_in_the_next_step() {
        let o = Outcome::new(Value::Null, &["One.".into(), "Two.".into()], "Call x.");
        assert_eq!(o.text, "One. Two. Call x.");
    }
}
