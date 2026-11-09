//! The scheduler: grants units, queues requests and offers freed units.
//!
//! Scheduling works per identity group (linked units are one physical
//! thing). A group is busy while any member has a lease that is offered,
//! held, paused or cleaning; every check of that and the lease insert it
//! guards happen inside the caller's transaction, so one physical thing is
//! never leased twice.
//!
//! The one exception is borrowing (see `interrupt`): a paused lease keeps its
//! group busy, and exactly one more lease, its borrower's, may be written on
//! that group, and only while the paused lease is the group's sole live
//! lease.
//!
//! Health checks run synchronously on the caller's thread. The http check
//! builds its own runtime, so the scheduler must not be called from inside a
//! tokio runtime. An unhealthy result is kept for [`HEALTH_RECHECK_BACKOFF`]
//! so that a waiter on a failing unit does not stall every dispatch.

mod dispatch;
mod forced;
mod interrupt;
mod lifecycle;
pub mod liveness;

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;

use crate::clock::Clock;
use crate::config::Config;
use crate::error::ErrorCode;
use crate::health::{HealthChecker, HealthResult};
use crate::inventory::{Inventory, InventoryError, View};
use crate::model::{
    Caller, DirtyInfo, EndCause, HealthSpec, HolderRef, LeaseId, LeaseState, Priority, ProjectKey,
    RenderResource, RenderUnit, RequestId, Resource, ResourceId, ResourceType, Scope, ServiceMode,
    TagSet, Unit, UnitId, UnitSettings, UsageTemplate,
};
use crate::store::inventory::{self as inventory_sql, ScopeFilter};
use crate::store::leases::{self, LeaseRow, NewLease, NewRequest};
use crate::store::notices::{self, Notice};
use crate::store::{StoreError, Tx, audit};

use dispatch::{HealthCache, QueueSnapshot};

pub use forced::LeaseEdit;
pub use lifecycle::Confirm;

/// Runs a unit's health checks. Implemented by [`HealthChecker`]; tests use
/// stubs.
pub trait HealthProbe: Send + Sync {
    /// Run `spec` against the unit.
    fn check(
        &self,
        spec: &HealthSpec,
        resource: &ResourceId,
        unit: &UnitId,
        settings: &UnitSettings,
    ) -> HealthResult;
}

impl HealthProbe for HealthChecker {
    fn check(
        &self,
        spec: &HealthSpec,
        resource: &ResourceId,
        unit: &UnitId,
        settings: &UnitSettings,
    ) -> HealthResult {
        self.check_unit(spec, resource, unit, settings)
    }
}

/// What a request asks for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    /// Any eligible unit of the resource.
    Resource(ResourceId),
    /// One specific unit.
    Unit(ResourceId, UnitId),
}

/// A lease request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcquireRequest {
    pub target: Target,
    /// Expected duration of use.
    pub duration: Duration,
    pub interruptible: bool,
    pub priority: Priority,
    /// Short reason, shown to others who may see it.
    pub purpose: String,
    /// Set when the user said to take the unit regardless; stored and
    /// audited (the override behaviour itself belongs to interruption).
    pub override_reason: Option<String>,
}

/// What a holder is told about what it leases: the resource's description,
/// the unit's description, the resource's usage template rendered for the
/// unit and the resource's tags. Shared by grants and lease views so every
/// path carries one brief.
struct Brief {
    resource_description: String,
    unit_description: String,
    usage: String,
    resource_tags: TagSet,
}

fn brief(resource: &Resource, unit: &Unit) -> Brief {
    // The stored template passed `parse` when it was written.
    let usage = UsageTemplate::from_stored(resource.usage.clone()).render(
        &RenderResource {
            id: &resource.id,
            description: &resource.description,
        },
        &RenderUnit {
            id: &unit.id,
            description: &unit.description,
            settings: &unit.settings,
        },
    );
    Brief {
        resource_description: resource.description.clone(),
        unit_description: unit.description.clone(),
        usage,
        resource_tags: resource.tags.clone(),
    }
}

/// An immediate grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Grant {
    pub lease_id: LeaseId,
    pub resource: ResourceId,
    pub unit: UnitId,
    pub settings: UnitSettings,
    /// The resource's description.
    pub resource_description: String,
    /// The unit's description.
    pub unit_description: String,
    /// The resource's usage template, rendered for this unit.
    pub usage: String,
    /// The resource's tags.
    pub resource_tags: TagSet,
    /// Unix milliseconds.
    pub ends_at: i64,
    /// Set when the unit was handed over without confirmed cleanup.
    pub dirty: Option<DirtyInfo>,
}

/// The answer to an acquire.
// One short-lived value per call; boxing the grant would only add noise for callers.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum AcquireOutcome {
    /// A unit is the caller's now.
    Granted(Grant),
    /// The request waits in the queue.
    ///
    /// Queues are per physical thing, not per resource: a request competes
    /// with every queued request whose target units share an identity group
    /// with its own, whichever resource those requests name.
    Queued {
        request_id: RequestId,
        /// 1 + the number of earlier queued requests (by priority, then
        /// clock time) whose target identity groups intersect this one's.
        position: u32,
        /// `None` when unknown: a target unit is free but failing its health
        /// check, nothing on the targets is leased, or a lease's remaining
        /// time is unknown (paused).
        estimated_wait: Option<Duration>,
    },
    /// An override by a paused holder against its own borrower: the
    /// borrower is reclaimed and the unit returns to the paused lease.
    Reclaiming {
        /// The caller's paused lease, which gets the unit back.
        lease_id: LeaseId,
    },
}

/// A lease as seen by its holder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Lease {
    pub id: LeaseId,
    pub resource: ResourceId,
    pub unit: UnitId,
    pub settings: UnitSettings,
    /// The resource's description.
    pub resource_description: String,
    /// The unit's description.
    pub unit_description: String,
    /// The resource's usage template, rendered for this unit.
    pub usage: String,
    /// The resource's tags.
    pub resource_tags: TagSet,
    pub state: LeaseState,
    pub holder: HolderRef,
    pub priority: Priority,
    pub interruptible: bool,
    pub purpose: String,
    pub duration: Duration,
    pub created_at: i64,
    pub ends_at: Option<i64>,
    pub claim_deadline: Option<i64>,
    /// When the cleanup window ends, while cleaning.
    pub cleanup_deadline: Option<i64>,
    pub override_reason: Option<String>,
    pub dirty: Option<DirtyInfo>,
    /// When the holder must answer a pending yield request.
    pub yield_deadline: Option<i64>,
    /// While paused: the time left on the lease when it was paused.
    pub paused_remaining: Option<Duration>,
    /// Once ended: why.
    pub end_cause: Option<EndCause>,
    /// The queued request the lease was offered for; `None` for a direct
    /// grant.
    pub request_id: Option<RequestId>,
}

/// A queued request as seen by its holder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MyRequest {
    pub request_id: RequestId,
    pub resource: ResourceId,
    /// Set when the request targets one unit.
    pub unit: Option<UnitId>,
    /// 1 + the competing requests ahead (see [`AcquireOutcome::Queued`]).
    pub position: u32,
    pub priority: Priority,
    pub interruptible: bool,
    pub purpose: String,
    pub created_at: i64,
}

/// Everything a holder has: live leases and queued requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MyLeases {
    pub leases: Vec<Lease>,
    pub requests: Vec<MyRequest>,
}

/// Why a scheduler operation failed.
#[derive(Debug, Error)]
pub enum SchedulerError {
    /// No such resource, unit, lease or request is visible to the caller.
    #[error("no such resource, unit, lease or request")]
    NotFound,
    /// The caller already holds that unit.
    #[error("you already hold that unit")]
    AlreadyHeld,
    /// An extension was refused because someone more important waits.
    #[error("extension refused: a higher-priority request is waiting")]
    ExtendRefused {
        /// The waiter's purpose, when the caller may see it.
        waiting_purpose: Option<String>,
    },
    /// An extension must move the end later than it is now.
    #[error("an extension must end later than the lease does now ({ends_at})")]
    ExtendNotLater {
        /// The lease's current end, unix milliseconds.
        ends_at: i64,
    },
    /// The one cleanup extension was already used.
    #[error("the cleanup extension was already used")]
    ExtensionUsed,
    /// The caller is not the holder.
    #[error("you are not the holder")]
    NotHolder,
    /// The targeted unit (or every unit of the resource) is being retired
    /// and will never be granted again.
    #[error("that unit is being retired; ask for another unit or resource")]
    Retiring,
    /// A reclaim before the borrower ran past its declared duration.
    #[error("the borrower has not run past its declared time; reclaim only after it does")]
    BorrowerNotOverrun {
        /// When the borrower's declared duration ends (unix ms); `None`
        /// while it has not claimed the unit yet.
        due_at: Option<i64>,
    },
    /// The resource has no units, so nothing could ever be granted.
    #[error("that resource has no units; add one or ask for another resource")]
    NoUnits,
    /// The lease is not in a state that allows this.
    #[error("the lease is {state:?}, which does not allow that")]
    BadState { state: LeaseState },
    /// Storage failed.
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl ErrorCode for SchedulerError {
    fn code(&self) -> &'static str {
        match self {
            SchedulerError::NotFound => "not_found",
            SchedulerError::AlreadyHeld => "already_held",
            SchedulerError::ExtendRefused { .. } => "extend_refused",
            SchedulerError::ExtendNotLater { .. } => "extend_not_later",
            SchedulerError::ExtensionUsed => "extension_used",
            SchedulerError::NotHolder => "not_holder",
            SchedulerError::Retiring => "retiring",
            SchedulerError::NoUnits => "no_units",
            SchedulerError::BorrowerNotOverrun { .. } => "borrower_not_overrun",
            SchedulerError::BadState { .. } => "bad_state",
            SchedulerError::Store(e) => e.code(),
        }
    }

    fn details(&self) -> Option<serde_json::Value> {
        match self {
            SchedulerError::ExtendRefused { waiting_purpose } => {
                Some(json!({ "waiting_purpose": waiting_purpose }))
            }
            SchedulerError::BorrowerNotOverrun { due_at } => Some(json!({ "due_at": due_at })),
            _ => None,
        }
    }
}

/// Everything `status` reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusView {
    pub resources: Vec<ResourceStatus>,
}

/// One resource in a status report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResourceStatus {
    pub id: ResourceId,
    #[serde(rename = "type")]
    pub ty: ResourceType,
    pub scope: Scope,
    /// The resource's description.
    pub description: String,
    /// The resource's tags.
    pub tags: TagSet,
    /// True when this project resource hides a user resource of the same id.
    pub shadows_user: bool,
    pub units: Vec<UnitStatus>,
    /// Requests queued on this resource, in queue order.
    pub queue: Vec<QueueEntry>,
}

/// One unit in a status report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnitStatus {
    pub id: UnitId,
    /// The unit's description.
    pub description: String,
    pub retiring: bool,
    pub dirty: Option<DirtyInfo>,
    pub last_health: Option<String>,
    pub last_health_at: Option<i64>,
    pub occupancy: Occupancy,
}

/// Who, if anyone, has a unit's identity group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Occupancy {
    /// Nobody.
    Free,
    /// A holder the viewer may see.
    Held(HolderView),
    /// A holder whose resource the viewer may not see; nothing more is told.
    HeldInAnotherProject,
}

impl fmt::Display for Occupancy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Occupancy::Free => f.write_str("free"),
            Occupancy::Held(h) => write!(f, "held for \"{}\"", h.purpose),
            Occupancy::HeldInAnotherProject => f.write_str("held by an agent in another project"),
        }
    }
}

/// A visible holder of a unit's group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HolderView {
    pub holder: HolderRef,
    pub lease_id: LeaseId,
    pub state: LeaseState,
    pub purpose: String,
    /// The resource and unit the lease is on (a linked unit may differ).
    pub resource: ResourceId,
    pub unit: UnitId,
    pub ends_at: Option<i64>,
}

/// One queued request in a status report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QueueEntry {
    pub request_id: RequestId,
    pub holder: HolderRef,
    pub priority: Priority,
    pub purpose: String,
    pub position: u32,
    /// Set when the request targets one unit.
    pub unit: Option<UnitId>,
    pub created_at: i64,
}

/// True when a caller in `project` may see a resource in `scope`: user-scope
/// resources are visible to everyone, project resources only to that project.
#[must_use]
pub fn can_see(project: Option<&ProjectKey>, scope: &Scope) -> bool {
    match scope {
        Scope::User => true,
        Scope::Project(key) => project == Some(key),
    }
}

/// The party a notice mentions besides its recipient.
#[derive(Debug, Clone, Copy)]
pub struct OtherParty<'a> {
    /// Scope of the resource the other party holds or asks for.
    pub scope: &'a Scope,
    /// The other party's stated purpose.
    pub purpose: &'a str,
}

/// Publish `notice`, filling `other_purpose` only when the recipient (in
/// `recipient_project`) may see the other party's resource. Every scheduler
/// notice goes through here.
///
/// # Errors
/// [`StoreError`] when the notice cannot be stored.
pub fn notify(
    tx: &Tx,
    mut notice: Notice,
    recipient_project: Option<&ProjectKey>,
    other: Option<OtherParty<'_>>,
) -> Result<(), StoreError> {
    notice.other_purpose = other
        .filter(|o| can_see(recipient_project, o.scope))
        .map(|o| o.purpose.to_owned());
    notices::publish(tx, &notice)
}

/// Milliseconds in `d`, saturating.
fn millis(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

/// A non-negative millisecond count as a duration.
fn from_millis(ms: i64) -> Duration {
    Duration::from_millis(u64::try_from(ms).unwrap_or(0))
}

/// The identity group of `unit_row`.
fn group(tx: &Tx, unit_row: i64) -> Result<Vec<i64>, SchedulerError> {
    Inventory::identity_group(tx, unit_row).map_err(|e| match e {
        InventoryError::Store(s) => SchedulerError::Store(s),
        // identity_group only ever fails in storage.
        _ => SchedulerError::Store(StoreError::Corrupt("unit_links")),
    })
}

/// True when any member of `group` has a live lease.
fn group_busy(tx: &Tx, group: &[i64]) -> Result<bool, SchedulerError> {
    Ok(!leases::live_leases_on(tx, group)?.is_empty())
}

/// The actor recorded for actions the scheduler takes on its own.
fn clerk_actor() -> Caller {
    Caller {
        holder: HolderRef::User {
            tag: "clerk".to_owned(),
        },
        project: None,
        override_reason: None,
    }
}

/// Who asks, for how long and why: the common part of an acquire and a
/// queued request.
struct Ask<'a> {
    holder: &'a HolderRef,
    project: Option<&'a ProjectKey>,
    priority: Priority,
    interruptible: bool,
    purpose: &'a str,
    duration_ms: i64,
    override_reason: Option<&'a str>,
    /// The queued request being served; `None` for a direct grant.
    request_id: Option<&'a RequestId>,
}

/// A lease just written by [`Scheduler::hand_over`].
struct HandedOver {
    row: i64,
    id: LeaseId,
    ends_at: Option<i64>,
    claim_deadline: Option<i64>,
    dirty: Option<DirtyInfo>,
}

/// How long an unhealthy health-check result stands before the unit is
/// probed again. Healthy results are never kept: a unit is always checked
/// again before it is handed over.
pub const HEALTH_RECHECK_BACKOFF: Duration = Duration::from_secs(30);

/// An unhealthy result kept for [`HEALTH_RECHECK_BACKOFF`], with what it
/// was checked against: a unit whose settings or health spec differ (an
/// update, or a reused row id) is probed afresh.
struct KeptUnhealthy {
    at: i64,
    settings: UnitSettings,
    spec: HealthSpec,
}

/// The scheduler service.
pub struct Scheduler {
    clock: Arc<dyn Clock>,
    config: Config,
    health: Arc<dyn HealthProbe>,
    /// Unhealthy results by unit row, in memory only (a restart forgets
    /// them, which only costs a probe).
    unhealthy: Mutex<HashMap<i64, KeptUnhealthy>>,
}

impl Scheduler {
    /// A scheduler reading time from `clock` and checking units with
    /// `health`.
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>, config: Config, health: Arc<dyn HealthProbe>) -> Self {
        Self {
            clock,
            config,
            health,
            unhealthy: Mutex::new(HashMap::new()),
        }
    }

    fn now(&self) -> i64 {
        self.clock.now_ms()
    }

    /// Forget the kept health results of `units`, so they are probed again
    /// at the next chance (after an update of the unit or its resource).
    pub fn forget_health(&self, units: &[i64]) {
        let mut kept = self.kept_unhealthy();
        for unit in units {
            kept.remove(unit);
        }
    }

    fn kept_unhealthy(&self) -> MutexGuard<'_, HashMap<i64, KeptUnhealthy>> {
        // Every operation leaves the map consistent, so a panic while it
        // was locked leaves nothing to repair.
        self.unhealthy
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Run the resource's health checks on `unit` once per `cache`, recording
    /// the result on the unit. An unhealthy result stands, without a probe,
    /// for [`HEALTH_RECHECK_BACKOFF`] while the unit's settings and health
    /// spec are unchanged, so a waiter on a failing unit does not make every
    /// dispatch wait on a slow check.
    fn healthy(
        &self,
        tx: &Tx,
        cache: &mut HealthCache,
        resource: &Resource,
        unit: &Unit,
    ) -> Result<bool, SchedulerError> {
        if let Some(ok) = cache.get(&unit.row) {
            return Ok(*ok);
        }
        let mode: Option<ServiceMode> = match &unit.settings {
            UnitSettings::Service { mode, .. } => Some(*mode),
            _ => None,
        };
        let spec = resource
            .health
            .clone()
            .unwrap_or_else(|| HealthSpec::default_for(resource.ty, mode));
        let now = self.now();
        let backoff = millis(HEALTH_RECHECK_BACKOFF);
        if self.kept_unhealthy().get(&unit.row).is_some_and(|k| {
            now < k.at.saturating_add(backoff) && k.settings == unit.settings && k.spec == spec
        }) {
            cache.insert(unit.row, false);
            return Ok(false);
        }
        let result = self
            .health
            .check(&spec, &resource.id, &unit.id, &unit.settings);
        let (ok, text) = match result {
            HealthResult::Healthy => (true, "healthy".to_owned()),
            HealthResult::Unhealthy(why) => (false, format!("unhealthy: {why}")),
        };
        inventory_sql::record_health(tx, unit.row, &text, now)?;
        if ok {
            self.kept_unhealthy().remove(&unit.row);
        } else {
            self.kept_unhealthy().insert(
                unit.row,
                KeptUnhealthy {
                    at: now,
                    settings: unit.settings.clone(),
                    spec,
                },
            );
        }
        cache.insert(unit.row, ok);
        Ok(ok)
    }

    /// Write a lease on `unit` for `ask`, held (synchronous grant) or offered
    /// (asynchronous), and move the unit's dirty record onto it. The caller
    /// has checked, in this transaction, that the unit's group is free.
    fn hand_over(
        &self,
        tx: &Tx,
        resource: &Resource,
        unit: &Unit,
        ask: &Ask<'_>,
        state: LeaseState,
    ) -> Result<HandedOver, SchedulerError> {
        let now = self.now();
        let id = LeaseId::new();
        let (ends_at, claim_deadline) = if state == LeaseState::Held {
            (Some(now.saturating_add(ask.duration_ms)), None)
        } else {
            let window = millis(Duration::from_secs(self.config.claim_window));
            (None, Some(now.saturating_add(window)))
        };
        let row = leases::insert_lease(
            tx,
            &NewLease {
                id: &id,
                unit_row: unit.row,
                resource_row: resource.row,
                state,
                holder: ask.holder,
                project: ask.project,
                priority: ask.priority,
                interruptible: ask.interruptible,
                purpose: ask.purpose,
                duration_ms: ask.duration_ms,
                created_at: now,
                ends_at,
                claim_deadline,
                override_reason: ask.override_reason,
                dirty: unit.dirty.as_ref(),
                request_id: ask.request_id,
            },
        )?;
        if unit.dirty.is_some() {
            inventory_sql::set_dirty(tx, unit.row, None)?;
        }
        Ok(HandedOver {
            row,
            id,
            ends_at,
            claim_deadline,
            dirty: unit.dirty.clone(),
        })
    }

    /// Ask for a unit. A free, healthy unit of the target is granted at once
    /// (state held); otherwise the request is queued. A queued request asks
    /// an interruptible holder to yield when the interrupt rule allows; one
    /// carrying `override_reason` instead takes the unit from the held lease
    /// that started earliest (see `interrupt`).
    ///
    /// The queue is settled first (a `dispatch`), so a newcomer never takes a
    /// unit that an earlier waiter could have been offered.
    ///
    /// # Errors
    /// [`SchedulerError::NotFound`] for a target the caller cannot see;
    /// [`SchedulerError::AlreadyHeld`] when the caller already holds the
    /// targeted unit (or a unit linked to it);
    /// [`SchedulerError::NoUnits`] when the resource has no units at all.
    // By value: the agreed interface hands the request over to the scheduler.
    #[allow(clippy::needless_pass_by_value)]
    pub fn acquire(
        &self,
        tx: &Tx,
        who: &Caller,
        req: AcquireRequest,
    ) -> Result<AcquireOutcome, SchedulerError> {
        let (resource_id, unit_id) = match &req.target {
            Target::Resource(r) => (r, None),
            Target::Unit(r, u) => (r, Some(u)),
        };
        let row = inventory_sql::find_visible(tx, who.project.as_ref(), resource_id)?
            .ok_or(SchedulerError::NotFound)?;
        let mut cache = HealthCache::new();
        self.dispatch_with(tx, &mut cache, self.now())?;
        // Loaded after dispatch, which may remove retiring units or move
        // dirty records.
        let resource = inventory_sql::load_resource(tx, row)?;
        if resource.units.is_empty() {
            return Err(SchedulerError::NoUnits);
        }
        let pinned = match unit_id {
            Some(u) => Some(
                resource
                    .units
                    .iter()
                    .find(|x| &x.id == u)
                    .ok_or(SchedulerError::NotFound)?,
            ),
            None => None,
        };
        if let Some(unit) = pinned {
            let members = group(tx, unit.row)?;
            if leases::live_leases_on(tx, &members)?
                .iter()
                .any(|l| l.holder == who.holder)
            {
                return Err(SchedulerError::AlreadyHeld);
            }
        }
        // A retiring unit is never granted again: refuse now rather than
        // queue a request that the retiring sweep would later drop.
        let all_retiring = !resource.units.is_empty() && resource.units.iter().all(|u| u.retiring);
        if pinned.is_some_and(|u| u.retiring) || (pinned.is_none() && all_retiring) {
            return Err(SchedulerError::Retiring);
        }
        let ask = Ask {
            holder: &who.holder,
            project: who.project.as_ref(),
            priority: req.priority,
            interruptible: req.interruptible,
            purpose: &req.purpose,
            duration_ms: millis(req.duration),
            override_reason: req.override_reason.as_deref(),
            request_id: None,
        };
        let candidates: Vec<&Unit> = match pinned {
            Some(u) => vec![u],
            None => resource.units.iter().collect(),
        };
        for unit in candidates {
            if unit.retiring || group_busy(tx, &group(tx, unit.row)?)? {
                continue;
            }
            if !self.healthy(tx, &mut cache, &resource, unit)? {
                continue;
            }
            let handed = self.hand_over(tx, &resource, unit, &ask, LeaseState::Held)?;
            audit::record(
                tx,
                self.now(),
                who,
                "granted",
                &json!({
                    "lease": handed.id,
                    "resource": resource.id,
                    "unit": unit.id,
                    "purpose": req.purpose,
                    "override_reason": req.override_reason,
                    "dirty": handed.dirty,
                }),
            )?;
            let brief = brief(&resource, unit);
            return Ok(AcquireOutcome::Granted(Grant {
                lease_id: handed.id,
                resource: resource.id.clone(),
                unit: unit.id.clone(),
                settings: unit.settings.clone(),
                resource_description: brief.resource_description,
                unit_description: brief.unit_description,
                usage: brief.usage,
                resource_tags: brief.resource_tags,
                // Invariant: a held hand-over always sets ends_at.
                ends_at: handed.ends_at.unwrap_or_default(),
                dirty: handed.dirty,
            }));
        }
        self.queue_request(tx, who, &req, &resource, pinned, &ask)
    }

    /// Queue an acquire that found no free unit, then either override the
    /// held lease it displaces (with `override_reason`) or ask a holder to
    /// yield.
    fn queue_request(
        &self,
        tx: &Tx,
        who: &Caller,
        req: &AcquireRequest,
        resource: &Resource,
        pinned: Option<&Unit>,
        ask: &Ask<'_>,
    ) -> Result<AcquireOutcome, SchedulerError> {
        let eligible: Vec<&Unit> = match pinned {
            Some(u) => vec![u],
            None => resource.units.iter().collect(),
        }
        .into_iter()
        .filter(|u| !u.retiring)
        .collect();
        let displaced = match req.override_reason {
            Some(_) => Self::override_target(tx, who, &eligible)?,
            None => None,
        };
        if let (Some(target), Some(reason)) = (&displaced, &req.override_reason)
            && let Some(paused) = Self::own_borrowers_paused(tx, who, target)?
        {
            // Overriding one's own borrower is a reclaim, without waiting
            // for an overrun: the user asked for the unit back.
            self.start_reclaim(tx, who, &paused, target, Some(reason))?;
            return Ok(AcquireOutcome::Reclaiming {
                lease_id: paused.id,
            });
        }
        let outcome = self.enqueue(
            tx,
            who,
            req,
            resource,
            pinned,
            ask,
            displaced.as_ref().map(|l| l.row),
        )?;
        if let AcquireOutcome::Queued { request_id, .. } = &outcome {
            match (&displaced, &req.override_reason) {
                (Some(lease), Some(reason)) => {
                    let other = OtherParty {
                        scope: &resource.scope,
                        purpose: &req.purpose,
                    };
                    self.apply_override(tx, who, lease, request_id, reason, other)?;
                }
                _ => self.maybe_request_yield(tx, request_id)?,
            }
        }
        Ok(outcome)
    }

    // One argument over the lint's limit; bundling them would only move the
    // list into a struct used once.
    #[allow(clippy::too_many_arguments)]
    fn enqueue(
        &self,
        tx: &Tx,
        who: &Caller,
        req: &AcquireRequest,
        resource: &Resource,
        pinned: Option<&Unit>,
        ask: &Ask<'_>,
        override_lease: Option<i64>,
    ) -> Result<AcquireOutcome, SchedulerError> {
        let before = QueueSnapshot::load(tx)?;
        let request_id = RequestId::new();
        let row = leases::insert_request(
            tx,
            &NewRequest {
                id: &request_id,
                resource_row: resource.row,
                unit_row: pinned.map(|u| u.row),
                holder: ask.holder,
                project: ask.project,
                priority: ask.priority,
                interruptible: ask.interruptible,
                purpose: ask.purpose,
                duration_ms: ask.duration_ms,
                created_at: self.now(),
                override_reason: ask.override_reason,
                override_lease,
            },
        )?;
        let after = QueueSnapshot::load(tx)?;
        self.publish_moves(tx, &before, &after)?;
        let position = after.position(row).ok_or(StoreError::Corrupt("requests"))?;
        let eligible: Vec<&Unit> = match pinned {
            Some(u) => vec![u],
            None => resource.units.iter().collect(),
        }
        .into_iter()
        .filter(|u| !u.retiring)
        .collect();
        let estimated_wait = self.estimate(tx, &after, row, &eligible)?;
        audit::record(
            tx,
            self.now(),
            who,
            "queued",
            &json!({
                "request": request_id,
                "resource": resource.id,
                "unit": pinned.map(|u| &u.id),
                "purpose": req.purpose,
                "position": position,
                "override_reason": req.override_reason,
            }),
        )?;
        Ok(AcquireOutcome::Queued {
            request_id,
            position,
            estimated_wait,
        })
    }

    /// The remaining time of live leases on the identity groups of the
    /// request's eligible (non-retiring) units, plus the declared durations
    /// of competing requests ahead, divided by the eligible unit count.
    ///
    /// `None` when no unit is eligible, when an eligible unit's group is free
    /// (the request queued only because of health, so no lease end will free
    /// it), when nothing is leased, or when a lease's remaining time is
    /// unknown (paused).
    fn estimate(
        &self,
        tx: &Tx,
        queue: &QueueSnapshot,
        row: i64,
        eligible: &[&Unit],
    ) -> Result<Option<Duration>, SchedulerError> {
        let Ok(units) = i64::try_from(eligible.len()) else {
            return Ok(None);
        };
        if units == 0 {
            return Ok(None);
        }
        let mut members: Vec<i64> = Vec::new();
        for unit in eligible {
            let unit_group = group(tx, unit.row)?;
            if !group_busy(tx, &unit_group)? {
                return Ok(None);
            }
            members.extend(unit_group);
        }
        members.sort_unstable();
        members.dedup();
        let live = leases::live_leases_on(tx, &members)?;
        if live.is_empty() {
            return Ok(None);
        }
        let now = self.now();
        let mut total: i64 = 0;
        for lease in live {
            let remaining = match lease.state {
                LeaseState::Held => lease.ends_at.map(|e| e - now),
                LeaseState::Offered => Some(lease.duration_ms),
                LeaseState::Cleaning => lease.cleanup_deadline.map(|d| d - now),
                LeaseState::Paused | LeaseState::Ended => None,
            };
            let Some(remaining) = remaining else {
                return Ok(None);
            };
            total = total.saturating_add(remaining.max(0));
        }
        for ahead in queue.ahead_of(row) {
            total = total.saturating_add(ahead.duration_ms);
        }
        Ok(Some(from_millis(total / units)))
    }

    /// Claim an offered lease; its clock starts now.
    ///
    /// # Errors
    /// [`SchedulerError::NotFound`], [`SchedulerError::NotHolder`], or
    /// [`SchedulerError::BadState`] when the lease is not an open offer (an
    /// offer past its claim deadline reports `Ended`: it is withdrawn at the
    /// next dispatch).
    pub fn claim(&self, tx: &Tx, who: &Caller, id: &LeaseId) -> Result<Lease, SchedulerError> {
        let now = self.now();
        let lease = leases::find_lease(tx, id)?.ok_or(SchedulerError::NotFound)?;
        if lease.holder != who.holder {
            return Err(SchedulerError::NotHolder);
        }
        if lease.state != LeaseState::Offered {
            return Err(SchedulerError::BadState { state: lease.state });
        }
        if lease.claim_deadline.is_some_and(|d| now > d) {
            return Err(SchedulerError::BadState {
                state: LeaseState::Ended,
            });
        }
        let ends_at = now.saturating_add(lease.duration_ms);
        leases::set_held(tx, lease.row, ends_at)?;
        if lease.paused_for_lease.is_some() {
            // A borrower's declared duration, fixed now: extend moves
            // ends_at but not this.
            leases::set_borrow_due(tx, lease.row, ends_at)?;
        }
        audit::record(tx, now, who, "claimed", &json!({ "lease": id }))?;
        let lease = leases::find_lease(tx, id)?.ok_or(SchedulerError::NotFound)?;
        Self::lease_view(tx, lease)
    }

    fn lease_view(tx: &Tx, lease: LeaseRow) -> Result<Lease, SchedulerError> {
        let resource = inventory_sql::load_resource(tx, lease.resource_row)?;
        let unit = resource
            .units
            .iter()
            .find(|u| u.row == lease.unit_row)
            .ok_or(SchedulerError::NotFound)?;
        let brief = brief(&resource, unit);
        Ok(Lease {
            id: lease.id,
            resource: resource.id,
            unit: unit.id.clone(),
            settings: unit.settings.clone(),
            resource_description: brief.resource_description,
            unit_description: brief.unit_description,
            usage: brief.usage,
            resource_tags: brief.resource_tags,
            state: lease.state,
            holder: lease.holder,
            priority: lease.priority,
            interruptible: lease.interruptible,
            purpose: lease.purpose,
            duration: from_millis(lease.duration_ms),
            created_at: lease.created_at,
            ends_at: lease.ends_at,
            claim_deadline: lease.claim_deadline,
            cleanup_deadline: lease.cleanup_deadline,
            override_reason: lease.override_reason,
            dirty: lease.dirty,
            yield_deadline: lease.yield_deadline,
            paused_remaining: lease.paused_remaining_ms.map(from_millis),
            end_cause: lease.end_cause,
            request_id: lease.request_id,
        })
    }

    /// One of the caller's leases, in any state.
    ///
    /// # Errors
    /// [`SchedulerError::NotFound`] for an unknown lease or one the caller
    /// does not hold (others' leases are not disclosed).
    pub fn get_lease(&self, tx: &Tx, who: &Caller, id: &LeaseId) -> Result<Lease, SchedulerError> {
        let lease = leases::find_lease(tx, id)?.ok_or(SchedulerError::NotFound)?;
        if lease.holder != who.holder {
            return Err(SchedulerError::NotFound);
        }
        Self::lease_view(tx, lease)
    }

    /// The holder of live lease `id`, only when it is a `User` holder: the
    /// user's CLI acts as that holder on leases it made. Any other holder
    /// is not disclosed.
    ///
    /// # Errors
    /// [`SchedulerError::NotFound`] for an unknown or ended lease, or one
    /// held by anything but a `User` holder.
    pub fn user_holder(&self, tx: &Tx, id: &LeaseId) -> Result<HolderRef, SchedulerError> {
        let lease = leases::find_lease(tx, id)?.ok_or(SchedulerError::NotFound)?;
        match lease.holder {
            holder @ HolderRef::User { .. } if lease.state != LeaseState::Ended => Ok(holder),
            _ => Err(SchedulerError::NotFound),
        }
    }

    /// The holder of request `id` while it is queued or its offer is
    /// pending, only when it is a `User` holder: the user's CLI acts as
    /// that holder to claim or cancel it. Any other holder is not
    /// disclosed.
    ///
    /// # Errors
    /// [`SchedulerError::NotFound`] for an unknown request, one that is
    /// neither queued nor offered, or one of anything but a `User` holder.
    pub fn user_request_holder(
        &self,
        tx: &Tx,
        id: &RequestId,
    ) -> Result<HolderRef, SchedulerError> {
        let holder = match leases::find_request(tx, id)? {
            Some(request) => request.holder,
            None => {
                leases::offer_for_request(tx, id)?
                    .ok_or(SchedulerError::NotFound)?
                    .holder
            }
        };
        match holder {
            HolderRef::User { .. } => Ok(holder),
            _ => Err(SchedulerError::NotFound),
        }
    }

    /// The caller's live leases (oldest first) and queued requests (in
    /// queue order, with positions).
    ///
    /// # Errors
    /// [`SchedulerError::Store`] on storage failure.
    pub fn my_leases(&self, tx: &Tx, who: &Caller) -> Result<MyLeases, SchedulerError> {
        let mut mine = Vec::new();
        for lease in leases::live_leases(tx)? {
            if lease.holder == who.holder {
                mine.push(Self::lease_view(tx, lease)?);
            }
        }
        let requests = QueueSnapshot::load(tx)?
            .entries()
            .filter(|e| e.request.holder == who.holder)
            .map(|e| MyRequest {
                request_id: e.request.id.clone(),
                resource: e.resource.clone(),
                unit: e.unit.clone(),
                position: e.position,
                priority: e.request.priority,
                interruptible: e.request.interruptible,
                purpose: e.request.purpose.clone(),
                created_at: e.request.created_at,
            })
            .collect();
        Ok(MyLeases {
            leases: mine,
            requests,
        })
    }

    /// Withdraw a queued request. Waiters behind it are told their new
    /// positions. A request that was already offered a unit (and so left
    /// the queue) withdraws the offer instead: the offered lease ends as
    /// `Unclaimed` at once and the unit goes to the next in line.
    ///
    /// # Errors
    /// [`SchedulerError::NotFound`]; [`SchedulerError::NotHolder`] when the
    /// request is someone else's.
    pub fn cancel(&self, tx: &Tx, who: &Caller, id: &RequestId) -> Result<(), SchedulerError> {
        let Some(request) = leases::find_request(tx, id)? else {
            let offer = leases::offer_for_request(tx, id)?.ok_or(SchedulerError::NotFound)?;
            if offer.holder != who.holder {
                return Err(SchedulerError::NotHolder);
            }
            audit::record(
                tx,
                self.now(),
                who,
                "request_cancelled",
                &json!({ "request": id, "offer": offer.id }),
            )?;
            return self.end_lease(tx, &offer.id, EndCause::Unclaimed, true);
        };
        if request.holder != who.holder {
            return Err(SchedulerError::NotHolder);
        }
        let before = QueueSnapshot::load(tx)?;
        self.remove_request(tx, &request)?;
        let after = QueueSnapshot::load(tx)?;
        self.publish_moves(tx, &before, &after)?;
        audit::record(
            tx,
            self.now(),
            who,
            "request_cancelled",
            &json!({ "request": id }),
        )?;
        Ok(())
    }

    /// Units, holders and queues of the resources in `view`. A holder whose
    /// resource the caller may not see is reported only as
    /// [`Occupancy::HeldInAnotherProject`].
    ///
    /// # Errors
    /// [`SchedulerError::Store`] on storage failure.
    pub fn status(&self, tx: &Tx, who: &Caller, view: View) -> Result<StatusView, SchedulerError> {
        let filter = match view {
            View::All => ScopeFilter::All,
            View::Caller => ScopeFilter::Visible(who.project.as_ref()),
        };
        let queue = QueueSnapshot::load(tx)?;
        let mut lease_resources: HashMap<i64, Resource> = HashMap::new();
        let mut out = Vec::new();
        for resource in inventory_sql::list_resources(tx, filter)? {
            let mut units = Vec::new();
            for unit in &resource.units {
                let live = leases::live_leases_on(tx, &group(tx, unit.row)?)?;
                let occupancy = match live.into_iter().next() {
                    None => Occupancy::Free,
                    Some(lease) => {
                        if let Entry::Vacant(slot) = lease_resources.entry(lease.resource_row) {
                            slot.insert(inventory_sql::load_resource(tx, lease.resource_row)?);
                        }
                        let held_on = &lease_resources[&lease.resource_row];
                        if view == View::All || can_see(who.project.as_ref(), &held_on.scope) {
                            let lease_unit = held_on
                                .units
                                .iter()
                                .find(|u| u.row == lease.unit_row)
                                .ok_or(StoreError::Corrupt("leases.unit_row"))?;
                            Occupancy::Held(HolderView {
                                holder: lease.holder,
                                lease_id: lease.id,
                                state: lease.state,
                                purpose: lease.purpose,
                                resource: held_on.id.clone(),
                                unit: lease_unit.id.clone(),
                                ends_at: lease.ends_at,
                            })
                        } else {
                            Occupancy::HeldInAnotherProject
                        }
                    }
                };
                units.push(UnitStatus {
                    id: unit.id.clone(),
                    description: unit.description.clone(),
                    retiring: unit.retiring,
                    dirty: unit.dirty.clone(),
                    last_health: unit.last_health.clone(),
                    last_health_at: unit.last_health_at,
                    occupancy,
                });
            }
            let entries = queue
                .entries()
                .filter(|e| e.request.resource_row == resource.row)
                .map(|e| QueueEntry {
                    request_id: e.request.id.clone(),
                    holder: e.request.holder.clone(),
                    priority: e.request.priority,
                    purpose: e.request.purpose.clone(),
                    position: e.position,
                    unit: e.unit.clone(),
                    created_at: e.request.created_at,
                })
                .collect();
            out.push(ResourceStatus {
                id: resource.id,
                ty: resource.ty,
                scope: resource.scope,
                description: resource.description,
                tags: resource.tags,
                shadows_user: resource.shadows_user,
                units,
                queue: entries,
            });
        }
        Ok(StatusView { resources: out })
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn user_scope_is_visible_to_everyone() {
        assert!(can_see(None, &Scope::User));
        assert!(can_see(
            Some(&ProjectKey(PathBuf::from("/a"))),
            &Scope::User
        ));
    }

    #[test]
    fn project_scope_is_visible_only_to_that_project() {
        let a = ProjectKey(PathBuf::from("/a"));
        let b = ProjectKey(PathBuf::from("/b"));
        assert!(can_see(Some(&a), &Scope::Project(a.clone())));
        assert!(!can_see(Some(&b), &Scope::Project(a.clone())));
        assert!(!can_see(None, &Scope::Project(a)));
    }

    #[test]
    fn invisible_holder_reads_as_another_project() {
        assert_eq!(
            Occupancy::HeldInAnotherProject.to_string(),
            "held by an agent in another project"
        );
    }
}
