//! Administrator operations: forced releases and evictions, queue surgery,
//! forced removal, link repair, moves and renames, lease edits, the doctor,
//! and export/import of the inventory.
//!
//! The engine performs no gating: who may run these (and the
//! `--override` rule for agents) is the CLI's job. Every operation
//! writes one audit entry `admin_<op>` with the caller as actor and the
//! caller's `override_reason` text.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

use crate::error::ErrorCode;
use crate::health;
use crate::inventory::{Inventory, InventoryError, NewResource, NewUnit, Warning};
use crate::model::{
    Caller, EndCause, HealthSpec, HolderRef, LeaseId, ProjectKey, RequestId, Resource, ResourceId,
    ResourceType, Scope, Unit, UnitId, UnitSettings,
};
use crate::scheduler::liveness;
use crate::scheduler::{Lease, LeaseEdit, Scheduler, SchedulerError};
use crate::store::inventory::{self as inventory_sql, ScopeFilter};
use crate::store::notices::NoticeKind;
use crate::store::{StoreError, Tx, audit, leases};

/// Which holders an eviction targets, parsed from `session:<id>`,
/// `pid:<pid>` or `user:<tag>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum HolderSpec {
    /// The agent session with this id.
    Session(String),
    /// Every CLI process with this pid, whatever its start time.
    Pid(u32),
    /// The user holder with this tag.
    User(String),
}

impl HolderSpec {
    /// Parse `session:<id>`, `pid:<pid>` or `user:<tag>`.
    ///
    /// # Errors
    /// A message naming the accepted forms.
    pub fn parse(s: &str) -> Result<Self, String> {
        let bad = || format!("{s:?} is not a holder: use session:<id>, pid:<pid> or user:<tag>");
        let (kind, value) = s.split_once(':').ok_or_else(bad)?;
        if value.is_empty() {
            return Err(bad());
        }
        match kind {
            "session" => Ok(Self::Session(value.to_owned())),
            "pid" => value.parse().map(Self::Pid).map_err(|_| bad()),
            "user" => Ok(Self::User(value.to_owned())),
            _ => Err(bad()),
        }
    }

    /// True when `holder` is one this spec names.
    #[must_use]
    pub fn matches(&self, holder: &HolderRef) -> bool {
        match (self, holder) {
            (Self::Session(id), HolderRef::AgentSession { session_id }) => id == session_id,
            (Self::Pid(p), HolderRef::CliProcess { pid, .. }) => p == pid,
            (Self::User(t), HolderRef::User { tag }) => t == tag,
            _ => false,
        }
    }
}

impl fmt::Display for HolderSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Session(id) => write!(f, "session:{id}"),
            Self::Pid(pid) => write!(f, "pid:{pid}"),
            Self::User(tag) => write!(f, "user:{tag}"),
        }
    }
}

impl TryFrom<String> for HolderSpec {
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl From<HolderSpec> for String {
    fn from(h: HolderSpec) -> Self {
        h.to_string()
    }
}

/// A scope written as text: `user` or `project:<path>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ScopeSpec(String);

impl ScopeSpec {
    /// Parse `user` or `project:<path>`.
    ///
    /// # Errors
    /// A message naming the accepted forms.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.strip_prefix("project:") {
            Some(path) if !path.is_empty() => Ok(Self(s.to_owned())),
            None if s == "user" => Ok(Self(s.to_owned())),
            _ => Err(format!("{s:?} is not a scope: use user or project:<path>")),
        }
    }

    /// The scope this text names.
    #[must_use]
    pub fn scope(&self) -> Scope {
        match self.0.strip_prefix("project:") {
            Some(path) => Scope::Project(ProjectKey(PathBuf::from(path))),
            None => Scope::User,
        }
    }
}

impl From<&Scope> for ScopeSpec {
    fn from(scope: &Scope) -> Self {
        match scope {
            Scope::User => Self("user".to_owned()),
            Scope::Project(key) => Self(format!("project:{}", key.0.to_string_lossy())),
        }
    }
}

impl fmt::Display for ScopeSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for ScopeSpec {
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl From<ScopeSpec> for String {
    fn from(s: ScopeSpec) -> Self {
        s.0
    }
}

/// A resource named by an administrator. Without `scope` the id resolves as
/// for the caller: its project first, then user scope.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceRef {
    pub resource: ResourceId,
    #[serde(default)]
    pub scope: Option<ScopeSpec>,
}

/// A unit named by an administrator; `scope` as in [`ResourceRef`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitRef {
    pub resource: ResourceId,
    #[serde(default)]
    pub scope: Option<ScopeSpec>,
    pub unit: UnitId,
}

impl UnitRef {
    fn resource_ref(&self) -> ResourceRef {
        ResourceRef {
            resource: self.resource.clone(),
            scope: self.scope.clone(),
        }
    }
}

/// Why an administrator operation failed.
#[derive(Debug, Error)]
pub enum AdminError {
    /// The target scope already has a resource (or the resource already has
    /// a unit) with that id.
    #[error("{id} is already taken there; pick another id or move the other one first")]
    IdClash { id: String },
    /// An import names resources that already exist.
    #[error("these resources already exist: {}; import with replace to overwrite them", .ids.join(", "))]
    ImportClash { ids: Vec<String> },
    /// A link would join identity groups that each have a live lease, so
    /// one physical thing would have two holders. Nothing was written.
    #[error(
        "linking {} would give one physical thing to two live leases ({}); end one of them first",
        .units.join(" and "),
        .leases.iter().map(LeaseId::as_str).collect::<Vec<_>>().join(", ")
    )]
    LinkConflict {
        /// The two units, as `resource/unit`.
        units: Vec<String>,
        leases: Vec<LeaseId>,
    },
    /// The two units are one thing only through other units; unlinking
    /// them directly would not split anything.
    #[error(
        "these units are not linked directly but through {}; unlink the direct links instead",
        .via.join(", ")
    )]
    NotDirectlyLinked {
        /// The other units of their identity group, as `resource/unit`.
        via: Vec<String>,
    },
    /// No such resource, unit, lease or request.
    #[error("no such resource, unit, lease or request")]
    NotFound,
    /// An argument or import document is not acceptable.
    #[error("{0}")]
    Invalid(String),
    /// An inventory rule refused.
    #[error(transparent)]
    Inventory(#[from] InventoryError),
    /// A scheduler rule refused.
    #[error(transparent)]
    Scheduler(#[from] SchedulerError),
    /// Storage failed.
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl ErrorCode for AdminError {
    fn code(&self) -> &'static str {
        match self {
            AdminError::IdClash { .. } => "id_clash",
            AdminError::ImportClash { .. } => "import_clash",
            AdminError::LinkConflict { .. } => "link_conflict",
            AdminError::NotDirectlyLinked { .. } => "not_directly_linked",
            AdminError::NotFound => "not_found",
            AdminError::Invalid(_) => "invalid_argument",
            AdminError::Inventory(e) => e.code(),
            AdminError::Scheduler(e) => e.code(),
            AdminError::Store(e) => e.code(),
        }
    }

    fn details(&self) -> Option<Value> {
        match self {
            AdminError::ImportClash { ids } => Some(json!({ "ids": ids })),
            AdminError::LinkConflict { leases, .. } => Some(json!({ "leases": leases })),
            AdminError::NotDirectlyLinked { via } => Some(json!({ "via": via })),
            AdminError::Inventory(e) => e.details(),
            AdminError::Scheduler(e) => e.details(),
            _ => None,
        }
    }
}

/// What the doctor found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    /// A live lease whose unit (or resource) no longer exists.
    LeaseOnMissingUnit,
    /// A queued request whose resource (or pinned unit) no longer exists.
    RequestForMissingResource,
    /// A CLI process holder that is no longer alive.
    DeadHolder,
    /// A device unit whose path does not exist (a warning; never fixed).
    MissingDevicePath,
    /// A link naming a unit that no longer exists.
    LinkToMissingUnit,
}

/// One doctor finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub kind: FindingKind,
    /// What it is about: a lease or request id, a holder, `resource/unit`,
    /// or `<unit row>-<unit row>` for a link.
    pub subject: String,
    /// True when this run repaired it.
    pub fixed: bool,
}

/// The doctor's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DoctorReport {
    pub findings: Vec<Finding>,
}

/// One end of an exported link: `[scope, resource, unit]`, the scope
/// written as `user` or `project:<path>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LinkEnd(pub ScopeSpec, pub ResourceId, pub UnitId);

/// The export document (version 1), also what `admin_import` takes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportDoc {
    pub version: u32,
    pub resources: Vec<ExportResource>,
    /// Every link, manual or found by identity matching. The format does
    /// not record which: on import, dedup re-creates the automatic ones and
    /// then every link listed here is made (or upgraded to) a manual link
    /// through `Inventory::link`, so after an import all of them are
    /// manual. Unlinks (suppressions) are not exported.
    #[serde(default)]
    pub links: Vec<[LinkEnd; 2]>,
}

/// A resource in an export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportResource {
    pub id: ResourceId,
    #[serde(rename = "type")]
    pub ty: ResourceType,
    /// `"user"` or `{"project": "<path>"}`.
    pub scope: Scope,
    #[serde(default)]
    pub description: String,
    /// Called `notes` in exports made before descriptions and usage existed.
    #[serde(default, alias = "notes")]
    pub usage: String,
    /// `null` means the type default.
    #[serde(default)]
    pub health: Option<HealthSpec>,
    #[serde(default)]
    pub units: Vec<ExportUnit>,
}

/// A unit in an export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportUnit {
    pub id: UnitId,
    pub settings: UnitSettings,
    /// Called `notes` in exports made before descriptions existed.
    #[serde(default, alias = "notes")]
    pub description: String,
}

/// What an import did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportReport {
    /// The resources created, as `<id> (<scope>)`.
    pub imported: Vec<String>,
    /// Of those, the ones that replaced an existing resource.
    pub replaced: Vec<String>,
    /// Warnings raised while adding units (dedup).
    pub warnings: Vec<Warning>,
}

/// What an eviction took away.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Evicted {
    pub requests_dropped: Vec<RequestId>,
    pub leases_ended: Vec<LeaseId>,
}

/// An inventory link refusal as the admin error that names it.
fn link_error(e: InventoryError) -> AdminError {
    match e {
        InventoryError::LinkConflict { units, leases } => {
            AdminError::LinkConflict { units, leases }
        }
        other => other.into(),
    }
}

/// `<id> (<scope>)`, how messages name a resource of any scope.
fn label(id: &ResourceId, scope: &Scope) -> String {
    format!("{id} ({})", ScopeSpec::from(scope))
}

/// Administrator operations over the services, for one command.
pub struct Admin<'a> {
    inventory: &'a Inventory,
    scheduler: &'a Scheduler,
    now_ms: i64,
}

impl<'a> Admin<'a> {
    /// Operations stamped at `now_ms`.
    #[must_use]
    pub fn new(inventory: &'a Inventory, scheduler: &'a Scheduler, now_ms: i64) -> Self {
        Self {
            inventory,
            scheduler,
            now_ms,
        }
    }

    fn audit(&self, tx: &Tx, who: &Caller, op: &str, detail: &Value) -> Result<(), AdminError> {
        Ok(audit::record(
            tx,
            self.now_ms,
            who,
            &format!("admin_{op}"),
            detail,
        )?)
    }

    fn resource(tx: &Tx, who: &Caller, r: &ResourceRef) -> Result<Resource, AdminError> {
        let row = match &r.scope {
            Some(scope) => inventory_sql::find_in_scope(tx, &scope.scope(), &r.resource)?,
            None => inventory_sql::find_visible(tx, who.project.as_ref(), &r.resource)?,
        };
        let row = row.ok_or(AdminError::NotFound)?;
        Ok(inventory_sql::load_resource(tx, row)?)
    }

    fn unit(tx: &Tx, who: &Caller, u: &UnitRef) -> Result<(Resource, Unit), AdminError> {
        let resource = Self::resource(tx, who, &u.resource_ref())?;
        let unit = resource
            .units
            .iter()
            .find(|x| x.id == u.unit)
            .cloned()
            .ok_or(AdminError::NotFound)?;
        Ok((resource, unit))
    }

    /// End lease `id` now, with no cleanup window: cause `AdminForced`, the
    /// unit marked dirty (`admin_forced`), then dispatch.
    ///
    /// # Errors
    /// [`AdminError::Scheduler`] with `NotFound`, or `BadState` for a lease
    /// that already ended.
    pub fn release(&self, tx: &Tx, who: &Caller, id: &LeaseId) -> Result<(), AdminError> {
        self.scheduler
            .end_lease(tx, id, EndCause::AdminForced, false)?;
        self.audit(tx, who, "release", &json!({ "lease": id }))
    }

    /// Drop every queued request and end every live lease of the holders
    /// `spec` names, as when a holder goes away but with cause and dirty
    /// reason `AdminForced`. Dropped requests' holders get `QueueDropped`;
    /// a holder asked to yield for one gets `YieldWithdrawn`.
    ///
    /// # Errors
    /// [`AdminError::Store`] on storage failure.
    pub fn evict(&self, tx: &Tx, who: &Caller, spec: &HolderSpec) -> Result<Evicted, AdminError> {
        let ended =
            self.scheduler
                .end_holders(tx, &|h| spec.matches(h), EndCause::AdminForced, true)?;
        self.audit(
            tx,
            who,
            "evict",
            &json!({
                "holder": spec,
                "requests_dropped": ended.requests,
                "leases_ended": ended.leases,
            }),
        )?;
        self.scheduler.dispatch(tx)?;
        Ok(Evicted {
            requests_dropped: ended.requests,
            leases_ended: ended.leases,
        })
    }

    /// Remove one queued request; its holder gets `QueueDropped`.
    ///
    /// # Errors
    /// [`AdminError::NotFound`] for an unknown request.
    pub fn dequeue(&self, tx: &Tx, who: &Caller, id: &RequestId) -> Result<(), AdminError> {
        let request = leases::find_request(tx, id)?.ok_or(AdminError::NotFound)?;
        self.scheduler
            .drop_requests(tx, &[request], NoticeKind::QueueDropped, &|resource| {
                format!(
                    "An administrator removed your request for {resource} from the queue; \
                     acquire again if you still need it."
                )
            })?;
        self.audit(tx, who, "dequeue", &json!({ "request": id }))
    }

    /// Remove every queued request for a resource; each holder gets
    /// `QueueDropped`. Returns the requests dropped.
    ///
    /// # Errors
    /// [`AdminError::NotFound`] for an unknown resource.
    pub fn clear_queue(
        &self,
        tx: &Tx,
        who: &Caller,
        r: &ResourceRef,
    ) -> Result<Vec<RequestId>, AdminError> {
        let resource = Self::resource(tx, who, r)?;
        let requests: Vec<_> = leases::queued_requests(tx)?
            .into_iter()
            .filter(|q| q.resource_row == resource.row)
            .collect();
        let dropped =
            self.scheduler
                .drop_requests(tx, &requests, NoticeKind::QueueDropped, &|id| {
                    format!(
                        "An administrator cleared the queue for {id}; acquire again if you still \
                     need it."
                    )
                })?;
        self.audit(
            tx,
            who,
            "clear_queue",
            &json!({ "resource": resource.id, "scope": resource.scope, "requests": dropped }),
        )?;
        Ok(dropped)
    }

    /// Remove a unit even if it is held or queued for. Its waiters (and,
    /// when no other unit of the resource is left, the resource's waiters)
    /// and its holders get `ResourceRemoved`; live leases end with cause
    /// `AdminForced`. Linked partners stay; only the links go.
    ///
    /// # Errors
    /// [`AdminError::NotFound`] for an unknown resource or unit.
    pub fn remove_unit(&self, tx: &Tx, who: &Caller, u: &UnitRef) -> Result<(), AdminError> {
        let (resource, unit) = Self::unit(tx, who, u)?;
        // Retiring first: nothing is handed out on it while it is emptied.
        inventory_sql::mark_retiring(tx, unit.row)?;
        let others_left = resource
            .units
            .iter()
            .any(|x| x.row != unit.row && !x.retiring);
        let requests: Vec<_> = leases::queued_requests(tx)?
            .into_iter()
            .filter(|q| {
                q.unit_row == Some(unit.row)
                    || (!others_left && q.unit_row.is_none() && q.resource_row == resource.row)
            })
            .collect();
        let text = format!(
            "An administrator removed unit {} of {}; stop using it now and ask for another \
             unit or resource.",
            unit.id, resource.id
        );
        self.scheduler
            .drop_requests(tx, &requests, NoticeKind::ResourceRemoved, &|_| {
                text.clone()
            })?;
        let ended = self.scheduler.force_off_units(tx, &[unit.row], &text)?;
        // The dispatch inside ending a lease may already have swept it.
        if inventory_sql::unit_by_row(tx, unit.row)?.is_some() {
            inventory_sql::delete_unit(tx, unit.row, self.now_ms, who)?;
        }
        self.audit(
            tx,
            who,
            "remove_unit",
            &json!({
                "resource": resource.id,
                "scope": resource.scope,
                "unit": unit.id,
                "leases_ended": ended,
            }),
        )
    }

    /// Delete a resource even if held or queued for; its holders and
    /// waiters get `ResourceRemoved`.
    ///
    /// # Errors
    /// [`AdminError::NotFound`] for an unknown resource.
    pub fn delete_resource(
        &self,
        tx: &Tx,
        who: &Caller,
        r: &ResourceRef,
    ) -> Result<(), AdminError> {
        let resource = Self::resource(tx, who, r)?;
        let ended = self.force_delete(tx, who, &resource)?;
        self.audit(
            tx,
            who,
            "delete_resource",
            &json!({ "resource": resource.id, "scope": resource.scope, "leases_ended": ended }),
        )
    }

    /// The common part of a forced delete: retire every unit, drop the
    /// resource's requests and end its leases (`ResourceRemoved` to each
    /// party), then delete the row.
    fn force_delete(
        &self,
        tx: &Tx,
        who: &Caller,
        resource: &Resource,
    ) -> Result<Vec<LeaseId>, AdminError> {
        let rows: Vec<i64> = resource.units.iter().map(|u| u.row).collect();
        for row in &rows {
            inventory_sql::mark_retiring(tx, *row)?;
        }
        let requests: Vec<_> = leases::queued_requests(tx)?
            .into_iter()
            .filter(|q| q.resource_row == resource.row)
            .collect();
        let text = format!(
            "An administrator deleted resource {}; stop using it now and ask for another \
             resource.",
            resource.id
        );
        self.scheduler
            .drop_requests(tx, &requests, NoticeKind::ResourceRemoved, &|_| {
                text.clone()
            })?;
        let ended = self.scheduler.force_off_units(tx, &rows, &text)?;
        inventory_sql::delete_resource(tx, resource.row, self.now_ms, who)?;
        Ok(ended)
    }

    /// Clear a unit's dirty record.
    ///
    /// # Errors
    /// [`AdminError::NotFound`] for an unknown resource or unit.
    pub fn clean(&self, tx: &Tx, who: &Caller, u: &UnitRef) -> Result<(), AdminError> {
        let (resource, unit) = Self::unit(tx, who, u)?;
        inventory_sql::set_dirty(tx, unit.row, None)?;
        self.audit(
            tx,
            who,
            "clean",
            &json!({
                "resource": resource.id,
                "scope": resource.scope,
                "unit": unit.id,
                "was": unit.dirty,
            }),
        )
    }

    /// Link two units as one physical thing (a manual link, or an existing
    /// link upgraded to manual), forgetting an earlier unlink of the pair.
    /// Returns the live leases on the joined identity group: more than one
    /// means two holders already had the thing before it was known to be
    /// one.
    ///
    /// # Errors
    /// [`AdminError::NotFound`] for an unknown resource or unit;
    /// [`AdminError::LinkConflict`] when both identity groups have a live
    /// lease (nothing is written).
    pub fn link(
        &self,
        tx: &Tx,
        who: &Caller,
        a: &UnitRef,
        b: &UnitRef,
    ) -> Result<Vec<LeaseId>, AdminError> {
        let (ra, ua) = Self::unit(tx, who, a)?;
        let (rb, ub) = Self::unit(tx, who, b)?;
        Inventory::link(tx, ua.row, ub.row).map_err(link_error)?;
        let group = Inventory::identity_group(tx, ua.row)?;
        let live: Vec<LeaseId> = leases::live_leases_on(tx, &group)?
            .into_iter()
            .map(|l| l.id)
            .collect();
        self.audit(
            tx,
            who,
            "link",
            &json!({
                "a": LinkEnd((&ra.scope).into(), ra.id, ua.id),
                "b": LinkEnd((&rb.scope).into(), rb.id, ub.id),
                "live_leases": live,
            }),
        )?;
        Ok(live)
    }

    /// Remove the link between two units and remember that they are
    /// different things, so identity matching never links them again.
    /// Allowed even on a leased group (the administrator asserts they are
    /// different things); returns the live leases of both resulting
    /// groups, as [`Admin::link`] does for the joined one.
    ///
    /// # Errors
    /// [`AdminError::NotFound`] for an unknown resource or unit;
    /// [`AdminError::NotDirectlyLinked`] when they are one group only
    /// through other units (nothing is written).
    pub fn unlink(
        &self,
        tx: &Tx,
        who: &Caller,
        a: &UnitRef,
        b: &UnitRef,
    ) -> Result<Vec<LeaseId>, AdminError> {
        let (ra, ua) = Self::unit(tx, who, a)?;
        let (rb, ub) = Self::unit(tx, who, b)?;
        if !inventory_sql::link_exists(tx, ua.row, ub.row)? {
            let group = Inventory::identity_group(tx, ua.row)?;
            if group.contains(&ub.row) {
                let mut via = Vec::new();
                for row in group.into_iter().filter(|r| *r != ua.row && *r != ub.row) {
                    if let Some(unit) = inventory_sql::unit_by_row(tx, row)? {
                        let resource = inventory_sql::load_resource(tx, unit.resource_row)?;
                        via.push(format!("{}/{}", resource.id, unit.id));
                    }
                }
                return Err(AdminError::NotDirectlyLinked { via });
            }
        }
        Inventory::unlink(tx, ua.row, ub.row)?;
        let mut rows = Inventory::identity_group(tx, ua.row)?;
        rows.extend(Inventory::identity_group(tx, ub.row)?);
        rows.sort_unstable();
        rows.dedup();
        let live: Vec<LeaseId> = leases::live_leases_on(tx, &rows)?
            .into_iter()
            .map(|l| l.id)
            .collect();
        self.audit(
            tx,
            who,
            "unlink",
            &json!({
                "a": LinkEnd((&ra.scope).into(), ra.id, ua.id),
                "b": LinkEnd((&rb.scope).into(), rb.id, ub.id),
                "live_leases": live,
            }),
        )?;
        Ok(live)
    }

    /// Move a resource to scope `to` without the promotion flow. Its pending
    /// promotions are deleted. Leases and requests keep working.
    ///
    /// # Errors
    /// [`AdminError::NotFound`]; [`AdminError::IdClash`] when `to` already
    /// has a resource with the id.
    pub fn move_resource(
        &self,
        tx: &Tx,
        who: &Caller,
        r: &ResourceRef,
        to: &Scope,
    ) -> Result<Resource, AdminError> {
        let resource = Self::resource(tx, who, r)?;
        if let Some(row) = inventory_sql::find_in_scope(tx, to, &resource.id)?
            && row != resource.row
        {
            return Err(AdminError::IdClash {
                id: label(&resource.id, to),
            });
        }
        inventory_sql::set_scope(tx, resource.row, to)?;
        inventory_sql::delete_promotions_of(tx, resource.row)?;
        self.audit(
            tx,
            who,
            "move",
            &json!({ "resource": resource.id, "from": resource.scope, "to": to }),
        )?;
        Ok(inventory_sql::load_resource(tx, resource.row)?)
    }

    /// Rename a resource, or with `unit` one of its units. Leases and
    /// requests name rows, not ids, so they keep working.
    ///
    /// # Errors
    /// [`AdminError::NotFound`]; [`AdminError::Invalid`] for a malformed
    /// id; [`AdminError::IdClash`] when the id is taken.
    pub fn rename(
        &self,
        tx: &Tx,
        who: &Caller,
        r: &ResourceRef,
        unit: Option<&UnitId>,
        new_id: &str,
    ) -> Result<Resource, AdminError> {
        let resource = Self::resource(tx, who, r)?;
        let invalid = |e: crate::model::IdError| AdminError::Invalid(e.to_string());
        let detail = if let Some(unit_id) = unit {
            let unit = resource
                .units
                .iter()
                .find(|u| &u.id == unit_id)
                .ok_or(AdminError::NotFound)?;
            let new = UnitId::parse(new_id).map_err(invalid)?;
            if resource
                .units
                .iter()
                .any(|u| u.id == new && u.row != unit.row)
            {
                return Err(AdminError::IdClash {
                    id: format!("{} of {}", new, resource.id),
                });
            }
            inventory_sql::rename_unit(tx, unit.row, &new)?;
            json!({ "resource": resource.id, "scope": resource.scope, "unit": unit.id, "to": new })
        } else {
            let new = ResourceId::parse(new_id).map_err(invalid)?;
            if let Some(row) = inventory_sql::find_in_scope(tx, &resource.scope, &new)?
                && row != resource.row
            {
                return Err(AdminError::IdClash {
                    id: label(&new, &resource.scope),
                });
            }
            inventory_sql::rename_resource(tx, resource.row, &new)?;
            json!({ "resource": resource.id, "scope": resource.scope, "to": new })
        };
        self.audit(tx, who, "rename", &detail)?;
        Ok(inventory_sql::load_resource(tx, resource.row)?)
    }

    /// Change a lease's end, priority or interruptibility. A new end
    /// re-arms the expiry warning; one in the past expires at the next tick.
    ///
    /// # Errors
    /// [`AdminError::Scheduler`] with `NotFound` or `BadState`.
    pub fn edit_lease(
        &self,
        tx: &Tx,
        who: &Caller,
        id: &LeaseId,
        edit: LeaseEdit,
    ) -> Result<Lease, AdminError> {
        let lease = self.scheduler.edit_lease(tx, id, edit)?;
        self.audit(
            tx,
            who,
            "edit_lease",
            &json!({
                "lease": id,
                "ends_at": edit.ends_at,
                "priority": edit.priority,
                "interruptible": edit.interruptible,
            }),
        )?;
        Ok(lease)
    }

    /// Look for damage: leases on missing units, requests for missing
    /// resources, dead CLI holders, device units whose path is gone, and
    /// links to missing units. With `fix`, repair each one that can be
    /// repaired (a missing device path is only reported) and audit each
    /// repair as `admin_doctor_fix`.
    ///
    /// Repairs run in an order where each leaves the next able to work:
    /// dangling links, then orphan leases, then orphan requests, then dead
    /// holders (whose leases end through the normal path).
    ///
    /// # Errors
    /// [`AdminError::Store`] or [`AdminError::Scheduler`] on failure.
    pub fn doctor(&self, tx: &Tx, who: &Caller, fix: bool) -> Result<DoctorReport, AdminError> {
        let mut findings = Vec::new();
        let mut found = |tx: &Tx, kind, subject: String, fixed: bool| -> Result<(), AdminError> {
            if fixed {
                self.audit(
                    tx,
                    who,
                    "doctor_fix",
                    &json!({ "kind": kind, "subject": subject }),
                )?;
            }
            findings.push(Finding {
                kind,
                subject,
                fixed,
            });
            Ok(())
        };
        for (a, b) in inventory_sql::dangling_links(tx)? {
            if fix {
                inventory_sql::delete_link(tx, a, b)?;
            }
            found(tx, FindingKind::LinkToMissingUnit, format!("{a}-{b}"), fix)?;
        }
        for lease in leases::orphan_leases(tx)? {
            if fix {
                self.scheduler.end_orphan_lease(tx, &lease)?;
            }
            found(
                tx,
                FindingKind::LeaseOnMissingUnit,
                lease.id.to_string(),
                fix,
            )?;
        }
        for request in leases::orphan_requests(tx)? {
            if fix {
                self.scheduler.drop_orphan_request(tx, &request)?;
            }
            found(
                tx,
                FindingKind::RequestForMissingResource,
                request.id.to_string(),
                fix,
            )?;
        }
        let mut seen: HashSet<HolderRef> = HashSet::new();
        let holders = leases::live_leases(tx)?
            .into_iter()
            .map(|l| l.holder)
            .chain(leases::queued_requests(tx)?.into_iter().map(|r| r.holder));
        for holder in holders {
            let HolderRef::CliProcess { pid, start_time } = holder else {
                continue;
            };
            if !seen.insert(holder.clone()) || liveness::process_alive(pid, start_time) {
                continue;
            }
            if fix {
                self.scheduler.holder_gone(tx, &holder)?;
            }
            let subject = serde_json::to_string(&holder).map_err(StoreError::from)?;
            found(tx, FindingKind::DeadHolder, subject, fix)?;
        }
        for resource in inventory_sql::list_resources(tx, ScopeFilter::All)? {
            for unit in &resource.units {
                if let UnitSettings::Device { path } = &unit.settings
                    && !health::path_exists(path)
                {
                    let subject = format!("{}/{}", resource.id, unit.id);
                    found(tx, FindingKind::MissingDevicePath, subject, false)?;
                }
            }
        }
        self.audit(
            tx,
            who,
            "doctor",
            &json!({ "fix": fix, "findings": findings.len() }),
        )?;
        Ok(DoctorReport { findings })
    }

    /// Every resource with its units, and every link, in the export format
    /// (version 1). Resources are ordered by id then scope; links by their
    /// ends.
    ///
    /// # Errors
    /// [`AdminError::Store`] on storage failure.
    pub fn export(&self, tx: &Tx, who: &Caller) -> Result<ExportDoc, AdminError> {
        let resources = inventory_sql::list_resources(tx, ScopeFilter::All)?;
        let mut ends: HashMap<i64, LinkEnd> = HashMap::new();
        for r in &resources {
            for u in &r.units {
                ends.insert(
                    u.row,
                    LinkEnd((&r.scope).into(), r.id.clone(), u.id.clone()),
                );
            }
        }
        let mut links: BTreeSet<[LinkEnd; 2]> = BTreeSet::new();
        for (a, b) in inventory_sql::all_links(tx)? {
            // A dangling link (damage) has no names; the doctor reports it.
            if let (Some(ea), Some(eb)) = (ends.get(&a), ends.get(&b)) {
                let mut pair = [ea.clone(), eb.clone()];
                pair.sort();
                links.insert(pair);
            }
        }
        let doc = ExportDoc {
            version: 1,
            resources: resources
                .into_iter()
                .map(|r| ExportResource {
                    id: r.id,
                    ty: r.ty,
                    scope: r.scope,
                    description: r.description,
                    usage: r.usage,
                    health: r.health,
                    units: r
                        .units
                        .into_iter()
                        .map(|u| ExportUnit {
                            id: u.id,
                            settings: u.settings,
                            description: u.description,
                        })
                        .collect(),
                })
                .collect(),
            links: links.into_iter().collect(),
        };
        self.audit(
            tx,
            who,
            "export",
            &json!({ "resources": doc.resources.len(), "links": doc.links.len() }),
        )?;
        Ok(doc)
    }

    /// Check an import document before anything is written.
    fn validate(doc: &ExportDoc) -> Result<(), AdminError> {
        if doc.version != 1 {
            return Err(AdminError::Invalid(format!(
                "unsupported export version {}; this clerk reads version 1",
                doc.version
            )));
        }
        let mut names: HashSet<(ScopeSpec, &ResourceId)> = HashSet::new();
        let mut units: HashSet<(ScopeSpec, &ResourceId, &UnitId)> = HashSet::new();
        for r in &doc.resources {
            let scope = ScopeSpec::from(&r.scope);
            if !names.insert((scope.clone(), &r.id)) {
                return Err(AdminError::Invalid(format!(
                    "resource {} appears twice in the document",
                    label(&r.id, &r.scope)
                )));
            }
            if let Some(h) = &r.health {
                h.validate(r.ty).map_err(|e| {
                    AdminError::Invalid(format!("health of {}: {e}", label(&r.id, &r.scope)))
                })?;
            }
            for u in &r.units {
                u.settings.validate(r.ty).map_err(|e| {
                    AdminError::Invalid(format!(
                        "settings of unit {} of {}: {e}",
                        u.id,
                        label(&r.id, &r.scope)
                    ))
                })?;
                if !units.insert((scope.clone(), &r.id, &u.id)) {
                    return Err(AdminError::Invalid(format!(
                        "unit {} of {} appears twice in the document",
                        u.id,
                        label(&r.id, &r.scope)
                    )));
                }
            }
        }
        for [a, b] in &doc.links {
            for end in [a, b] {
                if !units.contains(&(end.0.clone(), &end.1, &end.2)) {
                    return Err(AdminError::Invalid(format!(
                        "link names unit {} of {} ({}), which the document does not contain",
                        end.2, end.1, end.0
                    )));
                }
            }
        }
        Ok(())
    }

    /// Import a document made by [`Admin::export`]. The document is checked
    /// first; ids that already exist are refused unless `replace`, which
    /// force-deletes the existing resource first (its holders and waiters
    /// get `ResourceRemoved`). Resources and units go through the normal
    /// inventory path, so identity is computed and dedup and links run;
    /// then every document link is made manual through `Inventory::link`
    /// (upgrading one dedup already made). Every link creation passes the
    /// inventory's join guard. All in `tx`: any failure changes nothing.
    ///
    /// # Errors
    /// [`AdminError::Invalid`] for a malformed document;
    /// [`AdminError::ImportClash`] listing existing resources when not
    /// `replace`; [`AdminError::LinkConflict`] naming the units when a
    /// document link would join two leased identity groups; [`AdminError::Inventory`] when an inventory rule refuses
    /// (e.g. a unit duplicating one in the same scope).
    pub fn import(
        &self,
        tx: &Tx,
        who: &Caller,
        doc: &ExportDoc,
        replace: bool,
    ) -> Result<ImportReport, AdminError> {
        Self::validate(doc)?;
        let mut clashes: Vec<(Resource, String)> = Vec::new();
        for r in &doc.resources {
            if let Some(row) = inventory_sql::find_in_scope(tx, &r.scope, &r.id)? {
                clashes.push((
                    inventory_sql::load_resource(tx, row)?,
                    label(&r.id, &r.scope),
                ));
            }
        }
        if !clashes.is_empty() && !replace {
            return Err(AdminError::ImportClash {
                ids: clashes.into_iter().map(|(_, l)| l).collect(),
            });
        }
        let mut replaced = Vec::new();
        for (resource, name) in clashes {
            self.force_delete(tx, who, &resource)?;
            replaced.push(name);
        }
        let mut imported = Vec::new();
        let mut warnings = Vec::new();
        for r in &doc.resources {
            // The inventory only creates in the caller's own project, so
            // the import acts from inside the target project.
            let actor = Caller {
                holder: who.holder.clone(),
                project: match &r.scope {
                    Scope::Project(key) => Some(key.clone()),
                    Scope::User => None,
                },
                override_reason: who.override_reason.clone(),
            };
            self.inventory.create_resource(
                tx,
                &actor,
                &NewResource {
                    id: r.id.clone(),
                    ty: r.ty,
                    scope: r.scope.clone(),
                    description: r.description.clone(),
                    usage: r.usage.clone(),
                    health: r.health.clone(),
                },
            )?;
            for u in &r.units {
                let added = self.inventory.add_unit(
                    tx,
                    &actor,
                    &r.id,
                    &NewUnit {
                        id: u.id.clone(),
                        settings: u.settings.clone(),
                        description: u.description.clone(),
                    },
                )?;
                for w in added.warnings {
                    if !warnings.contains(&w) {
                        warnings.push(w);
                    }
                }
            }
            imported.push(label(&r.id, &r.scope));
        }
        for [a, b] in &doc.links {
            let (ra, rb) = (Self::end_row(tx, a)?, Self::end_row(tx, b)?);
            Inventory::link(tx, ra, rb).map_err(link_error)?;
        }
        self.audit(
            tx,
            who,
            "import",
            &json!({ "imported": imported, "replaced": replaced, "links": doc.links.len() }),
        )?;
        Ok(ImportReport {
            imported,
            replaced,
            warnings,
        })
    }

    /// The unit row a link end names.
    fn end_row(tx: &Tx, end: &LinkEnd) -> Result<i64, AdminError> {
        let row = inventory_sql::find_in_scope(tx, &end.0.scope(), &end.1)?
            .ok_or(AdminError::NotFound)?;
        Ok(inventory_sql::find_unit(tx, row, &end.2)?
            .ok_or(AdminError::NotFound)?
            .row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holder_spec_parses_the_three_forms() {
        assert_eq!(
            HolderSpec::parse("session:abc"),
            Ok(HolderSpec::Session("abc".into()))
        );
        assert_eq!(HolderSpec::parse("pid:42"), Ok(HolderSpec::Pid(42)));
        assert_eq!(
            HolderSpec::parse("user:me"),
            Ok(HolderSpec::User("me".into()))
        );
    }

    #[test]
    fn holder_spec_rejects_other_text() {
        for s in ["", "session:", "pid:x", "pid:-1", "host:a", "nocolon"] {
            assert!(HolderSpec::parse(s).is_err(), "{s}");
        }
    }

    #[test]
    fn pid_spec_matches_any_start_time() {
        let spec = HolderSpec::Pid(7);
        assert!(spec.matches(&HolderRef::CliProcess {
            pid: 7,
            start_time: 1
        }));
        assert!(spec.matches(&HolderRef::CliProcess {
            pid: 7,
            start_time: 99
        }));
        assert!(!spec.matches(&HolderRef::CliProcess {
            pid: 8,
            start_time: 1
        }));
        assert!(!spec.matches(&HolderRef::User { tag: "7".into() }));
    }

    #[test]
    fn scope_spec_round_trips() {
        for s in ["user", "project:/a/b"] {
            let spec = ScopeSpec::parse(s).unwrap();
            assert_eq!(ScopeSpec::from(&spec.scope()), spec);
        }
        assert!(ScopeSpec::parse("project:").is_err());
        assert!(ScopeSpec::parse("users").is_err());
    }
}
