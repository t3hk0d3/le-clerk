//! Dispatch: withdraw lapsed offers, remove retired units, and offer every
//! free identity group to its best waiter. Also the queue snapshot that
//! positions and estimates are computed from.

use std::cmp::Reverse;
use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap, HashSet};

use serde_json::json;

use crate::model::{
    DirtyReason, EndCause, LeaseState, Priority, Resource, ResourceId, Unit, UnitId,
};
use crate::scheduler::{
    Ask, HandedOver, Scheduler, SchedulerError, Verdict, clerk_actor, group, group_busy, notify,
};
use crate::store::inventory as inventory_sql;
use crate::store::leases::{self, LeaseRow, RequestRow};
use crate::store::notices::{Notice, NoticeKind};
use crate::store::{Tx, audit};

/// Results of checks run in place in one scheduling pass, by unit row, so
/// that a unit is checked at most once per pass.
pub(super) type HealthCache = HashMap<i64, bool>;

/// Queue order: override requests that displaced a lease first, then higher
/// priority, then by row id: insertion order, never the clock (which can
/// step back) or the ULID.
pub(super) fn queue_key(r: &RequestRow) -> (bool, Reverse<Priority>, i64) {
    (r.override_lease.is_none(), Reverse(r.priority), r.row)
}

/// One queued request with what it competes for.
pub(super) struct Waiting {
    pub(super) request: RequestRow,
    pub(super) resource: ResourceId,
    pub(super) unit: Option<UnitId>,
    /// Every unit row in the identity groups of the request's target units.
    targets: HashSet<i64>,
    /// 1 + the number of earlier requests (in queue order) whose target
    /// identity groups intersect this request's.
    pub(super) position: u32,
}

/// All queued requests in queue order, with positions.
pub(super) struct QueueSnapshot {
    entries: Vec<Waiting>,
}

impl QueueSnapshot {
    pub(super) fn load(tx: &Tx) -> Result<Self, SchedulerError> {
        let mut requests = leases::queued_requests(tx)?;
        requests.sort_by_key(queue_key);
        let mut resources: HashMap<i64, Resource> = HashMap::new();
        let mut groups: HashMap<i64, Vec<i64>> = HashMap::new();
        let mut entries: Vec<Waiting> = Vec::with_capacity(requests.len());
        for request in requests {
            if let Entry::Vacant(slot) = resources.entry(request.resource_row) {
                slot.insert(inventory_sql::load_resource(tx, request.resource_row)?);
            }
            let resource = &resources[&request.resource_row];
            let target_units: Vec<i64> = match request.unit_row {
                Some(u) => vec![u],
                None => resource.units.iter().map(|u| u.row).collect(),
            };
            let mut targets = HashSet::new();
            for unit in target_units {
                if let Entry::Vacant(slot) = groups.entry(unit) {
                    slot.insert(group(tx, unit)?);
                }
                targets.extend(groups[&unit].iter().copied());
            }
            let ahead = entries
                .iter()
                .filter(|e| !e.targets.is_disjoint(&targets))
                .count();
            let unit = request
                .unit_row
                .and_then(|row| resource.units.iter().find(|u| u.row == row))
                .map(|u| u.id.clone());
            entries.push(Waiting {
                resource: resource.id.clone(),
                unit,
                targets,
                position: u32::try_from(ahead + 1).unwrap_or(u32::MAX),
                request,
            });
        }
        Ok(Self { entries })
    }

    fn find(&self, row: i64) -> Option<&Waiting> {
        self.entries.iter().find(|e| e.request.row == row)
    }

    pub(super) fn entries(&self) -> impl Iterator<Item = &Waiting> {
        self.entries.iter()
    }

    pub(super) fn position(&self, row: i64) -> Option<u32> {
        self.find(row).map(|e| e.position)
    }

    /// The first request, in queue order, of priority strictly above
    /// `priority` whose targets include any of `units`.
    pub(super) fn first_above(&self, priority: Priority, units: &[i64]) -> Option<&RequestRow> {
        self.entries
            .iter()
            .filter(|e| e.request.priority > priority)
            .find(|e| units.iter().any(|u| e.targets.contains(u)))
            .map(|e| &e.request)
    }

    /// Requests ahead of `row` that compete with it, in queue order.
    pub(super) fn ahead_of(&self, row: i64) -> Vec<&RequestRow> {
        let Some(me) = self.find(row) else {
            return Vec::new();
        };
        self.entries
            .iter()
            .take_while(|e| e.request.row != row)
            .filter(|e| !e.targets.is_disjoint(&me.targets))
            .map(|e| &e.request)
            .collect()
    }
}

impl Scheduler {
    /// Bring the queue up to date: withdraw offers past their claim
    /// deadline, give lent units back to their paused holders (ahead of any
    /// queue), remove retiring units that are no longer leased, and offer
    /// every free identity group to its best waiter. Idempotent.
    ///
    /// # Errors
    /// [`SchedulerError::Store`] on storage failure.
    pub fn dispatch(&self, tx: &Tx) -> Result<(), SchedulerError> {
        self.dispatch_with(tx, &mut HealthCache::new(), self.now())
    }

    /// [`Scheduler::dispatch`], withdrawing only offers whose claim deadline
    /// is before `horizon`. `tick` passes the deadline it is processing, so
    /// that a backlog of overdue deadlines is handled in deadline order;
    /// everyone else passes the current time.
    pub(super) fn dispatch_with(
        &self,
        tx: &Tx,
        cache: &mut HealthCache,
        horizon: i64,
    ) -> Result<(), SchedulerError> {
        let before = QueueSnapshot::load(tx)?;
        self.expire_offers(tx, horizon)?;
        self.return_paused_units(tx)?;
        self.sweep_retiring(tx)?;
        self.call_in_overdue_leases(tx)?;
        self.offer_free_groups(tx, cache)?;
        let after = QueueSnapshot::load(tx)?;
        self.publish_moves(tx, &before, &after)
    }

    /// Tell every waiter present in both snapshots whose position changed.
    pub(super) fn publish_moves(
        &self,
        tx: &Tx,
        before: &QueueSnapshot,
        after: &QueueSnapshot,
    ) -> Result<(), SchedulerError> {
        for entry in after.entries() {
            let Some(old) = before.position(entry.request.row) else {
                continue;
            };
            if old == entry.position {
                continue;
            }
            let mut notice = Notice::new(
                entry.request.holder.clone(),
                NoticeKind::PositionChanged,
                format!(
                    "You are now number {} in the queue for {}; keep waiting, or cancel if \
                     you no longer need it.",
                    entry.position, entry.resource
                ),
                self.now(),
            );
            notice.request_id = Some(entry.request.id.clone());
            notice.resource = Some(entry.resource.clone());
            notice.unit.clone_from(&entry.unit);
            notice.position = Some(entry.position);
            notify(tx, notice, entry.request.project.as_ref(), None)?;
        }
        Ok(())
    }

    /// End offers whose claim deadline is before `horizon`, give back the
    /// dirty record they carried, and tell their holders the request is gone.
    fn expire_offers(&self, tx: &Tx, horizon: i64) -> Result<(), SchedulerError> {
        let now = self.now();
        for lease in leases::lapsed_offers(tx, horizon)? {
            // Never used, so there was nothing to clean.
            leases::set_ended(tx, lease.row, EndCause::Unclaimed, true)?;
            let unit = inventory_sql::unit_by_row(tx, lease.unit_row)?;
            if let (Some(dirty), Some(unit)) = (&lease.dirty, &unit)
                && unit.dirty.is_none()
            {
                inventory_sql::set_dirty(tx, unit.row, Some(dirty))?;
            }
            let resource = inventory_sql::load_resource(tx, lease.resource_row)?;
            let mut notice = Notice::new(
                lease.holder.clone(),
                NoticeKind::QueueDropped,
                format!(
                    "Your offer of {} lapsed unclaimed, so your request was dropped; acquire \
                     again if you still need it.",
                    resource.id
                ),
                now,
            );
            notice.lease_id = Some(lease.id.clone());
            notice.request_id.clone_from(&lease.request_id);
            notice.resource = Some(resource.id.clone());
            notice.unit = unit.map(|u| u.id);
            notify(tx, notice, lease.project.as_ref(), None)?;
            audit::record(
                tx,
                now,
                &clerk_actor(),
                "offer_expired",
                &json!({ "lease": lease.id, "holder": lease.holder }),
            )?;
        }
        Ok(())
    }

    /// Remove retiring units without a live lease. Requests pinned to such a
    /// unit are dropped first, with a notice. Then every resource-level
    /// request whose resource has no non-retiring unit left (whichever path
    /// removed or retired its units) can never be served and is dropped too.
    fn sweep_retiring(&self, tx: &Tx) -> Result<(), SchedulerError> {
        let now = self.now();
        for row in inventory_sql::retiring_unit_rows(tx)? {
            if leases::any_live_on(tx, &[row])? {
                continue;
            }
            let Some(unit) = inventory_sql::unit_by_row(tx, row)? else {
                continue;
            };
            let resource = inventory_sql::load_resource(tx, unit.resource_row)?;
            for request in leases::queued_requests(tx)?
                .into_iter()
                .filter(|r| r.unit_row == Some(row))
            {
                self.remove_request(tx, &request)?;
                let mut notice = Notice::new(
                    request.holder.clone(),
                    NoticeKind::QueueDropped,
                    format!(
                        "Unit {} of {} was retired, so your request for it was dropped; ask \
                         for the resource instead.",
                        unit.id, resource.id
                    ),
                    now,
                );
                notice.request_id = Some(request.id.clone());
                notice.resource = Some(resource.id.clone());
                notice.unit = Some(unit.id.clone());
                notify(tx, notice, request.project.as_ref(), None)?;
            }
            inventory_sql::delete_unit(tx, row, now, &clerk_actor())?;
            audit::record(
                tx,
                now,
                &clerk_actor(),
                "unit_removed",
                &json!({ "resource": resource.id, "unit": unit.id, "retired": true }),
            )?;
        }
        let waited_on: BTreeSet<i64> = leases::queued_requests(tx)?
            .into_iter()
            .filter(|r| r.unit_row.is_none())
            .map(|r| r.resource_row)
            .collect();
        for resource_row in waited_on {
            let resource = inventory_sql::load_resource(tx, resource_row)?;
            if resource.units.iter().any(|u| !u.retiring) {
                continue;
            }
            let why = if resource.units.is_empty() {
                "has no units left"
            } else {
                "has only retired units left"
            };
            for request in leases::queued_requests(tx)?
                .into_iter()
                .filter(|r| r.resource_row == resource_row && r.unit_row.is_none())
            {
                self.remove_request(tx, &request)?;
                let mut notice = Notice::new(
                    request.holder.clone(),
                    NoticeKind::QueueDropped,
                    format!(
                        "{} {why}, so your request was dropped; ask for another resource.",
                        resource.id
                    ),
                    now,
                );
                notice.request_id = Some(request.id.clone());
                notice.resource = Some(resource.id.clone());
                notify(tx, notice, request.project.as_ref(), None)?;
            }
        }
        Ok(())
    }

    /// Whether a queued request could use `lease`'s unit (or a unit linked
    /// to it).
    pub(super) fn has_waiter(tx: &Tx, lease: &LeaseRow) -> Result<bool, SchedulerError> {
        let members = group(tx, lease.unit_row)?;
        let mut resources: HashSet<i64> = HashSet::new();
        for row in &members {
            if let Some(u) = inventory_sql::unit_by_row(tx, *row)? {
                resources.insert(u.resource_row);
            }
        }
        Ok(leases::queued_requests(tx)?
            .iter()
            .any(|r| match r.unit_row {
                Some(u) => members.contains(&u),
                None => resources.contains(&r.resource_row),
            }))
    }

    /// Start the cleanup of every overdue lease whose unit someone wants.
    fn call_in_overdue_leases(&self, tx: &Tx) -> Result<(), SchedulerError> {
        for lease in leases::live_leases(tx)? {
            if lease.state == LeaseState::Held && lease.overdue && Self::has_waiter(tx, &lease)? {
                self.call_in_overdue(tx, &lease)?;
            }
        }
        Ok(())
    }

    /// Offer each free group to the best waiter that can use it.
    fn offer_free_groups(&self, tx: &Tx, cache: &mut HealthCache) -> Result<(), SchedulerError> {
        let mut seen: HashSet<i64> = HashSet::new();
        for row in inventory_sql::all_unit_rows(tx)? {
            if seen.contains(&row) {
                continue;
            }
            let members_rows = group(tx, row)?;
            seen.extend(members_rows.iter().copied());
            if group_busy(tx, &members_rows)? {
                continue;
            }
            let mut members: Vec<Unit> = Vec::new();
            for r in &members_rows {
                if let Some(u) = inventory_sql::unit_by_row(tx, *r)? {
                    members.push(u);
                }
            }
            if members.iter().all(|u| u.retiring) {
                continue;
            }
            let member_resources: HashSet<i64> = members.iter().map(|u| u.resource_row).collect();
            let mut waiters: Vec<RequestRow> = leases::queued_requests(tx)?
                .into_iter()
                .filter(|r| match r.unit_row {
                    Some(u) => members_rows.contains(&u),
                    None => member_resources.contains(&r.resource_row),
                })
                .collect();
            waiters.sort_by_key(queue_key);
            'waiters: for waiter in &waiters {
                let resource = inventory_sql::load_resource(tx, waiter.resource_row)?;
                let mut candidates: Vec<&Unit> = members
                    .iter()
                    .filter(|u| {
                        !u.retiring
                            && u.resource_row == waiter.resource_row
                            && waiter.unit_row.is_none_or(|p| p == u.row)
                    })
                    .collect();
                candidates.sort_by(|a, b| a.id.cmp(&b.id));
                for unit in candidates {
                    let verdict = if waiter.skip_checks {
                        Verdict::Healthy
                    } else {
                        self.healthy(tx, cache, &resource, unit)?
                    };
                    match verdict {
                        Verdict::Healthy => {
                            self.offer(tx, &resource, unit, waiter)?;
                            break 'waiters;
                        }
                        Verdict::Unhealthy => {}
                        // The group waits for the result rather than go to
                        // someone later in the queue; it re-enters dispatch.
                        Verdict::Checking => break 'waiters,
                    }
                }
            }
        }
        Ok(())
    }

    /// Offer `unit` to `waiter`: an offered lease with a claim deadline, the
    /// request leaves the queue, and the waiter is told. The caller has
    /// checked, in this transaction, that the unit may be handed over.
    pub(super) fn offer(
        &self,
        tx: &Tx,
        resource: &Resource,
        unit: &Unit,
        waiter: &RequestRow,
    ) -> Result<HandedOver, SchedulerError> {
        let ask = Ask {
            holder: &waiter.holder,
            project: waiter.project.as_ref(),
            priority: waiter.priority,
            interruptible: waiter.interruptible,
            purpose: &waiter.purpose,
            duration_ms: waiter.duration_ms,
            override_reason: waiter.override_reason.as_deref(),
            request_id: Some(&waiter.id),
        };
        let handed = self.hand_over(tx, resource, unit, &ask, LeaseState::Offered)?;
        self.remove_request(tx, waiter)?;
        let abandoned = if handed
            .dirty
            .as_ref()
            .is_some_and(|d| d.reason == DirtyReason::Abandoned)
        {
            " The previous owner abandoned it: check the unit before you rely on it."
        } else {
            ""
        };
        let mut notice = Notice::new(
            waiter.holder.clone(),
            NoticeKind::Offered,
            format!(
                "Unit {} of {} is ready for you; claim lease {} before the deadline or it \
                 goes to the next in line.{abandoned}",
                unit.id, resource.id, handed.id
            ),
            self.now(),
        );
        notice.lease_id = Some(handed.id.clone());
        notice.request_id = Some(waiter.id.clone());
        notice.resource = Some(resource.id.clone());
        notice.unit = Some(unit.id.clone());
        notice.deadline = handed.claim_deadline;
        notice.dirty.clone_from(&handed.dirty);
        notify(tx, notice, waiter.project.as_ref(), None)?;
        audit::record(
            tx,
            self.now(),
            &clerk_actor(),
            "offered",
            &json!({
                "lease": handed.id,
                "request": waiter.id,
                "holder": waiter.holder,
                "resource": resource.id,
                "unit": unit.id,
                "override_reason": waiter.override_reason,
                "dirty": handed.dirty,
            }),
        )?;
        Ok(handed)
    }
}
