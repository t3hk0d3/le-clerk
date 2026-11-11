//! Response shapes of the RPC surface: the phase-2 JSON contract.
//!
//! Internal types are not serialised blindly. Durations travel as integer
//! milliseconds (`*_ms`), storage rows stay private, and every field name
//! here is part of the contract.

use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use crate::inventory::{Created, Promotion, Warning};
use crate::model::{
    DirtyInfo, EndCause, HealthSpec, HolderRef, LeaseId, LeaseState, Priority, ProjectKey,
    PromotionId, RequestId, Resource, ResourceId, ResourceType, Scope, TagSet, Unit, UnitId,
    UnitSettings,
};
use crate::scheduler::{AcquireOutcome, BlockedBy, Displaced, Grant, Lease, MyLeases, MyRequest};
use crate::store::audit::AuditEntry;

/// Milliseconds in `d`, saturating.
#[must_use]
pub fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// A resource with its units.
#[derive(Debug, Serialize)]
pub struct ResourceView {
    pub id: ResourceId,
    #[serde(rename = "type")]
    pub ty: ResourceType,
    pub scope: Scope,
    pub description: String,
    pub usage: String,
    /// `None` means the type default.
    pub health: Option<HealthSpec>,
    /// Search tags, sorted.
    pub tags: TagSet,
    pub created_at: i64,
    /// True when this project resource hides a user resource of the same id.
    pub shadows_user: bool,
    pub units: Vec<UnitView>,
}

impl From<Resource> for ResourceView {
    fn from(r: Resource) -> Self {
        Self {
            id: r.id,
            ty: r.ty,
            scope: r.scope,
            description: r.description,
            usage: r.usage,
            health: r.health,
            tags: r.tags,
            created_at: r.created_at,
            shadows_user: r.shadows_user,
            units: r.units.into_iter().map(UnitView::from).collect(),
        }
    }
}

/// One unit of a resource.
#[derive(Debug, Serialize)]
pub struct UnitView {
    pub id: UnitId,
    pub settings: UnitSettings,
    pub description: String,
    pub dirty: Option<DirtyInfo>,
    pub retiring: bool,
    pub last_health: Option<String>,
    pub last_health_at: Option<i64>,
    pub created_at: i64,
}

impl From<Unit> for UnitView {
    fn from(u: Unit) -> Self {
        Self {
            id: u.id,
            settings: u.settings,
            description: u.description,
            dirty: u.dirty,
            retiring: u.retiring,
            last_health: u.last_health,
            last_health_at: u.last_health_at,
            created_at: u.created_at,
        }
    }
}

/// A created or changed resource plus its warnings.
#[derive(Debug, Serialize)]
pub struct CreatedResource {
    pub resource: ResourceView,
    pub warnings: Vec<Warning>,
}

impl From<Created<Resource>> for CreatedResource {
    fn from(c: Created<Resource>) -> Self {
        Self {
            resource: c.value.into(),
            warnings: c.warnings,
        }
    }
}

/// An added or changed unit plus its warnings.
#[derive(Debug, Serialize)]
pub struct CreatedUnit {
    pub unit: UnitView,
    pub warnings: Vec<Warning>,
}

impl From<Created<Unit>> for CreatedUnit {
    fn from(c: Created<Unit>) -> Self {
        Self {
            unit: c.value.into(),
            warnings: c.warnings,
        }
    }
}

/// A pending promotion.
#[derive(Debug, Serialize)]
pub struct PromotionView {
    pub id: PromotionId,
    pub resource: ResourceId,
    /// The resource's scope now (a project).
    pub scope: Scope,
    pub description: String,
    pub requester: Option<ProjectKey>,
    pub reason: String,
    pub created_at: i64,
}

impl From<Promotion> for PromotionView {
    fn from(p: Promotion) -> Self {
        Self {
            id: p.id,
            resource: p.resource,
            scope: p.scope,
            description: p.description,
            requester: p.requester,
            reason: p.reason,
            created_at: p.created_at,
        }
    }
}

/// The answer to an acquire, tagged by `outcome`.
// One short-lived value per call; boxing the grant would only add noise for callers.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum AcquireView {
    /// A unit is the caller's now.
    Granted(Grant),
    /// The request waits in the queue.
    Queued {
        request_id: RequestId,
        position: u32,
        /// `null` when unknown.
        estimated_wait_ms: Option<u64>,
        /// What keeps the request from a unit now; `null` when nothing
        /// specific does.
        blocked_by: Option<BlockedBy>,
        /// The lease an override displaced for this request; absent
        /// otherwise.
        #[serde(skip_serializing_if = "Option::is_none")]
        displaced: Option<Displaced>,
        /// The caller had already queued for this target and this is that
        /// request; absent otherwise.
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        existing: bool,
    },
    /// A paused holder's override of its own borrower: the unit comes back.
    Reclaiming { lease_id: LeaseId },
}

impl AcquireView {
    /// The answer for `o`. `None` for [`AcquireOutcome::Checking`], which is
    /// between the engine and its handle and has no wire form.
    #[must_use]
    pub fn from_outcome(o: AcquireOutcome) -> Option<Self> {
        Some(match o {
            AcquireOutcome::Granted(g) => Self::Granted(g),
            AcquireOutcome::Queued {
                request_id,
                position,
                estimated_wait,
                blocked_by,
                displaced,
                existing,
            } => Self::Queued {
                request_id,
                position,
                estimated_wait_ms: estimated_wait.map(millis),
                blocked_by,
                displaced,
                existing,
            },
            AcquireOutcome::Reclaiming { lease_id } => Self::Reclaiming { lease_id },
            AcquireOutcome::Checking => return None,
        })
    }
}

/// A lease as seen by its holder.
// Independent facts about the lease, each its own field on the wire.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Serialize)]
pub struct LeaseView {
    pub lease_id: LeaseId,
    pub resource: ResourceId,
    pub unit: UnitId,
    pub settings: UnitSettings,
    pub resource_description: String,
    pub unit_description: String,
    /// The usage template rendered for this unit.
    pub usage: String,
    /// The resource's tags, sorted.
    pub resource_tags: TagSet,
    pub state: LeaseState,
    pub holder: HolderRef,
    pub priority: Priority,
    pub interruptible: bool,
    pub purpose: String,
    pub duration_ms: u64,
    pub created_at: i64,
    pub ends_at: Option<i64>,
    pub claim_deadline: Option<i64>,
    pub cleanup_deadline: Option<i64>,
    pub override_reason: Option<String>,
    pub dirty: Option<DirtyInfo>,
    pub yield_deadline: Option<i64>,
    pub paused_remaining_ms: Option<u64>,
    /// Once ended: why.
    pub end_cause: Option<EndCause>,
    /// The queued request the lease was offered for; `null` for a direct
    /// grant.
    pub request_id: Option<RequestId>,
    /// Held with its expiry warning out; absent otherwise.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub expiring: bool,
    /// Held past its end, until someone asks for the unit; absent otherwise.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub overdue: bool,
    /// A repeated `claim` or `confirm`: nothing changed; absent otherwise.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub already: bool,
    /// The holder's other queued requests for this unit, cancelled by a
    /// `claim`; absent when there are none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cancelled_requests: Vec<RequestId>,
}

/// The caller's live leases and queued requests (`list_my_leases`).
#[derive(Debug, Serialize)]
pub struct MyLeasesView {
    pub leases: Vec<LeaseView>,
    pub requests: Vec<MyRequest>,
}

impl From<MyLeases> for MyLeasesView {
    fn from(m: MyLeases) -> Self {
        Self {
            leases: m.leases.into_iter().map(LeaseView::from).collect(),
            requests: m.requests,
        }
    }
}

impl From<Lease> for LeaseView {
    fn from(l: Lease) -> Self {
        Self {
            lease_id: l.id,
            resource: l.resource,
            unit: l.unit,
            settings: l.settings,
            resource_description: l.resource_description,
            unit_description: l.unit_description,
            usage: l.usage,
            resource_tags: l.resource_tags,
            state: l.state,
            holder: l.holder,
            priority: l.priority,
            interruptible: l.interruptible,
            purpose: l.purpose,
            duration_ms: millis(l.duration),
            created_at: l.created_at,
            ends_at: l.ends_at,
            claim_deadline: l.claim_deadline,
            cleanup_deadline: l.cleanup_deadline,
            override_reason: l.override_reason,
            dirty: l.dirty,
            yield_deadline: l.yield_deadline,
            paused_remaining_ms: l.paused_remaining.map(millis),
            end_cause: l.end_cause,
            request_id: l.request_id,
            expiring: l.expiring,
            overdue: l.overdue,
            already: l.already,
            cancelled_requests: l.cancelled_requests,
        }
    }
}

/// One audit log entry.
#[derive(Debug, Serialize)]
pub struct AuditView {
    pub id: i64,
    pub at: i64,
    pub actor: HolderRef,
    pub override_reason: Option<String>,
    pub action: String,
    pub detail: Value,
}

impl From<AuditEntry> for AuditView {
    fn from(e: AuditEntry) -> Self {
        Self {
            id: e.id,
            at: e.at,
            actor: e.actor,
            override_reason: e.override_reason,
            action: e.action,
            detail: e.detail,
        }
    }
}
