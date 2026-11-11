//! `clerk resource …` and `clerk unit …`: one daemon call per command.
//!
//! `create`/`update` and `unit add`/`update` also take a YAML document
//! (`-f PATH` or `-f -`) whose keys are the RPC argument names. A field set
//! both by a flag and in the document is a usage error, not merged.

use std::fmt::Write as _;
use std::path::PathBuf;

use clap::{Args, Subcommand, ValueEnum};
use leclerk::admin::ScopeSpec;
use leclerk::client::ApiError;
use leclerk::model::{BuiltinCheck, Check, HealthSpec, ResourceType, Scope, ServiceMode};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value, json};

use super::admin::{self, ScopeFlag};
use super::caller::{self, CallerKind};
use super::input::{self, InputSpec};
use super::output::View;
use super::{Ctx, UsageError, views};

#[derive(Subcommand)]
pub enum ResourceAction {
    /// Register a resource.
    Create(CreateArgs),
    /// Change a resource's description, usage, tags or health checks.
    Update(UpdateArgs),
    /// Delete a free resource.
    Delete {
        /// The resource id.
        id: String,
        #[command(flatten)]
        scope: ScopeFlag,
    },
    /// List resources, optionally only those carrying every given tag.
    List(views::TagFilter),
    /// One resource with its units and its raw usage template.
    Show {
        /// The resource id.
        id: String,
        #[command(flatten)]
        scope: ScopeFlag,
    },
}

impl ResourceAction {
    /// The command as typed: `resource create`.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Create(_) => "resource create",
            Self::Update(_) => "resource update",
            Self::Delete { .. } => "resource delete",
            Self::List(_) => "resource list",
            Self::Show { .. } => "resource show",
        }
    }
}

#[derive(Subcommand)]
pub enum UnitAction {
    /// Add a unit to a resource.
    Add(UnitArgs),
    /// Change a unit's settings or description; unset settings are kept.
    Update(UnitArgs),
    /// Remove a free unit.
    Remove(UnitRef),
    /// Retire a unit: removed now, or once its lease ends.
    Retire(UnitRef),
}

impl UnitAction {
    /// The command as typed: `unit add`.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Add(_) => "unit add",
            Self::Update(_) => "unit update",
            Self::Remove(_) => "unit remove",
            Self::Retire(_) => "unit retire",
        }
    }
}

/// What kind of thing a resource leases out.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum TypeArg {
    Device,
    Service,
    Generic,
}

impl From<TypeArg> for ResourceType {
    fn from(t: TypeArg) -> Self {
        match t {
            TypeArg::Device => Self::Device,
            TypeArg::Service => Self::Service,
            TypeArg::Generic => Self::Generic,
        }
    }
}

/// Where a new resource lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeArg {
    /// Every project of this user.
    User,
    /// The project of the working directory.
    Project,
}

/// A built-in health check, by its kebab-case name.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CheckArg {
    Exists,
    Accessible,
    NotOpen,
    TcpConnect,
    PortFree,
    Http,
    None,
}

impl From<CheckArg> for BuiltinCheck {
    fn from(c: CheckArg) -> Self {
        match c {
            CheckArg::Exists => Self::Exists,
            CheckArg::Accessible => Self::Accessible,
            CheckArg::NotOpen => Self::NotOpen,
            CheckArg::TcpConnect => Self::TcpConnect,
            CheckArg::PortFree => Self::PortFree,
            CheckArg::Http => Self::Http,
            CheckArg::None => Self::None,
        }
    }
}

/// `--description`, `--usage`, `--usage-file`.
#[derive(Args)]
pub struct TextArgs {
    /// One line saying what the resource is.
    #[arg(long)]
    description: Option<String>,
    /// Markdown instructions for agents; `{unit.<key>}` placeholders are
    /// filled in on grant.
    #[arg(long, conflicts_with = "usage_file")]
    usage: Option<String>,
    /// Read the usage instructions from a file.
    #[arg(long, value_name = "PATH")]
    usage_file: Option<PathBuf>,
}

impl TextArgs {
    /// The usage text from `--usage` or `--usage-file`.
    fn usage(&self) -> Result<Option<String>, UsageError> {
        match (&self.usage, &self.usage_file) {
            (Some(text), _) => Ok(Some(text.clone())),
            (None, Some(path)) => std::fs::read_to_string(path).map(Some).map_err(|e| {
                UsageError(format!(
                    "Cannot read --usage-file {}: {e}. Check the path.",
                    path.display()
                ))
            }),
            (None, None) => Ok(None),
        }
    }
}

/// `--health`, `--health-command`.
#[derive(Args)]
pub struct HealthArgs {
    /// Built-in health checks, in order (omit for the type default).
    #[arg(long, value_enum, value_delimiter = ',', value_name = "CHECK")]
    health: Vec<CheckArg>,
    /// A shell command appended as a custom check; exit 0 means healthy.
    #[arg(long, value_name = "SCRIPT")]
    health_command: Option<String>,
}

impl HealthArgs {
    fn spec(&self) -> Option<HealthSpec> {
        let mut checks: Vec<Check> = self
            .health
            .iter()
            .map(|c| Check::Builtin((*c).into()))
            .collect();
        if let Some(command) = &self.health_command {
            checks.push(Check::Custom {
                command: command.clone(),
            });
        }
        (!checks.is_empty()).then_some(HealthSpec { checks })
    }
}

#[derive(Args)]
pub struct CreateArgs {
    /// The new resource's id.
    id: String,
    /// What kind of thing it is.
    #[arg(long = "type", value_enum)]
    ty: Option<TypeArg>,
    /// `user`, `project` (this directory's project) or `project:<path>`.
    /// Default: `user`; in an agent session, the session's project when
    /// there is one.
    #[arg(long, value_parser = admin::parse_scope, value_name = "SCOPE")]
    scope: Option<ScopeSpec>,
    #[command(flatten)]
    text: TextArgs,
    /// Search tags.
    #[arg(long = "tag", value_delimiter = ',', value_name = "TAG")]
    tags: Vec<String>,
    #[command(flatten)]
    health: HealthArgs,
    /// Read the fields from a YAML file, or `-` for stdin.
    #[arg(short = 'f', long = "file", value_name = "PATH")]
    file: Option<String>,
}

#[derive(Args)]
pub struct UpdateArgs {
    /// The resource id.
    id: String,
    #[command(flatten)]
    scope: ScopeFlag,
    #[command(flatten)]
    text: TextArgs,
    /// Replace every tag with these.
    #[arg(long = "tag", value_delimiter = ',', value_name = "TAG")]
    tags: Vec<String>,
    /// Add these tags.
    #[arg(long = "add-tag", value_delimiter = ',', value_name = "TAG")]
    add_tags: Vec<String>,
    /// Remove these tags.
    #[arg(long = "remove-tag", value_delimiter = ',', value_name = "TAG")]
    remove_tags: Vec<String>,
    #[command(flatten)]
    health: HealthArgs,
    /// Read the fields from a YAML file, or `-` for stdin.
    #[arg(short = 'f', long = "file", value_name = "PATH")]
    file: Option<String>,
}

#[derive(Args)]
pub struct UnitRef {
    /// The resource id.
    resource: String,
    /// The unit id.
    unit: String,
    #[command(flatten)]
    scope: ScopeFlag,
}

#[derive(Args)]
pub struct UnitArgs {
    /// The resource id.
    resource: String,
    /// The unit id.
    unit: String,
    #[command(flatten)]
    scope: ScopeFlag,
    /// Device: the node's path.
    #[arg(long)]
    path: Option<String>,
    /// Service: host name or address.
    #[arg(long)]
    host: Option<String>,
    /// Service: TCP port.
    #[arg(long)]
    port: Option<u16>,
    /// Service: the holder runs the server on the port (default: connect).
    #[arg(long)]
    listen: bool,
    /// Service: URL scheme.
    #[arg(long)]
    scheme: Option<String>,
    /// Service: service-specific identifier.
    #[arg(long)]
    service_id: Option<String>,
    /// Generic: the identity key.
    #[arg(long)]
    key: Option<String>,
    /// One line saying which one this is.
    #[arg(long)]
    description: Option<String>,
    /// Read the fields from a YAML file, or `-` for stdin.
    #[arg(short = 'f', long = "file", value_name = "PATH")]
    file: Option<String>,
}

/// The YAML for `resource create`.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateDoc {
    #[serde(rename = "type")]
    ty: Option<ResourceType>,
    scope: Option<ScopeArg>,
    description: Option<String>,
    usage: Option<String>,
    tags: Option<Vec<String>>,
    health: Option<HealthSpec>,
}

/// The YAML for `resource update`; `health: null` resets to the type
/// default.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateDoc {
    description: Option<String>,
    usage: Option<String>,
    tags: Option<Vec<String>>,
    add_tags: Option<Vec<String>>,
    remove_tags: Option<Vec<String>>,
    // Absent / null / value are three different patches.
    #[allow(clippy::option_option)]
    #[serde(default, deserialize_with = "present")]
    health: Option<Option<HealthSpec>>,
}

/// The YAML for `unit add` and `unit update`: the settings fields plus
/// `description`.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct UnitDoc {
    path: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    mode: Option<ServiceMode>,
    scheme: Option<String>,
    service_id: Option<String>,
    identity_key: Option<String>,
    description: Option<String>,
}

/// A field that is present (even as `null`) is `Some`.
// Three states (absent / null / value) are the point.
#[allow(clippy::option_option)]
fn present<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// The value from the flag or from the document; both is a usage error.
fn once<T>(
    field: &str,
    flag: &str,
    by_flag: Option<T>,
    by_doc: Option<T>,
) -> Result<Option<T>, UsageError> {
    match (by_flag, by_doc) {
        (Some(_), Some(_)) => Err(UsageError(format!(
            "{field} is given both as {flag} and in the YAML document; give it once"
        ))),
        (by_flag, by_doc) => Ok(by_flag.or(by_doc)),
    }
}

fn non_empty(list: &[String]) -> Option<Vec<String>> {
    (!list.is_empty()).then(|| list.to_vec())
}

/// `description` and `usage` from the flags or the document.
fn put_text(
    a: &mut Map<String, Value>,
    text: &TextArgs,
    description: Option<String>,
    usage: Option<String>,
) -> Result<(), UsageError> {
    let by_flag = text.description.clone();
    if let Some(d) = once("description", "--description", by_flag, description)? {
        a.insert("description".into(), json!(d));
    }
    if let Some(u) = once("usage", "--usage/--usage-file", text.usage()?, usage)? {
        a.insert("usage".into(), json!(u));
    }
    Ok(())
}

pub async fn resource(ctx: &Ctx, action: ResourceAction) -> anyhow::Result<()> {
    match action {
        ResourceAction::Create(args) => create(ctx, args).await,
        ResourceAction::Update(args) => update(ctx, args).await,
        ResourceAction::Delete { id, scope } => delete(ctx, id, &scope).await,
        ResourceAction::List(filter) => views::resources(ctx, filter.tags).await,
        ResourceAction::Show { id, scope } => views::show(ctx, &id, &scope).await,
    }
}

pub async fn unit(ctx: &Ctx, action: UnitAction) -> anyhow::Result<()> {
    match action {
        UnitAction::Add(args) => add_unit(ctx, args).await,
        UnitAction::Update(args) => update_unit(ctx, args).await,
        UnitAction::Remove(r) => remove_unit(ctx, r, "remove_unit").await,
        UnitAction::Retire(r) => remove_unit(ctx, r, "retire_unit").await,
    }
}

async fn create(ctx: &Ctx, args: CreateArgs) -> anyhow::Result<()> {
    let doc: CreateDoc = input::load(&InputSpec::parse(args.file.as_deref()))?.unwrap_or_default();
    let caller = ctx.caller(CallerKind::Interactive)?;
    let ty = once("type", "--type", args.ty.map(ResourceType::from), doc.ty)?.ok_or_else(|| {
        UsageError(
            "Give the resource type with --type device|service|generic (or `type:` in the YAML)."
                .to_owned(),
        )
    })?;
    let by_doc = doc
        .scope
        .map(|scope| match scope {
            ScopeArg::User => Ok(Scope::User),
            ScopeArg::Project => caller.project.clone().map(Scope::Project).ok_or_else(|| {
                UsageError(
                    "Cannot use scope project: the working directory resolves to no project. \
                     Run clerk from inside the project, or use scope user."
                        .to_owned(),
                )
            }),
        })
        .transpose()?;
    let scope = match once("scope", "--scope", args.scope.map(|s| s.scope()), by_doc)? {
        Some(scope) => scope,
        // An agent registers what it finds for its own project by default.
        None if caller::is_agent(&caller) => {
            caller.project.clone().map_or(Scope::User, Scope::Project)
        }
        None => Scope::User,
    };
    let mut a = Map::new();
    a.insert("id".into(), json!(args.id));
    a.insert("type".into(), json!(ty));
    a.insert("scope".into(), json!(scope));
    put_text(&mut a, &args.text, doc.description, doc.usage)?;
    if let Some(tags) = once("tags", "--tag", non_empty(&args.tags), doc.tags)? {
        a.insert("tags".into(), json!(tags));
    }
    if let Some(health) = once(
        "health",
        "--health/--health-command",
        args.health.spec(),
        doc.health,
    )? {
        a.insert("health".into(), json!(health));
    }
    let result = ctx
        .client()
        .await?
        .call("create_resource", &caller, Value::Object(a))
        .await?;
    ctx.out.emit(&View::new(result, |v| {
        let r = &v["resource"];
        let mut text = format!(
            "Created resource {} ({}).",
            views::text(&r["id"]),
            views::scope_text(&r["scope"])
        );
        text.push_str(&warnings(&v["warnings"]));
        text
    }))
}

async fn update(ctx: &Ctx, args: UpdateArgs) -> anyhow::Result<()> {
    let doc: UpdateDoc = input::load(&InputSpec::parse(args.file.as_deref()))?.unwrap_or_default();
    let caller = ctx.caller(CallerKind::Interactive)?;
    let mut a = Map::new();
    a.insert("id".into(), json!(args.id));
    if let Some(scope) = &args.scope.scope {
        a.insert("scope".into(), json!(scope.to_string()));
    }
    put_text(&mut a, &args.text, doc.description, doc.usage)?;
    let lists = [
        (
            "tags",
            once("tags", "--tag", non_empty(&args.tags), doc.tags)?,
        ),
        (
            "add_tags",
            once(
                "add_tags",
                "--add-tag",
                non_empty(&args.add_tags),
                doc.add_tags,
            )?,
        ),
        (
            "remove_tags",
            once(
                "remove_tags",
                "--remove-tag",
                non_empty(&args.remove_tags),
                doc.remove_tags,
            )?,
        ),
    ];
    for (key, value) in lists {
        if let Some(value) = value {
            a.insert(key.into(), json!(value));
        }
    }
    if let Some(health) = once(
        "health",
        "--health/--health-command",
        args.health.spec().map(Some),
        doc.health,
    )? {
        a.insert("health".into(), json!(health));
    }
    let result = ctx
        .client()
        .await?
        .call("update_resource", &caller, Value::Object(a))
        .await?;
    ctx.out.emit(&View::new(result, |v| {
        format!(
            "Updated resource {}.\n{}",
            views::text(&v["id"]),
            views::resource_text(v)
        )
    }))
}

async fn delete(ctx: &Ctx, id: String, scope: &ScopeFlag) -> anyhow::Result<()> {
    let caller = ctx.caller(CallerKind::Interactive)?;
    let mut a = json!({ "id": id });
    scope.put(&mut a);
    ctx.client()
        .await?
        .call("delete_resource", &caller, a)
        .await?;
    ctx.out
        .emit(&View::new(json!({"deleted": {"resource": id}}), |v| {
            format!(
                "Deleted resource {}.",
                views::text(&v["deleted"]["resource"])
            )
        }))
}

/// The unit fields from the flags and the document, merged.
struct UnitFields {
    path: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    mode: Option<ServiceMode>,
    scheme: Option<String>,
    service_id: Option<String>,
    identity_key: Option<String>,
    description: Option<String>,
}

impl UnitFields {
    fn merge(args: &UnitArgs, doc: UnitDoc) -> Result<Self, UsageError> {
        Ok(Self {
            path: once("path", "--path", args.path.clone(), doc.path)?,
            host: once("host", "--host", args.host.clone(), doc.host)?,
            port: once("port", "--port", args.port, doc.port)?,
            mode: once(
                "mode",
                "--listen",
                args.listen.then_some(ServiceMode::Listen),
                doc.mode,
            )?,
            scheme: once("scheme", "--scheme", args.scheme.clone(), doc.scheme)?,
            service_id: once(
                "service_id",
                "--service-id",
                args.service_id.clone(),
                doc.service_id,
            )?,
            identity_key: once("identity_key", "--key", args.key.clone(), doc.identity_key)?,
            description: once(
                "description",
                "--description",
                args.description.clone(),
                doc.description,
            )?,
        })
    }

    /// The settings kinds the given fields belong to.
    fn kinds(&self) -> Vec<ResourceType> {
        let mut kinds = Vec::new();
        if self.path.is_some() {
            kinds.push(ResourceType::Device);
        }
        if self.host.is_some()
            || self.port.is_some()
            || self.mode.is_some()
            || self.scheme.is_some()
            || self.service_id.is_some()
        {
            kinds.push(ResourceType::Service);
        }
        if self.identity_key.is_some() {
            kinds.push(ResourceType::Generic);
        }
        kinds
    }

    /// The one settings kind given, if any.
    fn kind(&self) -> Result<Option<ResourceType>, UsageError> {
        match self.kinds().as_slice() {
            [] => Ok(None),
            [kind] => Ok(Some(*kind)),
            _ => Err(UsageError(
                "The unit settings mix kinds: give --path for a device, --host and --port for a \
                 service, or --key for a generic resource."
                    .to_owned(),
            )),
        }
    }

    /// The given settings fields as wire keys and values.
    fn settings_fields(&self) -> Map<String, Value> {
        let mut m = Map::new();
        let mut put = |k: &str, v: Option<Value>| {
            if let Some(v) = v {
                m.insert(k.to_owned(), v);
            }
        };
        put("path", self.path.as_ref().map(|p| json!(p)));
        put("host", self.host.as_ref().map(|h| json!(h)));
        put("port", self.port.map(|p| json!(p)));
        put("mode", self.mode.map(|m| json!(m)));
        put("scheme", self.scheme.as_ref().map(|s| json!(s)));
        put("service_id", self.service_id.as_ref().map(|s| json!(s)));
        put("identity_key", self.identity_key.as_ref().map(|k| json!(k)));
        m
    }

    /// Complete settings for a new unit.
    fn new_settings(&self) -> Result<Value, UsageError> {
        let Some(kind) = self.kind()? else {
            return Err(UsageError(
                "Give the unit's settings: --path for a device, --host and --port for a service, \
                 or --key for a generic resource."
                    .to_owned(),
            ));
        };
        let mut m = self.settings_fields();
        if kind == ResourceType::Service {
            if self.host.is_none() || self.port.is_none() {
                return Err(UsageError(
                    "A service unit needs both --host and --port.".to_owned(),
                ));
            }
            m.entry("mode").or_insert(json!(ServiceMode::Connect));
            m.entry("scheme").or_insert(Value::Null);
            m.entry("service_id").or_insert(Value::Null);
        }
        m.insert("type".into(), json!(kind));
        Ok(Value::Object(m))
    }
}

async fn add_unit(ctx: &Ctx, args: UnitArgs) -> anyhow::Result<()> {
    let doc: UnitDoc = input::load(&InputSpec::parse(args.file.as_deref()))?.unwrap_or_default();
    let fields = UnitFields::merge(&args, doc)?;
    let settings = fields.new_settings()?;
    let caller = ctx.caller(CallerKind::Interactive)?;
    let mut a = json!({"resource": args.resource, "id": args.unit, "settings": settings});
    args.scope.put(&mut a);
    if let Some(description) = &fields.description {
        a["description"] = json!(description);
    }
    let result = ctx.client().await?.call("add_unit", &caller, a).await?;
    let name = format!("{}/{}", args.resource, args.unit);
    ctx.out.emit(&View::new(result, move |v| {
        let mut text = format!(
            "Added unit {name}: {}.",
            views::settings_text(&v["unit"]["settings"])
        );
        text.push_str(&warnings(&v["warnings"]));
        text
    }))
}

async fn update_unit(ctx: &Ctx, args: UnitArgs) -> anyhow::Result<()> {
    let doc: UnitDoc = input::load(&InputSpec::parse(args.file.as_deref()))?.unwrap_or_default();
    let fields = UnitFields::merge(&args, doc)?;
    let kind = fields.kind()?;
    let caller = ctx.caller(CallerKind::Interactive)?;
    let client = ctx.client().await?;
    let mut a = json!({"resource": args.resource, "unit": args.unit});
    args.scope.put(&mut a);
    if let Some(kind) = kind {
        // The daemon replaces settings whole: start from the current ones.
        let mut get = json!({ "id": args.resource });
        args.scope.put(&mut get);
        let resource = client.call("get_resource", &caller, get).await?;
        let current = resource["units"]
            .as_array()
            .and_then(|units| units.iter().find(|u| u["id"] == args.unit.as_str()))
            .map(|u| u["settings"].clone())
            .ok_or_else(|| ApiError {
                code: "not_found".to_owned(),
                message: format!(
                    "No unit {}/{} is visible to you; check `clerk resource show {}`.",
                    args.resource, args.unit, args.resource
                ),
                details: None,
            })?;
        let Value::Object(mut settings) = current else {
            anyhow::bail!("the daemon sent unit settings that are not an object");
        };
        if settings.get("type") != Some(&json!(kind)) {
            return Err(UsageError(format!(
                "Unit {}/{} is a {} unit; those settings do not apply to it.",
                args.resource,
                args.unit,
                views::text(settings.get("type").unwrap_or(&Value::Null))
            ))
            .into());
        }
        settings.extend(fields.settings_fields());
        a["settings"] = Value::Object(settings);
    }
    if let Some(description) = &fields.description {
        a["description"] = json!(description);
    }
    let result = client.call("update_unit", &caller, a).await?;
    let name = format!("{}/{}", args.resource, args.unit);
    ctx.out.emit(&View::new(result, move |v| {
        let mut text = format!(
            "Updated unit {name}: {}.",
            views::settings_text(&v["unit"]["settings"])
        );
        text.push_str(&warnings(&v["warnings"]));
        text
    }))
}

async fn remove_unit(ctx: &Ctx, r: UnitRef, method: &'static str) -> anyhow::Result<()> {
    let caller = ctx.caller(CallerKind::Interactive)?;
    let mut a = json!({"resource": r.resource, "unit": r.unit});
    r.scope.put(&mut a);
    let client = ctx.client().await?;
    client.call(method, &caller, a).await?;
    let done = if method == "retire_unit" {
        "retired"
    } else {
        "removed"
    };
    let mut value = json!({ done: {"resource": r.resource, "unit": r.unit} });
    if done == "retired" {
        // A unit in use stays, flagged, until its lease ends.
        let mut get = json!({ "id": r.resource });
        r.scope.put(&mut get);
        let resource = client.call("get_resource", &caller, get).await?;
        let stays = resource["units"]
            .as_array()
            .is_some_and(|units| units.iter().any(|u| u["id"] == r.unit.as_str()));
        value[done]["removed"] = json!(!stays);
    }
    ctx.out.emit(&View::new(value, move |v| {
        let u = &v[done];
        let name = format!(
            "{}/{}",
            views::text(&u["resource"]),
            views::text(&u["unit"])
        );
        if done != "retired" {
            format!("Removed unit {name}.")
        } else if u["removed"] == false {
            format!("Retired unit {name}: it is in use, so it goes once its lease ends.")
        } else {
            format!("Retired unit {name}: it was removed now.")
        }
    }))
}

/// One `warning: …` line per warning, each on a new line.
pub fn warnings(list: &Value) -> String {
    let mut text = String::new();
    for w in list.as_array().into_iter().flatten() {
        let t = views::text;
        let line = match w["kind"].as_str().unwrap_or_default() {
            "shadows_user_resource" => format!(
                "this project resource hides the user resource {} inside this project",
                t(&w["id"])
            ),
            "shadowed_by_project" => format!(
                "project {} has its own {}, which hides this one there",
                t(&w["project"]),
                t(&w["id"])
            ),
            "duplicate_of_user_unit" => format!(
                "this unit looks like the user unit {}/{} ({}); they are linked as one thing",
                t(&w["resource"]),
                t(&w["unit"]),
                t(&w["unit_description"])
            ),
            "duplicate_of_project_unit" => format!(
                "this unit looks like {}/{} of your project ({}); they are linked as one thing",
                t(&w["resource"]),
                t(&w["unit"]),
                t(&w["unit_description"])
            ),
            "similar_in_other_project" => format!(
                "a unit in project {} looks like this one; to share it, run `clerk approve {}`",
                t(&w["project"]),
                t(&w["promotion_id"])
            ),
            _ => w.to_string(),
        };
        let _ = write!(text, "\nwarning: {line}");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields() -> UnitFields {
        UnitFields {
            path: None,
            host: None,
            port: None,
            mode: None,
            scheme: None,
            service_id: None,
            identity_key: None,
            description: None,
        }
    }

    #[test]
    fn once_refuses_a_field_given_twice() {
        let err = once("usage", "--usage", Some(1), Some(2)).unwrap_err();
        assert!(
            err.0.contains("usage is given both as --usage"),
            "{}",
            err.0
        );
    }

    #[test]
    fn once_takes_either_side() {
        assert_eq!(once("x", "--x", None, Some(2)).unwrap(), Some(2));
        assert_eq!(once("x", "--x", Some(1), None).unwrap(), Some(1));
    }

    #[test]
    fn service_settings_default_to_connect() {
        let f = UnitFields {
            host: Some("h".into()),
            port: Some(1),
            ..fields()
        };
        assert_eq!(
            f.new_settings().unwrap(),
            json!({"type": "service", "mode": "connect", "scheme": null, "host": "h",
                   "port": 1, "service_id": null})
        );
    }

    #[test]
    fn service_settings_need_host_and_port() {
        let f = UnitFields {
            host: Some("h".into()),
            ..fields()
        };
        assert!(f.new_settings().is_err());
    }

    #[test]
    fn mixed_kinds_are_refused() {
        let f = UnitFields {
            path: Some("/dev/x".into()),
            identity_key: Some("k".into()),
            ..fields()
        };
        assert!(f.kind().is_err());
    }

    #[test]
    fn update_doc_tells_null_health_from_absent() {
        let opts = serde_saphyr::options! { no_schema: true };
        let reset: UpdateDoc =
            serde_saphyr::from_str_with_options("health: null\n", opts.clone()).unwrap();
        assert_eq!(reset.health, Some(None));
        let absent: UpdateDoc = serde_saphyr::from_str_with_options("usage: x\n", opts).unwrap();
        assert_eq!(absent.health, None);
    }
}
