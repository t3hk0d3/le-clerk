//! Interruption: yield requests, borrowing a paused holder's unit, reclaim,
//! and override.
//!
//! **Interrupt rule.** A queued request R may interrupt an interruptible
//! held lease L when `R.priority > L.priority`, or when the priorities are
//! equal and R's declared duration is shorter than L's remaining time.
//! Candidates are asked one at a time, most remaining time first, then
//! lowest priority. Every holder asked for R is remembered, so one that
//! refuses or lets `yield_window` pass is never asked again for R; when no
//! candidate is left, R simply waits in the queue.
//!
//! **Borrowing.** A holder that confirms "yielded" is paused: its clock
//! stops with the remaining time stored, and its unit is offered to R (the
//! borrower) with the normal claim window. The two leases link to each
//! other. A paused lease keeps its identity group busy for everyone else;
//! [`Scheduler::lend`] is the only place that writes a second live lease on
//! a group, and only while the paused lease is the group's sole live lease.
//! When the borrower's lease ends, however it ends, the next dispatch gives
//! the unit back to the paused holder before any queue.
//!
//! **Override.** An acquire carrying `override_reason` that has to queue
//! puts the held lease that started earliest (interruptible or not, never a
//! paused one) into cleaning with cause `Override`; when that lease ends it
//! is ended, not paused, and the overrider is first in the queue. Overriding
//! a borrower makes the overrider the paused holder's next borrower.

use std::cmp::Reverse;
use std::collections::HashSet;
use std::time::Duration;

use serde_json::json;

use crate::model::{Caller, EndCause, LeaseId, LeaseState, RequestId, Resource, Unit, UnitId};
use crate::scheduler::{
    Lease, OtherParty, Scheduler, SchedulerError, clerk_actor, group, millis, notify,
};
use crate::store::inventory as inventory_sql;
use crate::store::leases::{self, LeaseRow, RequestRow};
use crate::store::notices::NoticeKind;
use crate::store::{StoreError, Tx, audit};

/// True when request `r` may interrupt lease `l`, which has `remaining_ms`
/// left.
fn may_interrupt(r: &RequestRow, l: &LeaseRow, remaining_ms: i64) -> bool {
    r.priority > l.priority || (r.priority == l.priority && r.duration_ms < remaining_ms)
}

impl Scheduler {
    /// Live leases on the identity groups of the units `request` can use
    /// (its pinned unit, or every non-retiring unit of its resource),
    /// without duplicates, by row.
    fn leases_for_request(tx: &Tx, request: &RequestRow) -> Result<Vec<LeaseRow>, SchedulerError> {
        let resource = inventory_sql::load_resource(tx, request.resource_row)?;
        let mut members: Vec<i64> = Vec::new();
        for unit in resource
            .units
            .iter()
            .filter(|u| !u.retiring && request.unit_row.is_none_or(|p| p == u.row))
        {
            members.extend(group(tx, unit.row)?);
        }
        members.sort_unstable();
        members.dedup();
        let mut live = leases::live_leases_on(tx, &members)?;
        live.sort_by_key(|l| l.row);
        live.dedup_by_key(|l| l.row);
        Ok(live)
    }

    /// Ask the next holder `request` may interrupt, if `request` is still
    /// queued and has no ask pending. Called when an acquire queues, and
    /// whenever an ask for the request is refused, times out or is
    /// withdrawn.
    ///
    /// # Errors
    /// [`SchedulerError::Store`] on storage failure.
    pub(super) fn maybe_request_yield(
        &self,
        tx: &Tx,
        request: &RequestId,
    ) -> Result<(), SchedulerError> {
        let Some(request) = leases::find_request(tx, request)? else {
            return Ok(());
        };
        if leases::asked_for(tx, &request.id)?.is_some() {
            return Ok(());
        }
        let now = self.now();
        let asked: HashSet<i64> = leases::asked_leases(tx, request.row)?.into_iter().collect();
        let mut candidates: Vec<(i64, LeaseRow)> = Self::leases_for_request(tx, &request)?
            .into_iter()
            .filter(|l| {
                l.state == LeaseState::Held
                    && l.interruptible
                    // A borrower is never paused: one paused lease per group.
                    && l.paused_for_lease.is_none()
                    && l.yield_deadline.is_none()
                    && l.holder != request.holder
                    && !asked.contains(&l.row)
            })
            .filter_map(|l| {
                let remaining = l.ends_at? - now;
                (remaining > 0 && may_interrupt(&request, &l, remaining)).then_some((remaining, l))
            })
            .collect();
        candidates.sort_by_key(|(remaining, l)| (Reverse(*remaining), l.priority, l.row));
        let Some((_, lease)) = candidates.into_iter().next() else {
            return Ok(());
        };
        let deadline = now.saturating_add(millis(Duration::from_secs(self.config.yield_window)));
        leases::set_yield_asked(tx, lease.row, &request, now, deadline)?;
        let mut notice = self.lease_notice(
            tx,
            &lease,
            NoticeKind::YieldRequested,
            format!(
                "Someone needs the unit of lease {}; pause at a safe point, clean up and confirm \
                 \"yielded\" before the deadline (your remaining time is kept), or refuse.",
                lease.id
            ),
        )?;
        notice.deadline = Some(deadline);
        let scope = inventory_sql::load_resource(tx, request.resource_row)?.scope;
        notify(
            tx,
            notice,
            lease.project.as_ref(),
            Some(OtherParty {
                scope: &scope,
                purpose: &request.purpose,
            }),
        )?;
        audit::record(
            tx,
            now,
            &clerk_actor(),
            "yield_requested",
            &json!({
                "lease": lease.id,
                "holder": lease.holder,
                "request": request.id,
                "deadline": deadline,
            }),
        )?;
        Ok(())
    }

    /// Drop the yield ask pending on `lease` and move its request on to the
    /// next candidate.
    fn withdraw_ask(&self, tx: &Tx, lease: &LeaseRow) -> Result<(), SchedulerError> {
        leases::clear_yield_ask(tx, lease.row)?;
        if let Some(request) = &lease.yield_request {
            self.maybe_request_yield(tx, request)?;
        }
        Ok(())
    }

    /// A held lease the caller holds with a yield ask pending.
    fn asked_lease(tx: &Tx, who: &Caller, id: &LeaseId) -> Result<LeaseRow, SchedulerError> {
        let lease = Self::own_lease(tx, who, id)?;
        if lease.state != LeaseState::Held || lease.yield_deadline.is_none() {
            return Err(SchedulerError::BadState { state: lease.state });
        }
        Ok(lease)
    }

    /// Open or close a lease (held or offered) to interruption. Opening it
    /// lets waiting requests ask its holder to yield; closing it withdraws a
    /// pending ask, whose request moves on to the next holder.
    ///
    /// # Errors
    /// [`SchedulerError::NotFound`]; [`SchedulerError::NotHolder`];
    /// [`SchedulerError::BadState`] unless held or offered.
    pub fn set_interruptible(
        &self,
        tx: &Tx,
        who: &Caller,
        id: &LeaseId,
        interruptible: bool,
    ) -> Result<Lease, SchedulerError> {
        let lease = Self::own_lease(tx, who, id)?;
        if !matches!(lease.state, LeaseState::Held | LeaseState::Offered) {
            return Err(SchedulerError::BadState { state: lease.state });
        }
        leases::set_interruptible(tx, lease.row, interruptible)?;
        audit::record(
            tx,
            self.now(),
            who,
            "interruptible_set",
            &json!({ "lease": id, "interruptible": interruptible }),
        )?;
        self.interruptible_changed(tx, &lease, interruptible)?;
        Self::view(tx, id)
    }

    /// After `lease` (as it was before the change) was opened or closed to
    /// interruption: opening lets waiting requests ask its holder to yield;
    /// closing withdraws a pending ask, whose request moves on.
    pub(super) fn interruptible_changed(
        &self,
        tx: &Tx,
        lease: &LeaseRow,
        interruptible: bool,
    ) -> Result<(), SchedulerError> {
        if interruptible {
            let mut waiting = leases::queued_requests(tx)?;
            waiting.sort_by_key(super::dispatch::queue_key);
            for request in waiting {
                self.maybe_request_yield(tx, &request.id)?;
            }
        } else if lease.yield_deadline.is_some() {
            if let Some(request) = &lease.yield_request
                && let Some(request) = leases::find_request(tx, request)?
            {
                self.notify_withdrawn(tx, lease, &request)?;
            }
            self.withdraw_ask(tx, lease)?;
        }
        Ok(())
    }

    /// Refuse a yield request. The lease stays held; the request moves on to
    /// the next holder it may interrupt, and this holder is not asked again
    /// for it.
    ///
    /// # Errors
    /// [`SchedulerError::NotFound`]; [`SchedulerError::NotHolder`];
    /// [`SchedulerError::BadState`] unless held with a yield ask pending.
    pub fn refuse_yield(&self, tx: &Tx, who: &Caller, id: &LeaseId) -> Result<(), SchedulerError> {
        let lease = Self::asked_lease(tx, who, id)?;
        audit::record(
            tx,
            self.now(),
            who,
            "yield_refused",
            &json!({ "lease": id, "request": lease.yield_request }),
        )?;
        self.withdraw_ask(tx, &lease)
    }

    /// The holder of `lease` let its yield window pass (from `tick`).
    pub(super) fn yield_timed_out(&self, tx: &Tx, lease: &LeaseRow) -> Result<(), SchedulerError> {
        audit::record(
            tx,
            self.now(),
            &clerk_actor(),
            "yield_timed_out",
            &json!({ "lease": lease.id, "holder": lease.holder, "request": lease.yield_request }),
        )?;
        self.withdraw_ask(tx, lease)
    }

    /// The unit of `paused_unit`'s identity group that `request` can use:
    /// one of its resource (its pinned unit, if any) that is not retiring.
    fn borrow_unit(
        tx: &Tx,
        paused_unit: i64,
        request: &RequestRow,
    ) -> Result<Option<(Resource, Unit)>, SchedulerError> {
        let members = group(tx, paused_unit)?;
        let resource = inventory_sql::load_resource(tx, request.resource_row)?;
        let mut usable: Vec<&Unit> = resource
            .units
            .iter()
            .filter(|u| {
                !u.retiring
                    && members.contains(&u.row)
                    && request.unit_row.is_none_or(|p| p == u.row)
            })
            .collect();
        usable.sort_by(|a, b| a.id.cmp(&b.id));
        let Some(unit) = usable.first().map(|u| (*u).clone()) else {
            return Ok(None);
        };
        Ok(Some((resource, unit)))
    }

    /// Offer `unit` to `request` as the borrower of paused lease `paused`,
    /// and link the two leases. This is the double-grant guard's one
    /// exception: it writes a lease on a busy group, so it first checks that
    /// the paused lease is the group's only live lease.
    fn lend(
        &self,
        tx: &Tx,
        paused: &LeaseRow,
        resource: &Resource,
        unit: &Unit,
        request: &RequestRow,
    ) -> Result<LeaseId, SchedulerError> {
        let live = leases::live_leases_on(tx, &group(tx, paused.unit_row)?)?;
        let sole_paused = matches!(
            live.as_slice(),
            [only] if only.row == paused.row && only.state == LeaseState::Paused
        );
        if !sole_paused {
            // Unreachable while the invariants hold: a paused lease keeps its
            // group busy, so nothing else is written on it.
            return Err(StoreError::Corrupt("leases: lending a group that is not free").into());
        }
        let handed = self.offer(tx, resource, unit, request)?;
        leases::set_borrow_link(tx, paused.row, handed.row)?;
        Ok(handed.id)
    }

    /// Answer a yield request: pause the lease, store its remaining time,
    /// and offer its unit to the requester as borrower.
    pub(super) fn confirm_yielded(
        &self,
        tx: &Tx,
        who: &Caller,
        id: &LeaseId,
    ) -> Result<(), SchedulerError> {
        let now = self.now();
        let lease = Self::asked_lease(tx, who, id)?;
        if lease.yield_deadline.is_some_and(|d| now >= d) {
            // The window passed; the next tick moves the request on.
            return Err(SchedulerError::BadState { state: lease.state });
        }
        let ends_at = lease.ends_at.ok_or(StoreError::Corrupt("leases.ends_at"))?;
        if now >= ends_at {
            // Past its end: the next tick expires it; there is nothing left
            // to yield.
            return Err(SchedulerError::BadState { state: lease.state });
        }
        let request = match &lease.yield_request {
            Some(r) => leases::find_request(tx, r)?,
            None => None,
        };
        let Some(request) = request else {
            leases::clear_yield_ask(tx, lease.row)?;
            return Err(SchedulerError::BadState { state: lease.state });
        };
        let Some((resource, unit)) = Self::borrow_unit(tx, lease.unit_row, &request)? else {
            self.withdraw_ask(tx, &lease)?;
            return Err(SchedulerError::Retiring);
        };
        let remaining = ends_at - now;
        leases::set_paused(tx, lease.row, remaining)?;
        let paused = leases::lease_by_row(tx, lease.row)?.ok_or(SchedulerError::NotFound)?;
        let borrower = self.lend(tx, &paused, &resource, &unit, &request)?;
        audit::record(
            tx,
            now,
            who,
            "yielded",
            &json!({
                "lease": id,
                "request": request.id,
                "borrower_lease": borrower,
                "remaining_ms": remaining,
            }),
        )?;
        Ok(())
    }

    /// Give every paused lease whose borrower's lease has ended its unit
    /// back, ahead of any queue. Part of every dispatch, so a paused lease
    /// is resumed whichever way its borrower's lease ended. When the
    /// borrower was overridden, the overrider borrows next instead.
    pub(super) fn return_paused_units(&self, tx: &Tx) -> Result<(), SchedulerError> {
        for paused in leases::paused_leases(tx)? {
            let borrower = match paused.borrower_lease {
                Some(row) => leases::lease_by_row(tx, row)?,
                None => None,
            };
            if borrower
                .as_ref()
                .is_some_and(|b| b.state != LeaseState::Ended)
            {
                continue;
            }
            if let Some(b) = &borrower
                && self.lend_to_overrider(tx, &paused, b)?
            {
                continue;
            }
            self.resume(tx, &paused, borrower.as_ref())?;
        }
        Ok(())
    }

    /// Lend `paused`'s unit to the request that overrode its ended borrower
    /// `borrower`, if one is queued and can use the unit.
    fn lend_to_overrider(
        &self,
        tx: &Tx,
        paused: &LeaseRow,
        borrower: &LeaseRow,
    ) -> Result<bool, SchedulerError> {
        let overrider = leases::queued_requests(tx)?
            .into_iter()
            .find(|r| r.override_lease == Some(borrower.row));
        let Some(overrider) = overrider else {
            return Ok(false);
        };
        let Some((resource, unit)) = Self::borrow_unit(tx, paused.unit_row, &overrider)? else {
            return Ok(false);
        };
        self.lend(tx, paused, &resource, &unit, &overrider)?;
        Ok(true)
    }

    /// Resume a paused lease: held again with its stored remaining time, the
    /// expiry warning re-armed, and the unit's dirty record (if the borrower
    /// did not confirm cleanup) handed to it and named in `UnitReturned`.
    fn resume(
        &self,
        tx: &Tx,
        paused: &LeaseRow,
        borrower: Option<&LeaseRow>,
    ) -> Result<(), SchedulerError> {
        let now = self.now();
        let ends_at = now.saturating_add(paused.paused_remaining_ms.unwrap_or(0).max(0));
        let dirty = inventory_sql::unit_by_row(tx, paused.unit_row)?.and_then(|u| u.dirty);
        if dirty.is_some() {
            inventory_sql::set_dirty(tx, paused.unit_row, None)?;
        }
        leases::set_resumed(tx, paused.row, ends_at, dirty.as_ref())?;
        let confirmed = borrower.is_none_or(|b| b.cleanup_confirmed != Some(false));
        let action = if dirty.is_some() {
            format!(
                "Your unit is back on lease {} and your clock runs again, but the borrower did \
                 not confirm cleanup; check the unit's state before you resume.",
                paused.id
            )
        } else {
            format!(
                "Your unit is back on lease {} and your clock runs again; resume your work.",
                paused.id
            )
        };
        let mut notice = self.lease_notice(tx, paused, NoticeKind::UnitReturned, action)?;
        notice.deadline = Some(ends_at);
        notice.dirty.clone_from(&dirty);
        notice.cleanup_confirmed = Some(confirmed);
        notify(tx, notice, paused.project.as_ref(), None)?;
        audit::record(
            tx,
            now,
            &clerk_actor(),
            "unit_returned",
            &json!({
                "lease": paused.id,
                "borrower_lease": borrower.map(|b| &b.id),
                "cleanup_confirmed": confirmed,
                "ends_at": ends_at,
                "dirty": dirty,
            }),
        )?;
        Ok(())
    }

    /// A borrower ran past its declared duration (from `tick`): tell the
    /// paused holder, once.
    pub(super) fn borrower_overran(
        &self,
        tx: &Tx,
        borrower: &LeaseRow,
    ) -> Result<(), SchedulerError> {
        leases::set_overran_sent(tx, borrower.row)?;
        let paused = match borrower.paused_for_lease {
            Some(row) => leases::lease_by_row(tx, row)?,
            None => None,
        };
        let Some(paused) = paused.filter(|p| p.state == LeaseState::Paused) else {
            return Ok(());
        };
        let mut notice = self.lease_notice(
            tx,
            &paused,
            NoticeKind::BorrowerOverran,
            format!(
                "The borrower of your unit ran past its declared time; reclaim lease {} to get \
                 the unit back after the borrower's cleanup window, or keep waiting.",
                paused.id
            ),
        )?;
        notice.deadline = borrower.borrow_due_at;
        let scope = inventory_sql::load_resource(tx, borrower.resource_row)?.scope;
        notify(
            tx,
            notice,
            paused.project.as_ref(),
            Some(OtherParty {
                scope: &scope,
                purpose: &borrower.purpose,
            }),
        )?;
        audit::record(
            tx,
            self.now(),
            &clerk_actor(),
            "borrower_overran",
            &json!({ "lease": paused.id, "borrower_lease": borrower.id }),
        )?;
        Ok(())
    }

    /// Take a lent unit back from a borrower that overran its declared
    /// duration: the borrower's lease starts cleaning (cause `Reclaimed`),
    /// and when it ends the unit returns to the paused holder.
    ///
    /// # Errors
    /// [`SchedulerError::NotFound`]; [`SchedulerError::NotHolder`];
    /// [`SchedulerError::BadState`] unless `id` is paused with a borrower
    /// that is offered or held; [`SchedulerError::BorrowerNotOverrun`]
    /// before the borrower's declared duration has passed.
    pub fn reclaim(&self, tx: &Tx, who: &Caller, id: &LeaseId) -> Result<(), SchedulerError> {
        let now = self.now();
        let paused = Self::own_lease(tx, who, id)?;
        if paused.state != LeaseState::Paused {
            return Err(SchedulerError::BadState {
                state: paused.state,
            });
        }
        let borrower = match paused.borrower_lease {
            Some(row) => leases::lease_by_row(tx, row)?,
            None => None,
        }
        .ok_or(SchedulerError::BadState {
            state: paused.state,
        })?;
        match borrower.state {
            LeaseState::Offered => {
                return Err(SchedulerError::BorrowerNotOverrun { due_at: None });
            }
            LeaseState::Held => {}
            other => return Err(SchedulerError::BadState { state: other }),
        }
        let due_at = borrower.borrow_due_at;
        if due_at.is_none_or(|d| now < d) {
            return Err(SchedulerError::BorrowerNotOverrun { due_at });
        }
        self.start_reclaim(tx, who, &paused, &borrower, None)
    }

    /// Put `borrower` (held) into cleaning with cause `Reclaimed` for the
    /// holder of `paused`, and audit it with the override `reason`, if the
    /// reclaim came from one. A borrower already cleaning is left as it is:
    /// the unit comes back when that cleanup ends.
    pub(super) fn start_reclaim(
        &self,
        tx: &Tx,
        who: &Caller,
        paused: &LeaseRow,
        borrower: &LeaseRow,
        reason: Option<&str>,
    ) -> Result<(), SchedulerError> {
        let now = self.now();
        if borrower.state == LeaseState::Held {
            let window = millis(Duration::from_secs(self.config.cleanup_window));
            self.start_cleanup(
                tx,
                borrower,
                EndCause::Reclaimed,
                now.saturating_add(window),
                format!(
                    "Lease {} is reclaimed by the holder you borrowed from; stop, clean up and \
                     confirm before the deadline.",
                    borrower.id
                ),
            )?;
        }
        audit::record(
            tx,
            now,
            who,
            "reclaimed",
            &json!({
                "lease": paused.id,
                "borrower_lease": borrower.id,
                "reason": reason,
            }),
        )?;
        Ok(())
    }

    /// The caller's paused lease that `target` borrows from, if any.
    pub(super) fn own_borrowers_paused(
        tx: &Tx,
        who: &Caller,
        target: &LeaseRow,
    ) -> Result<Option<LeaseRow>, SchedulerError> {
        let paused = match target.paused_for_lease {
            Some(row) => leases::lease_by_row(tx, row)?,
            None => None,
        };
        Ok(paused.filter(|p| p.state == LeaseState::Paused && p.holder == who.holder))
    }

    /// Remove `request` from the queue. A holder being asked to yield for it
    /// is told the ask is withdrawn. Every scheduler path that takes a
    /// request out of the queue goes through here.
    pub(super) fn remove_request(
        &self,
        tx: &Tx,
        request: &RequestRow,
    ) -> Result<(), SchedulerError> {
        if let Some(lease) = leases::asked_for(tx, &request.id)? {
            self.notify_withdrawn(tx, &lease, request)?;
        }
        leases::delete_request(tx, request.row)?;
        Ok(())
    }

    /// Tell the holder of `lease` that the yield ask for `request` is
    /// withdrawn.
    fn notify_withdrawn(
        &self,
        tx: &Tx,
        lease: &LeaseRow,
        request: &RequestRow,
    ) -> Result<(), SchedulerError> {
        let notice = self.lease_notice(
            tx,
            lease,
            NoticeKind::YieldWithdrawn,
            format!(
                "The request to yield lease {} was withdrawn; carry on, you need not yield.",
                lease.id
            ),
        )?;
        let scope = inventory_sql::load_resource(tx, request.resource_row)?.scope;
        notify(
            tx,
            notice,
            lease.project.as_ref(),
            Some(OtherParty {
                scope: &scope,
                purpose: &request.purpose,
            }),
        )?;
        Ok(())
    }

    /// The lease an override by `who` on `units` displaces: the held or
    /// cleaning lease on their identity groups that started earliest,
    /// whether or not it is interruptible. A cleaning lease is already on its
    /// way out, so the override just waits for it. Paused leases are never
    /// displaced (their borrower may be), nor the caller's own.
    pub(super) fn override_target(
        tx: &Tx,
        who: &Caller,
        units: &[&Unit],
    ) -> Result<Option<LeaseRow>, SchedulerError> {
        let mut members: Vec<i64> = Vec::new();
        for unit in units {
            members.extend(group(tx, unit.row)?);
        }
        members.sort_unstable();
        members.dedup();
        Ok(leases::live_leases_on(tx, &members)?
            .into_iter()
            .filter(|l| {
                matches!(l.state, LeaseState::Held | LeaseState::Cleaning) && l.holder != who.holder
            })
            .min_by_key(|l| (l.created_at, l.row)))
    }

    /// Override `lease` for queued request `request`. A held lease starts
    /// cleaning (cause `Override`, the normal cleanup window) and its holder
    /// is told; a lease already cleaning is left as it is. Either way the
    /// override is audited with its reason and the request is served first
    /// when the lease ends. The lease is ended, never paused; a pending
    /// yield ask on it moves on.
    pub(super) fn apply_override(
        &self,
        tx: &Tx,
        who: &Caller,
        lease: &LeaseRow,
        request: &RequestId,
        reason: &str,
        other: OtherParty<'_>,
    ) -> Result<(), SchedulerError> {
        let now = self.now();
        let (resource, unit): (_, Option<UnitId>) = Self::names(tx, lease)?;
        let already_cleaning = lease.state == LeaseState::Cleaning;
        audit::record(
            tx,
            now,
            who,
            "override",
            &json!({
                "lease": lease.id,
                "holder": lease.holder,
                "request": request,
                "reason": reason,
                "resource": resource,
                "unit": unit,
                "already_cleaning": already_cleaning,
            }),
        )?;
        if already_cleaning {
            return Ok(());
        }
        let deadline = now.saturating_add(millis(Duration::from_secs(self.config.cleanup_window)));
        leases::set_cleaning(tx, lease.row, EndCause::Override, deadline)?;
        let mut notice = self.lease_notice(
            tx,
            lease,
            NoticeKind::Overridden,
            format!(
                "Lease {} was overridden at the user's request, so it ends; stop, clean up and \
                 confirm before the deadline, then acquire again if you still need the unit.",
                lease.id
            ),
        )?;
        notice.deadline = Some(deadline);
        notify(tx, notice, lease.project.as_ref(), Some(other))?;
        if lease.yield_deadline.is_some() {
            self.withdraw_ask(tx, lease)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::model::{HolderRef, Priority};

    fn request(priority: Priority, duration_ms: i64) -> RequestRow {
        RequestRow {
            row: 1,
            id: RequestId::new(),
            resource_row: 1,
            unit_row: None,
            holder: HolderRef::User { tag: "r".into() },
            project: Some(crate::model::ProjectKey(PathBuf::from("/p"))),
            priority,
            interruptible: false,
            purpose: String::new(),
            duration_ms,
            created_at: 0,
            override_reason: None,
            override_lease: None,
        }
    }

    fn lease(priority: Priority) -> LeaseRow {
        LeaseRow {
            row: 1,
            id: LeaseId::new(),
            unit_row: 1,
            resource_row: 1,
            state: LeaseState::Held,
            holder: HolderRef::User { tag: "l".into() },
            project: None,
            priority,
            interruptible: true,
            purpose: String::new(),
            duration_ms: 0,
            created_at: 0,
            ends_at: None,
            claim_deadline: None,
            cleanup_deadline: None,
            override_reason: None,
            dirty: None,
            request_id: None,
            warning_sent: false,
            cleanup_cause: None,
            extension_used: false,
            yield_request: None,
            yield_asked_at: None,
            yield_deadline: None,
            paused_remaining_ms: None,
            borrower_lease: None,
            paused_for_lease: None,
            borrow_due_at: None,
            overran_sent: false,
            cleanup_confirmed: None,
            end_cause: None,
            cleanup_ending_sent: false,
        }
    }

    #[test]
    fn higher_priority_interrupts_whatever_the_duration() {
        assert!(may_interrupt(
            &request(Priority::High, 100),
            &lease(Priority::Normal),
            1
        ));
    }

    #[test]
    fn equal_priority_interrupts_only_a_longer_remainder() {
        let r = request(Priority::Normal, 100);
        assert!(may_interrupt(&r, &lease(Priority::Normal), 101));
        assert!(!may_interrupt(&r, &lease(Priority::Normal), 100));
    }

    #[test]
    fn lower_priority_never_interrupts() {
        assert!(!may_interrupt(
            &request(Priority::Low, 1),
            &lease(Priority::Normal),
            1_000
        ));
    }
}
