//! Scheduler primitives for administrator actions and holder re-homing:
//! dropping requests with a notice, forcing leases off units that are being
//! removed, editing a lease, handing a lease to another holder, and
//! repairing rows a damaged database left dangling. The rules for when each
//! is used live in `admin`.

use serde_json::json;

use crate::model::{
    Caller, EndCause, HolderRef, LeaseId, LeaseState, Priority, RequestId, ResourceId,
};
use crate::scheduler::dispatch::QueueSnapshot;
use crate::scheduler::{Lease, Scheduler, SchedulerError, notify};
use crate::store::inventory as inventory_sql;
use crate::store::leases::{self, LeaseRow, RequestRow};
use crate::store::notices::{self, Notice, NoticeKind};
use crate::store::{StoreError, Tx, audit};

/// Changes to a lease made by an administrator. `None` leaves a field alone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LeaseEdit {
    /// The new end, unix ms; only for a held lease.
    pub ends_at: Option<i64>,
    pub priority: Option<Priority>,
    /// Only for a held or offered lease.
    pub interruptible: Option<bool>,
}

impl Scheduler {
    /// Take `requests` out of the queue through `remove_request` (a holder
    /// asked to yield for one is told the ask is withdrawn), tell each one's
    /// holder with a `kind` notice whose action text `action` builds from the
    /// resource id, and tell the waiters behind their new positions. Returns
    /// the ids dropped.
    ///
    /// # Errors
    /// [`SchedulerError::Store`] on storage failure.
    pub fn drop_requests(
        &self,
        tx: &Tx,
        requests: &[RequestRow],
        kind: NoticeKind,
        action: &dyn Fn(&ResourceId) -> String,
    ) -> Result<Vec<RequestId>, SchedulerError> {
        let before = QueueSnapshot::load(tx)?;
        let now = self.now();
        let mut dropped = Vec::with_capacity(requests.len());
        for request in requests {
            let resource = inventory_sql::load_resource(tx, request.resource_row)?;
            let unit = request
                .unit_row
                .and_then(|row| resource.units.iter().find(|u| u.row == row))
                .map(|u| u.id.clone());
            self.remove_request(tx, request)?;
            let mut notice = Notice::new(request.holder.clone(), kind, action(&resource.id), now);
            notice.request_id = Some(request.id.clone());
            notice.resource = Some(resource.id.clone());
            notice.unit = unit;
            notify(tx, notice, request.project.as_ref(), None)?;
            dropped.push(request.id.clone());
        }
        let after = QueueSnapshot::load(tx)?;
        self.publish_moves(tx, &before, &after)?;
        Ok(dropped)
    }

    /// End every live lease on `units` (rows the caller is about to delete,
    /// already flagged retiring so nothing new is handed out on them): each
    /// holder first gets `ResourceRemoved` with `action`, then the lease
    /// ends with cause `AdminForced`, cleanup unconfirmed, through
    /// [`Scheduler::end_lease`]. Ending a borrower gives the unit back to its
    /// paused holder, whose lease is then ended in turn. Returns the leases
    /// ended.
    ///
    /// # Errors
    /// [`SchedulerError::Store`] on storage failure.
    pub fn force_off_units(
        &self,
        tx: &Tx,
        units: &[i64],
        action: &str,
    ) -> Result<Vec<LeaseId>, SchedulerError> {
        let mut ended: Vec<LeaseId> = Vec::new();
        while let Some(lease) = leases::live_leases_on(tx, units)?.into_iter().next() {
            // A lease ended here never comes back to life; seeing one again
            // would mean the loop cannot finish.
            if ended.contains(&lease.id) {
                return Err(StoreError::Corrupt("leases.state").into());
            }
            let notice =
                self.lease_notice(tx, &lease, NoticeKind::ResourceRemoved, action.into())?;
            notify(tx, notice, lease.project.as_ref(), None)?;
            self.end_lease(tx, &lease.id, EndCause::AdminForced, false)?;
            ended.push(lease.id);
        }
        Ok(ended)
    }

    /// Apply an administrator's `edit` to lease `id`. A new `ends_at`
    /// re-arms the expiry warning (its lead computed from the new total
    /// duration); one in the past is expired by the next tick. Nothing is
    /// written unless every change is allowed.
    ///
    /// # Errors
    /// [`SchedulerError::NotFound`]; [`SchedulerError::BadState`] for an
    /// ended lease, an `ends_at` on a lease that is not held, or an
    /// `interruptible` on one that is neither held nor offered.
    pub fn edit_lease(
        &self,
        tx: &Tx,
        id: &LeaseId,
        edit: LeaseEdit,
    ) -> Result<Lease, SchedulerError> {
        let lease = leases::find_lease(tx, id)?.ok_or(SchedulerError::NotFound)?;
        let bad = SchedulerError::BadState { state: lease.state };
        if lease.state == LeaseState::Ended
            || (edit.ends_at.is_some() && lease.state != LeaseState::Held)
            || (edit.interruptible.is_some()
                && !matches!(lease.state, LeaseState::Held | LeaseState::Offered))
        {
            return Err(bad);
        }
        if let Some(new_end) = edit.ends_at {
            let old_end = lease.ends_at.ok_or(StoreError::Corrupt("leases.ends_at"))?;
            let duration_ms = lease
                .duration_ms
                .saturating_add(new_end.saturating_sub(old_end))
                .max(0);
            leases::set_extended(tx, lease.row, new_end, duration_ms)?;
        }
        if let Some(priority) = edit.priority {
            leases::set_priority(tx, lease.row, priority)?;
        }
        if let Some(interruptible) = edit.interruptible
            && interruptible != lease.interruptible
        {
            leases::set_interruptible(tx, lease.row, interruptible)?;
            self.interruptible_changed(tx, &lease, interruptible)?;
        }
        Self::view(tx, id)
    }

    /// Hand lease `id` from its holder, the caller, to `new_holder`, with
    /// the lease's undelivered notices. Used when a CLI detaches and an
    /// agent session carries on.
    ///
    /// # Errors
    /// [`SchedulerError::NotFound`]; [`SchedulerError::NotHolder`] when the
    /// caller is not the holder; [`SchedulerError::BadState`] for an ended
    /// lease.
    pub fn rehome_holder(
        &self,
        tx: &Tx,
        who: &Caller,
        id: &LeaseId,
        new_holder: &HolderRef,
    ) -> Result<Lease, SchedulerError> {
        let lease = Self::own_lease(tx, who, id)?;
        if lease.state == LeaseState::Ended {
            return Err(SchedulerError::BadState { state: lease.state });
        }
        leases::set_holder(tx, lease.row, new_holder)?;
        let moved = notices::rehome(tx, &lease.holder, id, new_holder)?;
        audit::record(
            tx,
            self.now(),
            who,
            "rehomed",
            &json!({ "lease": id, "from": lease.holder, "to": new_holder, "notices": moved }),
        )?;
        Self::view(tx, id)
    }

    /// End a live lease whose unit or resource row is gone (a damaged
    /// database). There is no unit to mark dirty and no names to load, so
    /// this bypasses [`Scheduler::end_lease`]; the holder is told.
    ///
    /// # Errors
    /// [`SchedulerError::Store`] on storage failure.
    pub fn end_orphan_lease(&self, tx: &Tx, lease: &LeaseRow) -> Result<(), SchedulerError> {
        leases::set_ended(tx, lease.row, EndCause::AdminForced, false)?;
        if let Some(borrower) = lease.borrower_lease {
            leases::clear_paused_for(tx, borrower)?;
        }
        let mut notice = Notice::new(
            lease.holder.clone(),
            NoticeKind::LeaseEnded,
            format!(
                "Lease {} pointed at a unit that no longer exists, so an administrator ended \
                 it; stop using whatever it was for.",
                lease.id
            ),
            self.now(),
        );
        notice.lease_id = Some(lease.id.clone());
        notify(tx, notice, lease.project.as_ref(), None)?;
        Ok(())
    }

    /// Drop a queued request whose resource or unit row is gone (a damaged
    /// database), without the queue bookkeeping that would need it; the
    /// holder is told.
    ///
    /// # Errors
    /// [`SchedulerError::Store`] on storage failure.
    pub fn drop_orphan_request(&self, tx: &Tx, request: &RequestRow) -> Result<(), SchedulerError> {
        leases::delete_request(tx, request.row)?;
        let mut notice = Notice::new(
            request.holder.clone(),
            NoticeKind::QueueDropped,
            format!(
                "Request {} was for a resource that no longer exists, so it was dropped; \
                 acquire again if you still need something.",
                request.id
            ),
            self.now(),
        );
        notice.request_id = Some(request.id.clone());
        notify(tx, notice, request.project.as_ref(), None)?;
        Ok(())
    }
}
