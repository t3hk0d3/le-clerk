//! RPC commands: one variant per operation, with explicit argument structs.
//!
//! Argument structs are the phase-2 request contract. They reject unknown
//! fields, take durations as integer milliseconds (`duration_ms`) and a
//! target as `{"resource": "r"}` or `{"resource": "r", "unit": "u"}`.

use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};

use crate::admin::{Admin, ExportDoc, HolderSpec, ResourceRef, ScopeSpec, UnitRef};
use crate::engine::ApiError;
use crate::engine::wire::{
    AcquireView, AuditView, CreatedResource, CreatedUnit, LeaseView, MyLeasesView, PromotionView,
    ResourceView,
};
use crate::inventory::{
    Inventory, InventoryError, NewResource, NewUnit, Promotions, ResourcePatch, UnitPatch, View,
};
use crate::model::{
    Caller, HealthSpec, HolderRef, Host, LeaseId, NoticeId, Priority, ProjectKey, PromotionId,
    RequestId, ResourceId, ResourceType, Scope, UnitId, UnitSettings,
};
use crate::scheduler::session::{Hosting, Sessions};
use crate::scheduler::{AcquireRequest, Confirm, LeaseEdit, Scheduler, Target};
use crate::store::inventory::{self as inventory_sql, ScopeFilter};
use crate::store::leases as leases_sql;
use crate::store::{Tx, audit, notices};

/// Which resources a listing or status covers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewArg {
    /// User scope plus the caller's project.
    #[default]
    Caller,
    /// Every scope. Refused to agent sessions (`scope_not_allowed`).
    All,
}

impl From<ViewArg> for View {
    fn from(v: ViewArg) -> Self {
        match v {
            ViewArg::Caller => View::Caller,
            ViewArg::All => View::All,
        }
    }
}

/// What a request asks for: a resource, or one of its units.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetArg {
    pub resource: ResourceId,
    #[serde(default)]
    pub unit: Option<UnitId>,
}

impl From<TargetArg> for Target {
    fn from(t: TargetArg) -> Self {
        match t.unit {
            Some(unit) => Target::Unit(t.resource, unit),
            None => Target::Resource(t.resource),
        }
    }
}

/// No arguments: `{}` (or a missing `args`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoArgs {}

/// `create_resource`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateResourceArgs {
    pub id: ResourceId,
    #[serde(rename = "type")]
    pub ty: ResourceType,
    /// `"user"` or `{"project": "<path>"}`.
    pub scope: Scope,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub usage: String,
    /// Absent means the type default.
    #[serde(default)]
    pub health: Option<HealthSpec>,
    /// Search tags; checked by the inventory.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// `update_resource`. Absent fields are left alone; `"health": null`
/// resets the health checks to the type default. Tags: `tags` replaces the
/// set (`[]` clears it), or `add_tags` / `remove_tags` edit it; not both.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateResourceArgs {
    pub id: ResourceId,
    /// Resolve the id in this scope only (`user` or `project:<path>`);
    /// absent resolves as for the caller. Refused to agent sessions.
    #[serde(default)]
    pub scope: Option<ScopeSpec>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub usage: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub health: Option<Option<HealthSpec>>,
    /// Replaces the whole tag set; `[]` clears it.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub add_tags: Vec<String>,
    #[serde(default)]
    pub remove_tags: Vec<String>,
}

/// `get_resource`, `delete_resource`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceArgs {
    pub id: ResourceId,
    /// Resolve the id in this scope only (`user` or `project:<path>`);
    /// absent resolves as for the caller. Refused to agent sessions.
    #[serde(default)]
    pub scope: Option<ScopeSpec>,
}

/// `status`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewArgs {
    #[serde(default)]
    pub view: ViewArg,
}

/// `list_resources`: a view, narrowed to resources carrying every tag.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListResourcesArgs {
    #[serde(default)]
    pub view: ViewArg,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// `add_unit`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddUnitArgs {
    pub resource: ResourceId,
    /// Resolve the id in this scope only (`user` or `project:<path>`);
    /// absent resolves as for the caller. Refused to agent sessions.
    #[serde(default)]
    pub scope: Option<ScopeSpec>,
    pub id: UnitId,
    pub settings: UnitSettings,
    #[serde(default)]
    pub description: String,
}

/// `update_unit`. Absent fields are left alone.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateUnitArgs {
    pub resource: ResourceId,
    /// Resolve the id in this scope only (`user` or `project:<path>`);
    /// absent resolves as for the caller. Refused to agent sessions.
    #[serde(default)]
    pub scope: Option<ScopeSpec>,
    pub unit: UnitId,
    #[serde(default)]
    pub settings: Option<UnitSettings>,
    #[serde(default)]
    pub description: Option<String>,
}

/// `remove_unit`, `retire_unit`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitArgs {
    pub resource: ResourceId,
    /// Resolve the id in this scope only (`user` or `project:<path>`);
    /// absent resolves as for the caller. Refused to agent sessions.
    #[serde(default)]
    pub scope: Option<ScopeSpec>,
    pub unit: UnitId,
}

/// `approve_promotion`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovePromotionArgs {
    pub id: PromotionId,
    /// Promote under this id instead of the resource's own.
    #[serde(default)]
    pub new_id: Option<ResourceId>,
}

/// `acquire`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcquireArgs {
    pub target: TargetArg,
    /// Expected duration of use; must be positive.
    pub duration_ms: u64,
    #[serde(default)]
    pub interruptible: bool,
    #[serde(default = "normal")]
    pub priority: Priority,
    pub purpose: String,
    #[serde(default)]
    pub override_reason: Option<String>,
    /// Grant the unit without its health checks (a specific unit only).
    #[serde(default)]
    pub skip_checks: bool,
}

/// `daemon_stop`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonStopArgs {
    /// A restart: clients waiting on the daemon reconnect to the next one
    /// instead of giving up. Sent only when true, so a daemon that predates
    /// it still stops.
    #[serde(default)]
    pub restart: bool,
}

/// `claim`, `release`, `refuse_yield`, `reclaim`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseArgs {
    pub lease_id: LeaseId,
}

/// `release`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseArgs {
    pub lease_id: LeaseId,
    /// The command that used the lease failed: end it now, unit dirty
    /// (`command_failed`), with no cleanup window.
    #[serde(default)]
    pub failed: bool,
}

/// `cancel`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestArgs {
    pub request_id: RequestId,
}

/// `extend`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtendArgs {
    pub lease_id: LeaseId,
    /// The new end, unix milliseconds.
    pub ends_at: i64,
}

/// `confirm`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmArgs {
    pub lease_id: LeaseId,
    /// `"clean"` (the default) or `"yielded"`.
    #[serde(default = "clean")]
    pub how: Confirm,
}

/// `request_cleanup_extension`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CleanupExtensionArgs {
    pub lease_id: LeaseId,
    pub reason: String,
}

/// `set_interruptible`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetInterruptibleArgs {
    pub lease_id: LeaseId,
    pub interruptible: bool,
}

/// `list_audit`. Unset fields do not filter.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditArgs {
    #[serde(default)]
    pub since: Option<i64>,
    #[serde(default)]
    pub actor: Option<HolderRef>,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

/// `admin_evict`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvictArgs {
    /// `session:<id>`, `pid:<pid>` or `user:<tag>`.
    pub holder: HolderSpec,
}

/// `admin_link`, `admin_unlink`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkArgs {
    pub a: UnitRef,
    pub b: UnitRef,
}

/// `admin_move`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveArgs {
    pub resource: ResourceId,
    /// The resource's current scope; absent resolves as for the caller.
    #[serde(default)]
    pub scope: Option<ScopeSpec>,
    /// `user` or `project:<path>`.
    pub to: ScopeSpec,
}

/// `admin_rename`: the resource, or with `unit` one of its units.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenameArgs {
    pub resource: ResourceId,
    #[serde(default)]
    pub scope: Option<ScopeSpec>,
    #[serde(default)]
    pub unit: Option<UnitId>,
    pub new_id: String,
}

/// `admin_edit_lease`. Absent fields are left alone.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditLeaseArgs {
    pub lease_id: LeaseId,
    /// The new end, unix milliseconds.
    #[serde(default)]
    pub ends_at: Option<i64>,
    /// Takes effect for queue and yield decisions at the next dispatch (the
    /// engine dispatches right after the command), not retroactively: a
    /// yield already asked or refused is not re-decided.
    #[serde(default)]
    pub priority: Option<Priority>,
    #[serde(default)]
    pub interruptible: Option<bool>,
}

/// `admin_doctor`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DoctorArgs {
    /// Repair what can be repaired.
    #[serde(default)]
    pub fix: bool,
}

/// `admin_import`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportArgs {
    /// A document as `admin_export` returns it.
    pub doc: ExportDoc,
    /// Overwrite resources that already exist.
    #[serde(default)]
    pub replace: bool,
}

/// `rehome_holder`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RehomeArgs {
    pub lease_id: LeaseId,
    pub new_holder: HolderRef,
}

fn normal() -> Priority {
    Priority::Normal
}

fn clean() -> Confirm {
    Confirm::Clean
}

/// A field that is present (even as `null`) is `Some`; serde's `default`
/// makes an absent one `None`.
// Three states (absent / null / value) are the point; mirrors
// `ResourcePatch::health`.
#[allow(clippy::option_option)]
fn present<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// Declares [`Command`] and its method table from one list, so the enum,
/// the method names and the read-only flags cannot drift apart.
macro_rules! commands {
    ($( $(#[$doc:meta])* $variant:ident($args:ty) = $name:literal, read_only: $ro:literal; )*) => {
        /// One RPC operation, carrying the caller and its arguments.
        #[derive(Debug, Clone)]
        pub enum Command {
            $( $(#[$doc])* $variant(Caller, $args), )*
        }

        impl Command {
            /// Every RPC method name, in declaration order.
            pub const METHODS: &'static [&'static str] = &[$( $name, )*];

            /// Build the command for `method` from its JSON `args` (`null`
            /// counts as `{}`).
            ///
            /// # Errors
            /// `unknown_method` for a name not in [`Command::METHODS`];
            /// `invalid_request` when `args` does not fit the method;
            /// `invalid_argument` for a value that parses but is not allowed.
            pub fn parse(method: &str, caller: Caller, args: Value) -> Result<Self, ApiError> {
                let command = match method {
                    $( $name => Self::$variant(caller, parse_args(method, args)?), )*
                    _ => return Err(ApiError::unknown_method(method)),
                };
                command.validate()?;
                Ok(command)
            }

            /// The `snake_case` RPC method name.
            #[must_use]
            pub fn method(&self) -> &'static str {
                match self { $( Self::$variant(..) => $name, )* }
            }

            /// Who is calling.
            #[must_use]
            pub fn caller(&self) -> &Caller {
                match self { $( Self::$variant(caller, _) => caller, )* }
            }

            /// Who is calling, to resolve it (the engine follows an agent
            /// session's resume aliases).
            pub fn caller_mut(&mut self) -> &mut Caller {
                match self { $( Self::$variant(caller, _) => caller, )* }
            }

            /// The read-only flag declared in the table.
            fn declared_read_only(&self) -> bool {
                match self { $( Self::$variant(..) => $ro, )* }
            }
        }
    };
}

commands! {
    /// Create a resource.
    CreateResource(CreateResourceArgs) = "create_resource", read_only: false;
    /// Change a resource's description, usage, health checks or tags.
    UpdateResource(UpdateResourceArgs) = "update_resource", read_only: false;
    /// Delete a free resource.
    DeleteResource(ResourceArgs) = "delete_resource", read_only: false;
    /// List resources.
    ListResources(ListResourcesArgs) = "list_resources", read_only: true;
    /// One resource with its units.
    GetResource(ResourceArgs) = "get_resource", read_only: true;
    /// Add a unit.
    AddUnit(AddUnitArgs) = "add_unit", read_only: false;
    /// Change a unit's settings or description.
    UpdateUnit(UpdateUnitArgs) = "update_unit", read_only: false;
    /// Remove a free unit.
    RemoveUnit(UnitArgs) = "remove_unit", read_only: false;
    /// Retire a unit (remove now, or once its lease ends).
    RetireUnit(UnitArgs) = "retire_unit", read_only: false;
    /// Pending promotions.
    ListPromotions(NoArgs) = "list_promotions", read_only: true;
    /// Approve a promotion.
    ApprovePromotion(ApprovePromotionArgs) = "approve_promotion", read_only: false;
    /// Ask for a unit.
    Acquire(AcquireArgs) = "acquire", read_only: false;
    /// Claim an offered lease.
    Claim(LeaseArgs) = "claim", read_only: false;
    /// Withdraw a queued request.
    Cancel(RequestArgs) = "cancel", read_only: false;
    /// Move a lease's end.
    Extend(ExtendArgs) = "extend", read_only: false;
    /// Give a lease back; its cleanup window starts.
    Release(ReleaseArgs) = "release", read_only: false;
    /// One of the caller's leases, in any state.
    GetLease(LeaseArgs) = "get_lease", read_only: true;
    /// The caller's live leases and queued requests.
    ListMyLeases(NoArgs) = "list_my_leases", read_only: true;
    /// The `User` holder of a live lease (anything else is `not_found`).
    /// Refused to agent sessions.
    LeaseHolder(LeaseArgs) = "lease_holder", read_only: true;
    /// The `User` holder of a queued request or pending offer (anything
    /// else is `not_found`). Refused to agent sessions.
    RequestHolder(RequestArgs) = "request_holder", read_only: true;
    /// Confirm cleanup (or a yield).
    Confirm(ConfirmArgs) = "confirm", read_only: false;
    /// Use the one cleanup extension.
    RequestCleanupExtension(CleanupExtensionArgs) = "request_cleanup_extension", read_only: false;
    /// Open or close a lease to interruption.
    SetInterruptible(SetInterruptibleArgs) = "set_interruptible", read_only: false;
    /// Refuse a yield request.
    RefuseYield(LeaseArgs) = "refuse_yield", read_only: false;
    /// Take a lent unit back from an overrunning borrower.
    Reclaim(LeaseArgs) = "reclaim", read_only: false;
    /// Units, holders and queues. `view: "all"` is refused to agent
    /// sessions (see [`ViewArg::All`]).
    Status(ViewArgs) = "status", read_only: true;
    /// Audit log entries, newest first.
    ListAudit(AuditArgs) = "list_audit", read_only: true;
    /// Stop the engine after answering (used by `clerk daemon stop`).
    DaemonStop(DaemonStopArgs) = "daemon_stop", read_only: false;
    /// End a lease now, unit dirty (`admin_forced`).
    AdminRelease(LeaseArgs) = "admin_release", read_only: false;
    /// End every lease and request of a holder.
    AdminEvict(EvictArgs) = "admin_evict", read_only: false;
    /// Remove one queued request.
    AdminDequeue(RequestArgs) = "admin_dequeue", read_only: false;
    /// Remove every queued request of a resource.
    AdminClearQueue(ResourceRef) = "admin_clear_queue", read_only: false;
    /// Remove a unit even if held or queued for.
    AdminRemoveUnit(UnitRef) = "admin_remove_unit", read_only: false;
    /// Delete a resource even if held or queued for.
    AdminDeleteResource(ResourceRef) = "admin_delete_resource", read_only: false;
    /// Clear a unit's dirty flag.
    AdminClean(UnitRef) = "admin_clean", read_only: false;
    /// Link two units as one physical thing.
    AdminLink(LinkArgs) = "admin_link", read_only: false;
    /// Unlink two units and keep them apart.
    AdminUnlink(LinkArgs) = "admin_unlink", read_only: false;
    /// Move a resource to another scope.
    AdminMove(MoveArgs) = "admin_move", read_only: false;
    /// Rename a resource or unit.
    AdminRename(RenameArgs) = "admin_rename", read_only: false;
    /// Change a lease's end, priority or interruptibility.
    AdminEditLease(EditLeaseArgs) = "admin_edit_lease", read_only: false;
    /// Look for (and with `fix`, repair) damage. Read-only without `fix`
    /// (see [`Command::read_only`]).
    AdminDoctor(DoctorArgs) = "admin_doctor", read_only: false;
    /// The inventory as a JSON document.
    AdminExport(NoArgs) = "admin_export", read_only: true;
    /// Load a JSON document made by `admin_export`.
    AdminImport(ImportArgs) = "admin_import", read_only: false;
    /// Hand one of the caller's leases to another holder.
    RehomeHolder(RehomeArgs) = "rehome_holder", read_only: false;
}

fn parse_args<T: DeserializeOwned>(method: &str, args: Value) -> Result<T, ApiError> {
    let args = if args.is_null() { json!({}) } else { args };
    serde_json::from_value(args)
        .map_err(|e| ApiError::invalid_request(format!("bad arguments for {method}: {e}")))
}

/// The services a command runs against.
pub(crate) struct Services<'a> {
    pub inventory: &'a Inventory,
    pub promotions: &'a Promotions,
    pub scheduler: &'a Scheduler,
    pub now_ms: i64,
    /// An acquire answers `checking` rather than queue behind a check on
    /// its way.
    pub defer_on_check: bool,
}

impl Command {
    /// True when the command never changes scheduling state, so the engine
    /// need not tick and dispatch after it. (Admin reads still write their
    /// audit entry.)
    #[must_use]
    pub fn read_only(&self) -> bool {
        match self {
            Self::AdminDoctor(_, a) => !a.fix,
            _ => self.declared_read_only(),
        }
    }

    /// Checks serde cannot express.
    fn validate(&self) -> Result<(), ApiError> {
        if let Self::Acquire(_, args) = self
            && args.duration_ms == 0
        {
            return Err(ApiError::invalid_argument("duration_ms must be positive"));
        }
        if let Self::Acquire(_, args) = self
            && args.skip_checks
            && args.target.unit.is_none()
        {
            return Err(ApiError::invalid_argument(
                "skip_checks needs a specific unit (<resource>/<unit>): you vouch for that one \
                 unit, not for any unit of the resource",
            ));
        }
        if matches!(self.caller().holder, HolderRef::AgentSession { .. })
            && let Some(what) = self.beyond_own_scopes()
        {
            return Err(ApiError::scope_not_allowed(what));
        }
        Ok(())
    }

    /// Checks that need the liveness record: an agent session may hold or
    /// be handed a lease only while a running process is known to run it,
    /// so that its leases end when that process does. `hosting` is what the
    /// call said about the caller, when it is an agent session.
    pub(crate) fn validate_hosting(
        &self,
        hosting: Option<&Hosting>,
        sessions: &Sessions,
    ) -> Result<(), ApiError> {
        if !matches!(
            self,
            Self::Acquire(..) | Self::Claim(..) | Self::RehomeHolder(..)
        ) {
            return Ok(());
        }
        if let (HolderRef::AgentSession { session_id }, Some(hosting)) =
            (&self.caller().holder, hosting)
            && *hosting != Hosting::Hosted
        {
            return Err(ApiError::session_not_hosted(session_id, hosting));
        }
        if let Self::RehomeHolder(_, args) = self
            && let HolderRef::AgentSession { session_id } = &args.new_holder
            && !sessions.is_alive(session_id)
        {
            return Err(ApiError::session_not_hosted(
                session_id,
                &Hosting::NotHosted,
            ));
        }
        Ok(())
    }

    /// What this command reaches beyond the caller's own scopes and
    /// holder, if anything: agent sessions are refused it.
    fn beyond_own_scopes(&self) -> Option<&'static str> {
        match self {
            Self::Status(_, a) if a.view == ViewArg::All => Some("view every scope"),
            Self::ListResources(_, a) if a.view == ViewArg::All => Some("list every scope"),
            Self::LeaseHolder(..) => Some("look up the user's lease holders"),
            Self::UpdateResource(_, UpdateResourceArgs { scope: Some(_), .. })
            | Self::GetResource(_, ResourceArgs { scope: Some(_), .. })
            | Self::DeleteResource(_, ResourceArgs { scope: Some(_), .. })
            | Self::AddUnit(_, AddUnitArgs { scope: Some(_), .. })
            | Self::UpdateUnit(_, UpdateUnitArgs { scope: Some(_), .. })
            | Self::RemoveUnit(_, UnitArgs { scope: Some(_), .. })
            | Self::RetireUnit(_, UnitArgs { scope: Some(_), .. }) => {
                Some("name a scope explicitly")
            }
            Self::RequestHolder(..) => Some("look up the user's request holders"),
            _ => None,
        }
    }

    /// Run the command in `tx` and encode its result; a `not_found` error
    /// gains details naming the missing thing (`{"kind", "id"}`) when the
    /// command names one.
    pub(crate) fn run(self, tx: &Tx, s: &Services<'_>) -> Result<Value, ApiError> {
        let subject = self.subject();
        let project = self.caller().project.clone();
        self.execute(tx, s).map_err(|mut e| {
            if e.code == "not_found"
                && e.details.is_none()
                && let Some(subject) = subject
            {
                e.details = subject.missing(tx, project.as_ref()).ok();
            }
            e
        })
    }

    /// What the command names, for `not_found` details.
    fn subject(&self) -> Option<Subject> {
        let resource = |id: &ResourceId| Some(Subject::Resource(id.clone()));
        let unit = |r: &ResourceId, u: &UnitId, scope: &Option<ScopeSpec>, admin: bool| {
            Some(Subject::Unit {
                resource: r.clone(),
                unit: u.clone(),
                scope: scope.clone(),
                admin,
            })
        };
        let lease = |id: &LeaseId| Some(Subject::Other("lease", id.as_str().to_owned()));
        let request = |id: &RequestId| Some(Subject::Other("request", id.as_str().to_owned()));
        match self {
            Self::UpdateResource(_, a) => resource(&a.id),
            Self::DeleteResource(_, a) | Self::GetResource(_, a) => resource(&a.id),
            Self::AddUnit(_, a) => resource(&a.resource),
            Self::UpdateUnit(_, a) => unit(&a.resource, &a.unit, &a.scope, false),
            Self::RemoveUnit(_, a) | Self::RetireUnit(_, a) => {
                unit(&a.resource, &a.unit, &a.scope, false)
            }
            Self::ApprovePromotion(_, a) => {
                Some(Subject::Other("promotion", a.id.as_str().to_owned()))
            }
            Self::Acquire(_, a) => match &a.target.unit {
                Some(u) => unit(&a.target.resource, u, &None, false),
                None => resource(&a.target.resource),
            },
            Self::Claim(_, a)
            | Self::GetLease(_, a)
            | Self::LeaseHolder(_, a)
            | Self::RefuseYield(_, a)
            | Self::Reclaim(_, a)
            | Self::AdminRelease(_, a) => lease(&a.lease_id),
            Self::Extend(_, a) => lease(&a.lease_id),
            Self::Release(_, a) => lease(&a.lease_id),
            Self::Confirm(_, a) => lease(&a.lease_id),
            Self::RequestCleanupExtension(_, a) => lease(&a.lease_id),
            Self::SetInterruptible(_, a) => lease(&a.lease_id),
            Self::RehomeHolder(_, a) => lease(&a.lease_id),
            Self::AdminEditLease(_, a) => lease(&a.lease_id),
            Self::Cancel(_, a) | Self::RequestHolder(_, a) | Self::AdminDequeue(_, a) => {
                request(&a.request_id)
            }
            Self::AdminClearQueue(_, a) | Self::AdminDeleteResource(_, a) => resource(&a.resource),
            Self::AdminRemoveUnit(_, a) | Self::AdminClean(_, a) => {
                unit(&a.resource, &a.unit, &a.scope, true)
            }
            Self::AdminMove(_, a) => resource(&a.resource),
            Self::AdminRename(_, a) => match &a.unit {
                Some(u) => unit(&a.resource, u, &a.scope, true),
                None => resource(&a.resource),
            },
            _ => None,
        }
    }

    /// Run the command in `tx` and encode its result. `DaemonStop` only
    /// records the request; stopping is the engine's job.
    // One flat arm per command reads better than splitting the table.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn execute(self, tx: &Tx, s: &Services<'_>) -> Result<Value, ApiError> {
        let inv = s.inventory;
        let sched = s.scheduler;
        match self {
            Self::CreateResource(who, a) => {
                let new = NewResource {
                    id: a.id,
                    ty: a.ty,
                    scope: a.scope,
                    description: a.description,
                    usage: a.usage,
                    health: a.health,
                    tags: a.tags,
                };
                encode(CreatedResource::from(inv.create_resource(tx, &who, &new)?))
            }
            Self::UpdateResource(who, a) => {
                let patch = ResourcePatch {
                    description: a.description,
                    usage: a.usage,
                    health: a.health,
                    tags: a.tags,
                    add_tags: a.add_tags,
                    remove_tags: a.remove_tags,
                };
                let who = scoped(tx, who, a.scope.as_ref(), &a.id)?;
                let updated = inv.update_resource(tx, &who, &a.id, patch)?;
                let rows: Vec<i64> = updated.units.iter().map(|u| u.row).collect();
                sched.forget_health(&rows);
                encode(ResourceView::from(updated))
            }
            Self::DeleteResource(who, a) => {
                let who = scoped(tx, who, a.scope.as_ref(), &a.id)?;
                inv.delete_resource(tx, &who, &a.id)?;
                Ok(Value::Null)
            }
            Self::ListResources(who, a) => encode(
                inv.list_resources(tx, &who, a.view.into(), &a.tags)?
                    .into_iter()
                    .map(ResourceView::from)
                    .collect::<Vec<_>>(),
            ),
            Self::GetResource(who, a) => {
                let who = scoped(tx, who, a.scope.as_ref(), &a.id)?;
                encode(ResourceView::from(inv.get_resource(tx, &who, &a.id)?))
            }
            Self::AddUnit(who, a) => {
                let who = scoped(tx, who, a.scope.as_ref(), &a.resource)?;
                let new = NewUnit {
                    id: a.id,
                    settings: a.settings,
                    description: a.description,
                };
                encode(CreatedUnit::from(inv.add_unit(
                    tx,
                    &who,
                    &a.resource,
                    &new,
                )?))
            }
            Self::UpdateUnit(who, a) => {
                let patch = UnitPatch {
                    settings: a.settings,
                    description: a.description,
                };
                let who = scoped(tx, who, a.scope.as_ref(), &a.resource)?;
                let updated = inv.update_unit(tx, &who, &a.resource, &a.unit, patch)?;
                sched.forget_health(&[updated.value.row]);
                encode(CreatedUnit::from(updated))
            }
            Self::RemoveUnit(who, a) => {
                let who = scoped(tx, who, a.scope.as_ref(), &a.resource)?;
                inv.remove_unit(tx, &who, &a.resource, &a.unit)?;
                Ok(Value::Null)
            }
            Self::RetireUnit(who, a) => {
                let who = scoped(tx, who, a.scope.as_ref(), &a.resource)?;
                inv.retire_unit(tx, &who, &a.resource, &a.unit)?;
                Ok(Value::Null)
            }
            Self::ListPromotions(_, NoArgs {}) => encode(
                s.promotions
                    .list(tx)?
                    .into_iter()
                    .map(PromotionView::from)
                    .collect::<Vec<_>>(),
            ),
            Self::ApprovePromotion(who, a) => encode(ResourceView::from(
                s.promotions.approve(tx, &who, a.id, a.new_id)?,
            )),
            Self::Acquire(who, a) => {
                let req = AcquireRequest {
                    target: a.target.into(),
                    duration: Duration::from_millis(a.duration_ms),
                    interruptible: a.interruptible,
                    priority: a.priority,
                    purpose: a.purpose,
                    override_reason: a.override_reason,
                    skip_checks: a.skip_checks,
                };
                let outcome = if s.defer_on_check {
                    sched.acquire_unless_checking(tx, &who, req)?
                } else {
                    sched.acquire(tx, &who, req)?
                };
                encode(AcquireView::from(outcome))
            }
            Self::Claim(who, a) => encode(LeaseView::from(sched.claim(tx, &who, &a.lease_id)?)),
            Self::Cancel(who, a) => {
                sched.cancel(tx, &who, &a.request_id)?;
                Ok(Value::Null)
            }
            Self::Extend(who, a) => encode(LeaseView::from(sched.extend(
                tx,
                &who,
                &a.lease_id,
                a.ends_at,
            )?)),
            Self::Release(who, a) => {
                let lease = if a.failed {
                    sched.release_failed(tx, &who, &a.lease_id)?
                } else {
                    sched.release(tx, &who, &a.lease_id)?
                };
                encode(LeaseView::from(lease))
            }
            Self::GetLease(who, a) => {
                encode(LeaseView::from(sched.get_lease(tx, &who, &a.lease_id)?))
            }
            Self::LeaseHolder(_, a) => {
                encode(json!({ "holder": sched.user_holder(tx, &a.lease_id)? }))
            }
            Self::RequestHolder(_, a) => {
                encode(json!({ "holder": sched.user_request_holder(tx, &a.request_id)? }))
            }
            Self::ListMyLeases(who, NoArgs {}) => {
                encode(MyLeasesView::from(sched.my_leases(tx, &who)?))
            }
            Self::Confirm(who, a) => {
                sched.confirm(tx, &who, &a.lease_id, a.how)?;
                Ok(Value::Null)
            }
            Self::RequestCleanupExtension(who, a) => encode(LeaseView::from(
                sched.request_cleanup_extension(tx, &who, &a.lease_id, &a.reason)?,
            )),
            Self::SetInterruptible(who, a) => encode(LeaseView::from(sched.set_interruptible(
                tx,
                &who,
                &a.lease_id,
                a.interruptible,
            )?)),
            Self::RefuseYield(who, a) => {
                sched.refuse_yield(tx, &who, &a.lease_id)?;
                Ok(Value::Null)
            }
            Self::Reclaim(who, a) => {
                sched.reclaim(tx, &who, &a.lease_id)?;
                Ok(Value::Null)
            }
            Self::Status(who, a) => encode(sched.status(tx, &who, a.view.into())?),
            Self::ListAudit(_, a) => {
                let filter = audit::AuditFilter {
                    since: a.since,
                    actor: a.actor,
                    action: a.action,
                    limit: a.limit,
                };
                encode(
                    audit::list(tx, &filter)?
                        .into_iter()
                        .map(AuditView::from)
                        .collect::<Vec<_>>(),
                )
            }
            Self::DaemonStop(who, a) => {
                audit::record(
                    tx,
                    s.now_ms,
                    &who,
                    "daemon_stop",
                    &json!({ "restart": a.restart }),
                )?;
                Ok(json!({ "stopping": true }))
            }
            Self::RehomeHolder(who, a) => encode(LeaseView::from(sched.rehome_holder(
                tx,
                &who,
                &a.lease_id,
                &a.new_holder,
            )?)),
            admin => admin.execute_admin(tx, s),
        }
    }

    /// The `admin_*` commands.
    fn execute_admin(self, tx: &Tx, s: &Services<'_>) -> Result<Value, ApiError> {
        let admin = Admin::new(s.inventory, s.scheduler, s.now_ms);
        match self {
            Self::AdminRelease(who, a) => {
                admin.release(tx, &who, &a.lease_id)?;
                Ok(Value::Null)
            }
            Self::AdminEvict(who, a) => encode(admin.evict(tx, &who, &a.holder)?),
            Self::AdminDequeue(who, a) => {
                admin.dequeue(tx, &who, &a.request_id)?;
                Ok(Value::Null)
            }
            Self::AdminClearQueue(who, a) => {
                encode(json!({ "requests_dropped": admin.clear_queue(tx, &who, &a)? }))
            }
            Self::AdminRemoveUnit(who, a) => {
                admin.remove_unit(tx, &who, &a)?;
                Ok(Value::Null)
            }
            Self::AdminDeleteResource(who, a) => {
                admin.delete_resource(tx, &who, &a)?;
                Ok(Value::Null)
            }
            Self::AdminClean(who, a) => {
                admin.clean(tx, &who, &a)?;
                Ok(Value::Null)
            }
            Self::AdminLink(who, a) => {
                encode(json!({ "live_leases": admin.link(tx, &who, &a.a, &a.b)? }))
            }
            Self::AdminUnlink(who, a) => {
                encode(json!({ "live_leases": admin.unlink(tx, &who, &a.a, &a.b)? }))
            }
            Self::AdminMove(who, a) => {
                let r = ResourceRef {
                    resource: a.resource,
                    scope: a.scope,
                };
                encode(ResourceView::from(admin.move_resource(
                    tx,
                    &who,
                    &r,
                    &a.to.scope(),
                )?))
            }
            Self::AdminRename(who, a) => {
                let r = ResourceRef {
                    resource: a.resource,
                    scope: a.scope,
                };
                encode(ResourceView::from(admin.rename(
                    tx,
                    &who,
                    &r,
                    a.unit.as_ref(),
                    &a.new_id,
                )?))
            }
            Self::AdminEditLease(who, a) => {
                let edit = LeaseEdit {
                    ends_at: a.ends_at,
                    priority: a.priority,
                    interruptible: a.interruptible,
                };
                encode(LeaseView::from(admin.edit_lease(
                    tx,
                    &who,
                    &a.lease_id,
                    edit,
                )?))
            }
            Self::AdminDoctor(who, a) => encode(admin.doctor(tx, &who, a.fix)?),
            Self::AdminExport(who, NoArgs {}) => encode(admin.export(tx, &who)?),
            Self::AdminImport(who, a) => encode(admin.import(tx, &who, &a.doc, a.replace)?),
            other => Err(ApiError::internal(format!(
                "{} is not an admin command",
                other.method()
            ))),
        }
    }
}

/// The thing a command names, for the details of a `not_found` error.
enum Subject {
    /// A resource.
    Resource(ResourceId),
    /// A unit of a resource; `admin` commands resolve ids in every scope.
    Unit {
        resource: ResourceId,
        unit: UnitId,
        scope: Option<ScopeSpec>,
        admin: bool,
    },
    /// A lease, request or promotion: its kind and id.
    Other(&'static str, String),
}

impl Subject {
    /// `{"kind", "id"}` of what is missing: for a unit, the resource when
    /// that is missing too, else `<resource>/<unit>`.
    fn missing(self, tx: &Tx, project: Option<&ProjectKey>) -> Result<Value, ApiError> {
        let resource_exists = |id: &ResourceId, scope: Option<&ScopeSpec>, admin: bool| {
            let found = match scope {
                Some(spec) => inventory_sql::find_in_scope(tx, &spec.scope(), id)?.is_some(),
                None if admin => inventory_sql::list_resources(tx, ScopeFilter::All)?
                    .iter()
                    .any(|r| &r.id == id),
                None => inventory_sql::find_visible(tx, project, id)?.is_some(),
            };
            Ok::<bool, ApiError>(found)
        };
        Ok(match self {
            Self::Resource(id) => json!({"kind": "resource", "id": id}),
            Self::Unit {
                resource,
                unit,
                scope,
                admin,
            } => {
                if resource_exists(&resource, scope.as_ref(), admin)? {
                    json!({"kind": "unit", "id": format!("{resource}/{unit}")})
                } else {
                    json!({"kind": "resource", "id": resource})
                }
            }
            Self::Other(kind, id) => json!({"kind": kind, "id": id}),
        })
    }
}

/// `who` as seen from an explicit `scope`: the resource id then resolves in
/// that scope and nowhere else. The inventory resolves ids by the caller's
/// project (that project first, then user scope), so the scope stands in
/// for the project once the resource is known to be there. Only the user's
/// CLI sends a scope; agent sessions are refused one (see `validate`).
fn scoped(
    tx: &Tx,
    who: Caller,
    scope: Option<&ScopeSpec>,
    id: &ResourceId,
) -> Result<Caller, ApiError> {
    let Some(spec) = scope else {
        return Ok(who);
    };
    let scope = spec.scope();
    if inventory_sql::find_in_scope(tx, &scope, id)?.is_none() {
        return Err(InventoryError::NotFound.into());
    }
    let project = match scope {
        Scope::User => None,
        Scope::Project(key) => Some(key),
    };
    Ok(Caller { project, ..who })
}

fn encode<T: serde::Serialize>(value: T) -> Result<Value, ApiError> {
    serde_json::to_value(value)
        .map_err(|e| ApiError::internal(format!("cannot encode result: {e}")))
}

// ---------------------------------------------------------------------------
// Hook RPCs
// ---------------------------------------------------------------------------

/// `session_brief`: who a hook speaks for. `session_id` is Claude Code's
/// hook input `session_id`, `claude_pid` the hook's `CLAUDE_PID`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookSessionArgs {
    pub session_id: String,
    /// The Claude Code process running the session (`CLAUDE_PID`).
    #[serde(default)]
    pub host: Option<Host>,
}

/// `mark_notices_shown`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarkShownArgs {
    pub session_id: String,
    /// The Claude Code process running the session (`CLAUDE_PID`).
    #[serde(default)]
    pub host: Option<Host>,
    /// The notices the hook showed.
    pub ids: Vec<NoticeId>,
}

/// `session_start`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionStartArgs {
    pub session_id: String,
    /// The Claude Code process running the session (`CLAUDE_PID`).
    #[serde(default)]
    pub host: Option<Host>,
    /// The hook input's `source` (`startup`, `resume`, `clear`, …).
    pub source: String,
}

/// `session_end`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionEndArgs {
    pub session_id: String,
    /// The Claude Code process running the session (`CLAUDE_PID`).
    #[serde(default)]
    pub host: Option<Host>,
    /// The hook input's `reason` (`clear`, `resume`, `logout`, …).
    pub reason: String,
}

/// An RPC of the Claude Code hooks (`clerk hook …`). Each speaks for the
/// session its `session_id` names, reports its `host` when given
/// ([`Sessions::report`]), and answers only about that session. They need
/// the liveness record, so they are not [`Command`]s.
#[derive(Debug, Clone)]
pub enum HookCommand {
    /// The session's live leases, queued requests, open waits and the
    /// pending notices no hook has shown.
    SessionBrief(HookSessionArgs),
    /// Mark notices shown by a hook; they stay pending.
    MarkNoticesShown(MarkShownArgs),
    /// `SessionStart`: report the host (which unhosts the sessions that
    /// process ran before).
    SessionStart(SessionStartArgs),
    /// `SessionEnd`: end the session's leases and requests now, when the
    /// hook's process is the session's host ([`Sessions::end`]).
    SessionEnd(SessionEndArgs),
}

/// What a [`HookCommand`] answered, for the engine to finish.
#[derive(Debug)]
pub(crate) struct HookAnswer {
    /// The result.
    pub value: Value,
    /// For `session_brief`: the holder whose open waits go in
    /// `open_waits` (counted outside the engine thread).
    pub open_waits_of: Option<HolderRef>,
    /// Scheduling state changed: the engine ticks and dispatches.
    pub changed: bool,
}

impl HookCommand {
    /// Every hook RPC method name.
    pub const METHODS: &'static [&'static str] = &[
        "session_brief",
        "mark_notices_shown",
        "session_start",
        "session_end",
    ];

    /// Build the hook command for `method` from its JSON `args`.
    ///
    /// # Errors
    /// `unknown_method` for a name not in [`HookCommand::METHODS`];
    /// `invalid_request` when `args` does not fit the method.
    pub fn parse(method: &str, args: Value) -> Result<Self, ApiError> {
        Ok(match method {
            "session_brief" => Self::SessionBrief(parse_args(method, args)?),
            "mark_notices_shown" => Self::MarkNoticesShown(parse_args(method, args)?),
            "session_start" => Self::SessionStart(parse_args(method, args)?),
            "session_end" => Self::SessionEnd(parse_args(method, args)?),
            _ => return Err(ApiError::unknown_method(method)),
        })
    }

    /// The `snake_case` RPC method name.
    #[must_use]
    pub fn method(&self) -> &'static str {
        match self {
            Self::SessionBrief(_) => "session_brief",
            Self::MarkNoticesShown(_) => "mark_notices_shown",
            Self::SessionStart(_) => "session_start",
            Self::SessionEnd(_) => "session_end",
        }
    }

    /// Run the hook command in `tx` against the liveness record.
    pub(crate) fn execute(
        self,
        tx: &Tx,
        sessions: &mut Sessions,
        sched: &Scheduler,
        now_ms: i64,
    ) -> Result<HookAnswer, ApiError> {
        match self {
            Self::SessionBrief(a) => {
                if let Some(host) = a.host {
                    sessions.report(tx, &a.session_id, host, now_ms)?;
                }
                let holder = agent_session(&a.session_id);
                let leases = leases_sql::live_leases(tx)?
                    .iter()
                    .filter(|l| l.holder == holder)
                    .count();
                let requests = leases_sql::queued_requests(tx)?
                    .iter()
                    .filter(|r| r.holder == holder)
                    .count();
                // Only these need waking while idle: a plain holder has
                // nothing it must answer (soft expiry keeps its lease).
                let interruptible = leases_sql::live_leases(tx)?
                    .iter()
                    .filter(|l| l.holder == holder && l.interruptible)
                    .count();
                let unshown = notices::unshown(tx, &holder)?;
                Ok(HookAnswer {
                    value: json!({
                        "session": a.session_id,
                        "connected": sessions.is_alive(&a.session_id),
                        "leases": leases,
                        "requests": requests,
                        "interruptible": interruptible,
                        "open_waits": 0,
                        "unshown": unshown,
                    }),
                    open_waits_of: Some(holder),
                    changed: false,
                })
            }
            Self::MarkNoticesShown(a) => {
                let holder = agent_session(&a.session_id);
                let marked = notices::mark_shown(tx, &holder, &a.ids, now_ms)?;
                Ok(HookAnswer {
                    value: json!({ "marked": marked }),
                    open_waits_of: None,
                    changed: false,
                })
            }
            Self::SessionStart(a) => {
                let (unhosted, released) = match a.host {
                    Some(host) => (
                        sessions.report(tx, &a.session_id, host, now_ms)?.unhosted,
                        sessions.take_released(host),
                    ),
                    None => (Vec::new(), Vec::new()),
                };
                Ok(HookAnswer {
                    value: json!({
                        "session": a.session_id,
                        "unhosted": unhosted,
                        "released": released,
                    }),
                    open_waits_of: None,
                    changed: false,
                })
            }
            Self::SessionEnd(a) => {
                let Some(ended) = sessions.end(tx, sched, &a.session_id, a.host, &a.reason)? else {
                    return Ok(HookAnswer {
                        value: json!({ "session": a.session_id, "ended": false }),
                        open_waits_of: None,
                        changed: false,
                    });
                };
                Ok(HookAnswer {
                    value: json!({
                        "session": a.session_id,
                        "ended": true,
                        "leases_ended": ended.leases,
                        "requests_dropped": ended.requests,
                    }),
                    open_waits_of: None,
                    changed: true,
                })
            }
        }
    }
}

fn agent_session(session_id: &str) -> HolderRef {
    HolderRef::AgentSession {
        session_id: session_id.to_owned(),
    }
}
