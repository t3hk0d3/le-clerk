//! Lease lifecycle: expiry warnings, extension, release, the cleanup
//! handshake, and ending leases (dirty when cleanup was not confirmed).
//!
//! Every deadline is stored on the lease row as an absolute time. `tick`
//! rebuilds the timers from the rows each time, so a daemon that restarts
//! long after deadlines passed catches up in one call, in deadline order.

use std::collections::HashSet;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::model::{
    Caller, DirtyInfo, DirtyReason, EndCause, LeaseId, LeaseState, ResourceId, UnitId,
};
use crate::scheduler::dispatch::{HealthCache, QueueSnapshot};
use crate::scheduler::{
    Lease, Scheduler, SchedulerError, can_see, clerk_actor, group, millis, notify,
};
use crate::store::inventory as inventory_sql;
use crate::store::leases::{self, LeaseRow};
use crate::store::notices::{Notice, NoticeKind};
use crate::store::{StoreError, Tx, audit};

/// How a cleaning lease's holder finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confirm {
    /// Cleanup is done.
    Clean,
    /// The holder paused its work and yields the unit it was asked for
    /// (see `interrupt`).
    Yielded,
}

/// The kinds of deadline `tick` processes. The declaration order is the
/// processing order for deadlines at the same instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Timer {
    /// An offer's claim window passed.
    Claim,
    /// A held lease nears its end.
    Warning,
    /// A held lease reached its end.
    Expiry,
    /// A held lease's holder did not answer a yield request in time.
    Yield,
    /// A borrower ran past its declared duration.
    Overrun,
    /// A cleaning lease's window is about to pass.
    CleanupEnding,
    /// A cleaning lease's window passed.
    Cleanup,
}

/// Longest lead of the cleanup-ending notice.
const CLEANUP_ENDING_LEAD_MAX: Duration = Duration::from_secs(30);

/// The dirty reason recorded when a lease ends with `cause` and cleanup was
/// not confirmed. A cleanup that started at expiry reads `expired`; any
/// other cleanup whose window passed reads `window_passed`.
fn dirty_reason(cause: EndCause) -> DirtyReason {
    match cause {
        EndCause::Expired => DirtyReason::Expired,
        EndCause::HolderGone => DirtyReason::HolderGone,
        EndCause::Override => DirtyReason::Override,
        EndCause::CommandFailed => DirtyReason::CommandFailed,
        EndCause::AdminForced => DirtyReason::AdminForced,
        EndCause::Released | EndCause::Yielded | EndCause::Reclaimed | EndCause::Unclaimed => {
            DirtyReason::WindowPassed
        }
    }
}

impl Scheduler {
    /// How long before a lease of `duration_ms` ends its expiry warning goes
    /// out: `min(warning_lead_max, warning_lead_ratio × duration)`.
    // Durations are far below 2^52 ms, so the f64 round trip is exact enough
    // and the rounded product fits an i64.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn warning_lead(&self, duration_ms: i64) -> i64 {
        let cap = millis(Duration::from_secs(self.config.warning_lead_max));
        let share = (duration_ms as f64 * self.config.warning_lead_ratio).round() as i64;
        cap.min(share.max(0))
    }

    /// How long before a cleanup window ends the `CleanupEnding` notice goes
    /// out: `min(30 s, cleanup_window / 2)`.
    fn cleanup_ending_lead(&self) -> i64 {
        let half = millis(Duration::from_secs(self.config.cleanup_window)) / 2;
        millis(CLEANUP_ENDING_LEAD_MAX).min(half)
    }

    /// The deadlines of `lease` as `(when it becomes due, kind)`. An offer's
    /// claim deadline is inclusive (it can still be claimed at that instant),
    /// so its timer is due one millisecond later.
    fn timers(&self, lease: &LeaseRow) -> Vec<(i64, Timer)> {
        match lease.state {
            LeaseState::Offered => lease
                .claim_deadline
                .map(|c| (c.saturating_add(1), Timer::Claim))
                .into_iter()
                .collect(),
            LeaseState::Held => {
                let Some(ends_at) = lease.ends_at else {
                    return Vec::new();
                };
                let mut out = Vec::with_capacity(2);
                if !lease.warning_sent {
                    out.push((
                        ends_at - self.warning_lead(lease.duration_ms),
                        Timer::Warning,
                    ));
                }
                out.push((ends_at, Timer::Expiry));
                if let Some(deadline) = lease.yield_deadline {
                    out.push((deadline, Timer::Yield));
                }
                if let (Some(due), Some(_), false) = (
                    lease.borrow_due_at,
                    lease.paused_for_lease,
                    lease.overran_sent,
                ) {
                    out.push((due, Timer::Overrun));
                }
                out
            }
            // A cleaning lease without a deadline would never end: end it
            // at once rather than leave its unit stuck.
            LeaseState::Cleaning => {
                let deadline = lease.cleanup_deadline.unwrap_or(i64::MIN);
                let mut out = Vec::with_capacity(2);
                if !lease.cleanup_ending_sent && lease.cleanup_deadline.is_some() {
                    out.push((
                        deadline.saturating_sub(self.cleanup_ending_lead()),
                        Timer::CleanupEnding,
                    ));
                }
                out.push((deadline, Timer::Cleanup));
                out
            }
            // A paused lease's clock is stopped: no timers.
            LeaseState::Paused | LeaseState::Ended => Vec::new(),
        }
    }

    /// Process every deadline due at the clock's current time, earliest
    /// first (claim, warning, expiry, yield, overrun, cleanup ending,
    /// cleanup for equal times, then by lease row), including deadlines that processing creates (an expiry's
    /// cleanup window may itself be overdue). Returns the next pending
    /// deadline, if any; the daemon sleeps until then. Idempotent: a second
    /// call at the same time does nothing.
    ///
    /// # Errors
    /// [`SchedulerError::Store`] on storage failure.
    pub fn tick(&self, tx: &Tx) -> Result<Option<i64>, SchedulerError> {
        let now = self.now();
        // Each processed timer changes its lease, so it is not due again.
        // The set catches a timer that is still due after processing: a bug,
        // asserted in debug builds and logged (then skipped, so the loop
        // ends) in release builds.
        let mut done: HashSet<(Timer, i64)> = HashSet::new();
        loop {
            let mut next: Option<i64> = None;
            let mut due: Option<(i64, Timer, LeaseRow)> = None;
            for lease in leases::timed_leases(tx)? {
                for (at, timer) in self.timers(&lease) {
                    if at > now {
                        next = Some(next.map_or(at, |n| n.min(at)));
                        continue;
                    }
                    let refire = done.contains(&(timer, lease.row));
                    debug_assert!(
                        !refire,
                        "{timer:?} timer of lease {} still due after processing",
                        lease.id
                    );
                    if refire {
                        tracing::error!(
                            lease = %lease.id,
                            ?timer,
                            "timer still due after it was processed; skipping it"
                        );
                        continue;
                    }
                    let earlier = due
                        .as_ref()
                        .is_none_or(|(a, t, l)| (at, timer, lease.row) < (*a, *t, l.row));
                    if earlier {
                        due = Some((at, timer, lease.clone()));
                    }
                }
            }
            let Some((at, timer, lease)) = due else {
                return Ok(next);
            };
            done.insert((timer, lease.row));
            match timer {
                Timer::Claim => self.dispatch_with(tx, &mut HealthCache::new(), at)?,
                Timer::Warning => self.warn(tx, &lease)?,
                Timer::Expiry => self.expire(tx, &lease)?,
                Timer::Yield => self.yield_timed_out(tx, &lease)?,
                Timer::Overrun => self.borrower_overran(tx, &lease)?,
                Timer::CleanupEnding => self.cleanup_ending(tx, &lease)?,
                Timer::Cleanup => {
                    let cause = lease.cleanup_cause.unwrap_or(EndCause::Released);
                    self.finish(tx, &lease, cause, false, at)?;
                }
            }
        }
    }

    /// The restart grace (spec §7.2), run once at daemon startup before the
    /// first tick: a cleaning lease whose cleanup deadline passed while the
    /// daemon was down gets a fresh cleanup window from now, and an offer
    /// whose claim deadline passed gets a fresh claim window. Each change is
    /// audited as `restart_grace`, and the holder is told the new deadline
    /// again (`CleanupWindowStarted` or `Offered`). Returns how many leases
    /// were adjusted.
    ///
    /// Held leases past their end are left alone: the first tick expires
    /// them with a full cleanup window from now (see `expire`). The point is
    /// that a holder whose confirm or claim is what restarted the daemon is
    /// served before its lease is ended under it.
    ///
    /// # Errors
    /// [`SchedulerError::Store`] on storage failure.
    pub fn restart_grace(&self, tx: &Tx) -> Result<usize, SchedulerError> {
        let now = self.now();
        let cleanup = millis(Duration::from_secs(self.config.cleanup_window));
        let claim = millis(Duration::from_secs(self.config.claim_window));
        let mut adjusted = 0;
        for lease in leases::timed_leases(tx)? {
            let detail = match (lease.state, lease.cleanup_deadline, lease.claim_deadline) {
                (LeaseState::Cleaning, Some(passed), _) if passed < now => {
                    let deadline = now.saturating_add(cleanup);
                    leases::set_cleanup_deadline(tx, lease.row, deadline)?;
                    let mut notice = self.lease_notice(
                        tx,
                        &lease,
                        NoticeKind::CleanupWindowStarted,
                        format!(
                            "The clerk restarted after lease {}'s cleanup window passed; you \
                             have a fresh window: clean up and confirm before the new deadline, \
                             or the unit is marked dirty.",
                            lease.id
                        ),
                    )?;
                    notice.deadline = Some(deadline);
                    notify(tx, notice, lease.project.as_ref(), None)?;
                    json!({
                        "lease": lease.id,
                        "holder": lease.holder,
                        "was": passed,
                        "cleanup_deadline": deadline,
                    })
                }
                (LeaseState::Offered, _, Some(passed)) if passed < now => {
                    let deadline = now.saturating_add(claim);
                    leases::set_claim_deadline(tx, lease.row, deadline)?;
                    let mut notice = self.lease_notice(
                        tx,
                        &lease,
                        NoticeKind::Offered,
                        format!(
                            "The clerk restarted after the claim window of lease {} passed; the \
                             unit is still yours to claim before the new deadline, or it goes \
                             to the next in line.",
                            lease.id
                        ),
                    )?;
                    notice.deadline = Some(deadline);
                    notice.dirty.clone_from(&lease.dirty);
                    notify(tx, notice, lease.project.as_ref(), None)?;
                    json!({
                        "lease": lease.id,
                        "holder": lease.holder,
                        "was": passed,
                        "claim_deadline": deadline,
                    })
                }
                _ => continue,
            };
            audit::record(tx, now, &clerk_actor(), "restart_grace", &detail)?;
            adjusted += 1;
        }
        Ok(adjusted)
    }

    /// The resource and unit ids of `lease`, for notices and audit.
    pub(super) fn names(
        tx: &Tx,
        lease: &LeaseRow,
    ) -> Result<(ResourceId, Option<UnitId>), SchedulerError> {
        let resource = inventory_sql::load_resource(tx, lease.resource_row)?;
        let unit = resource
            .units
            .iter()
            .find(|u| u.row == lease.unit_row)
            .map(|u| u.id.clone());
        Ok((resource.id, unit))
    }

    /// A notice to the holder of `lease` about it.
    pub(super) fn lease_notice(
        &self,
        tx: &Tx,
        lease: &LeaseRow,
        kind: NoticeKind,
        action: String,
    ) -> Result<Notice, SchedulerError> {
        let (resource, unit) = Self::names(tx, lease)?;
        let mut notice = Notice::new(lease.holder.clone(), kind, action, self.now());
        notice.lease_id = Some(lease.id.clone());
        notice.request_id.clone_from(&lease.request_id);
        notice.resource = Some(resource);
        notice.unit = unit;
        Ok(notice)
    }

    /// Send the expiry warning of `lease`, once. A lease already past its
    /// end (a backlog after downtime) is about to expire in the same tick,
    /// so the stale warning is skipped.
    fn warn(&self, tx: &Tx, lease: &LeaseRow) -> Result<(), SchedulerError> {
        leases::set_warning_sent(tx, lease.row)?;
        let Some(ends_at) = lease.ends_at else {
            return Ok(());
        };
        if ends_at <= self.now() {
            return Ok(());
        }
        let mut notice = self.lease_notice(
            tx,
            lease,
            NoticeKind::ExpiryWarning,
            format!(
                "Lease {} expires soon; extend it if you need more time, or wrap up and release \
                 it.",
                lease.id
            ),
        )?;
        notice.deadline = Some(ends_at);
        notify(tx, notice, lease.project.as_ref(), None)?;
        Ok(())
    }

    /// Tell the holder of a cleaning lease that its window ends soon, once
    /// per deadline. A window that already passed (a backlog after
    /// downtime) ends in the same tick, so the stale notice is skipped.
    fn cleanup_ending(&self, tx: &Tx, lease: &LeaseRow) -> Result<(), SchedulerError> {
        leases::set_cleanup_ending_sent(tx, lease.row)?;
        let Some(deadline) = lease.cleanup_deadline else {
            return Ok(());
        };
        if deadline <= self.now() {
            return Ok(());
        }
        let mut notice = self.lease_notice(
            tx,
            lease,
            NoticeKind::CleanupEnding,
            format!(
                "The cleanup window of lease {} ends soon; finish cleaning and confirm, or ask \
                 for the one cleanup extension, or the unit is marked dirty.",
                lease.id
            ),
        )?;
        notice.deadline = Some(deadline);
        notify(tx, notice, lease.project.as_ref(), None)?;
        Ok(())
    }

    /// Start the cleanup phase of `lease` for `cause`, ending at `deadline`,
    /// and tell the holder.
    pub(super) fn start_cleanup(
        &self,
        tx: &Tx,
        lease: &LeaseRow,
        cause: EndCause,
        deadline: i64,
        action: String,
    ) -> Result<(), SchedulerError> {
        leases::set_cleaning(tx, lease.row, cause, deadline)?;
        let mut notice = self.lease_notice(tx, lease, NoticeKind::CleanupWindowStarted, action)?;
        notice.deadline = Some(deadline);
        notify(tx, notice, lease.project.as_ref(), None)?;
        Ok(())
    }

    /// Expire a held lease: it starts cleaning. The window runs from
    /// `max(ends_at, now)`: in normal running that is the lease's end, and
    /// after downtime the holder still gets a full window from now (§5.4).
    fn expire(&self, tx: &Tx, lease: &LeaseRow) -> Result<(), SchedulerError> {
        let now = self.now();
        let start = lease.ends_at.map_or(now, |e| e.max(now));
        let window = millis(Duration::from_secs(self.config.cleanup_window));
        self.start_cleanup(
            tx,
            lease,
            EndCause::Expired,
            start.saturating_add(window),
            format!(
                "Lease {} expired; stop using the unit, clean up and confirm before the \
                 deadline, or it is marked dirty.",
                lease.id
            ),
        )?;
        audit::record(
            tx,
            self.now(),
            &clerk_actor(),
            "expired",
            &json!({ "lease": lease.id, "holder": lease.holder }),
        )?;
        Ok(())
    }

    /// Load a lease the caller holds.
    pub(super) fn own_lease(
        tx: &Tx,
        who: &Caller,
        id: &LeaseId,
    ) -> Result<LeaseRow, SchedulerError> {
        let lease = leases::find_lease(tx, id)?.ok_or(SchedulerError::NotFound)?;
        if lease.holder != who.holder {
            return Err(SchedulerError::NotHolder);
        }
        Ok(lease)
    }

    /// Re-read a lease for its holder.
    pub(super) fn view(tx: &Tx, id: &LeaseId) -> Result<Lease, SchedulerError> {
        let lease = leases::find_lease(tx, id)?.ok_or(SchedulerError::NotFound)?;
        Self::lease_view(tx, lease)
    }

    /// Move a held lease's end to `new_ends_at`. The expiry warning is
    /// re-armed, with its lead computed from the new total duration.
    ///
    /// # Errors
    /// [`SchedulerError::NotFound`]; [`SchedulerError::NotHolder`];
    /// [`SchedulerError::BadState`] unless held (a lease already past its
    /// end reports `Cleaning`: it is treated as expired, as in `release`);
    /// [`SchedulerError::ExtendNotLater`] unless `new_ends_at` is after the
    /// current end; [`SchedulerError::ExtendRefused`] when the lease is not
    /// interruptible and a request of strictly higher priority waits for its
    /// unit's identity group.
    pub fn extend(
        &self,
        tx: &Tx,
        who: &Caller,
        id: &LeaseId,
        new_ends_at: i64,
    ) -> Result<Lease, SchedulerError> {
        let now = self.now();
        let lease = Self::own_lease(tx, who, id)?;
        if lease.state != LeaseState::Held {
            return Err(SchedulerError::BadState { state: lease.state });
        }
        let ends_at = lease.ends_at.ok_or(StoreError::Corrupt("leases.ends_at"))?;
        if now >= ends_at {
            return Err(SchedulerError::BadState {
                state: LeaseState::Cleaning,
            });
        }
        if new_ends_at <= ends_at {
            return Err(SchedulerError::ExtendNotLater { ends_at });
        }
        if !lease.interruptible {
            let members = group(tx, lease.unit_row)?;
            let queue = QueueSnapshot::load(tx)?;
            if let Some(waiter) = queue.first_above(lease.priority, &members) {
                let scope = inventory_sql::load_resource(tx, waiter.resource_row)?.scope;
                return Err(SchedulerError::ExtendRefused {
                    waiting_purpose: can_see(who.project.as_ref(), &scope)
                        .then(|| waiter.purpose.clone()),
                });
            }
        }
        let duration_ms = lease.duration_ms.saturating_add(new_ends_at - ends_at);
        leases::set_extended(tx, lease.row, new_ends_at, duration_ms)?;
        audit::record(
            tx,
            now,
            who,
            "extended",
            &json!({ "lease": id, "from": ends_at, "ends_at": new_ends_at }),
        )?;
        Self::view(tx, id)
    }

    /// Release a held lease: it starts cleaning, and the holder has the
    /// cleanup window to confirm. Releasing a paused lease forfeits the
    /// holder's claim to get its unit back: the lease ends at once
    /// (confirmed, as the holder has nothing on the unit) and the borrower
    /// keeps the unit.
    ///
    /// A held lease already past its end that no tick has expired yet is
    /// treated as expired: the expiry runs now (cause `Expired`) and the
    /// release succeeds without further change, returning the cleaning
    /// lease. (`extend` on such a lease returns `BadState`.)
    ///
    /// Releasing an offered lease declines the offer: it ends as
    /// `Unclaimed` at once and the unit goes to the next in line.
    ///
    /// # Errors
    /// [`SchedulerError::NotFound`]; [`SchedulerError::NotHolder`];
    /// [`SchedulerError::BadState`] unless held, paused or offered.
    pub fn release(&self, tx: &Tx, who: &Caller, id: &LeaseId) -> Result<Lease, SchedulerError> {
        let now = self.now();
        let lease = Self::own_lease(tx, who, id)?;
        if lease.state == LeaseState::Offered {
            return self.decline_offer(tx, who, &lease);
        }
        if lease.state == LeaseState::Paused {
            audit::record(
                tx,
                now,
                who,
                "released",
                &json!({ "lease": id, "paused": true }),
            )?;
            self.finish(tx, &lease, EndCause::Released, true, now)?;
            return Self::view(tx, id);
        }
        if lease.state != LeaseState::Held {
            return Err(SchedulerError::BadState { state: lease.state });
        }
        if lease.ends_at.is_some_and(|e| now >= e) {
            self.expire(tx, &lease)?;
            return Self::view(tx, id);
        }
        let window = millis(Duration::from_secs(self.config.cleanup_window));
        self.start_cleanup(
            tx,
            &lease,
            EndCause::Released,
            now.saturating_add(window),
            format!(
                "Lease {id} is released; clean up and confirm before the deadline, or the unit \
                 is marked dirty."
            ),
        )?;
        audit::record(tx, now, who, "released", &json!({ "lease": id }))?;
        Self::view(tx, id)
    }

    /// Release a lease because the command that used it failed: it ends at
    /// once with cause `CommandFailed`, cleanup unconfirmed, so its unit is
    /// marked dirty (`command_failed`). An offered lease is declined as in
    /// [`Scheduler::release`]; a paused one ends as there (its holder has
    /// nothing on the unit).
    ///
    /// # Errors
    /// [`SchedulerError::NotFound`]; [`SchedulerError::NotHolder`];
    /// [`SchedulerError::BadState`] for an ended lease.
    pub fn release_failed(
        &self,
        tx: &Tx,
        who: &Caller,
        id: &LeaseId,
    ) -> Result<Lease, SchedulerError> {
        let lease = Self::own_lease(tx, who, id)?;
        match lease.state {
            LeaseState::Ended => Err(SchedulerError::BadState { state: lease.state }),
            LeaseState::Offered => self.decline_offer(tx, who, &lease),
            _ => {
                audit::record(
                    tx,
                    self.now(),
                    who,
                    "released",
                    &json!({ "lease": id, "failed": true }),
                )?;
                self.finish(tx, &lease, EndCause::CommandFailed, false, self.now())?;
                Self::view(tx, id)
            }
        }
    }

    /// End an offered lease the caller declines as `Unclaimed`, and offer
    /// the unit on.
    fn decline_offer(
        &self,
        tx: &Tx,
        who: &Caller,
        lease: &LeaseRow,
    ) -> Result<Lease, SchedulerError> {
        let now = self.now();
        audit::record(
            tx,
            now,
            who,
            "offer_declined",
            &json!({ "lease": lease.id }),
        )?;
        self.finish(tx, lease, EndCause::Unclaimed, true, now)?;
        Self::view(tx, &lease.id)
    }

    /// A cleaning lease the caller holds whose window is still open.
    fn open_cleanup(
        tx: &Tx,
        who: &Caller,
        id: &LeaseId,
        now: i64,
    ) -> Result<LeaseRow, SchedulerError> {
        let lease = Self::own_lease(tx, who, id)?;
        if lease.state != LeaseState::Cleaning {
            return Err(SchedulerError::BadState { state: lease.state });
        }
        if lease.cleanup_deadline.is_none_or(|d| now >= d) {
            return Err(SchedulerError::BadState {
                state: LeaseState::Ended,
            });
        }
        Ok(lease)
    }

    /// Confirm that cleanup is done: the lease ends with its cleanup cause
    /// and the unit stays clean. [`Confirm::Yielded`] instead answers a yield
    /// request on a held lease: the lease is paused and its unit lent to the
    /// requester (see `interrupt`).
    ///
    /// # Errors
    /// [`SchedulerError::NotFound`]; [`SchedulerError::NotHolder`];
    /// [`SchedulerError::BadState`] for `Clean` unless cleaning with the
    /// window open (a passed window reports `Ended`: it ends at the next
    /// tick), and for `Yielded` unless held with a yield request whose
    /// window is open.
    pub fn confirm(
        &self,
        tx: &Tx,
        who: &Caller,
        id: &LeaseId,
        how: Confirm,
    ) -> Result<(), SchedulerError> {
        if how == Confirm::Yielded {
            return self.confirm_yielded(tx, who, id);
        }
        let now = self.now();
        let lease = Self::open_cleanup(tx, who, id, now)?;
        audit::record(tx, now, who, "cleanup_confirmed", &json!({ "lease": id }))?;
        let cause = lease.cleanup_cause.unwrap_or(EndCause::Released);
        self.finish(tx, &lease, cause, true, now)
    }

    /// Add `cleanup_extension` to a cleaning lease's window, once.
    ///
    /// # Errors
    /// [`SchedulerError::NotFound`]; [`SchedulerError::NotHolder`];
    /// [`SchedulerError::BadState`] unless cleaning with the window open;
    /// [`SchedulerError::ExtensionUsed`] on a second request.
    pub fn request_cleanup_extension(
        &self,
        tx: &Tx,
        who: &Caller,
        id: &LeaseId,
        reason: &str,
    ) -> Result<Lease, SchedulerError> {
        let now = self.now();
        let lease = Self::open_cleanup(tx, who, id, now)?;
        if lease.extension_used {
            return Err(SchedulerError::ExtensionUsed);
        }
        let extra = millis(Duration::from_secs(self.config.cleanup_extension));
        // Invariant: open_cleanup checked the deadline is set.
        let deadline = lease.cleanup_deadline.unwrap_or(now).saturating_add(extra);
        leases::set_cleanup_extended(tx, lease.row, deadline)?;
        audit::record(
            tx,
            now,
            who,
            "cleanup_extended",
            &json!({ "lease": id, "reason": reason, "deadline": deadline }),
        )?;
        Self::view(tx, id)
    }

    /// End a lease with `cause`. This is the one path that ends a lease in
    /// use: when cleanup is not `confirmed` the unit is marked dirty (reason
    /// from the cause; for `AdminForced`, every other unit of its identity
    /// group without a live lease or dirty record too, since the forced end
    /// gave no cleanup window); the holder is told; the end is audited with whether
    /// cleanup was confirmed; then the queue is dispatched. An offer that
    /// ends was never used, so it only gives back the dirty record it
    /// carried.
    ///
    /// Ending a borrower's lease gives the unit back to the paused holder
    /// (in the dispatch). Ending a paused lease forfeits its holder's claim
    /// to the unit: the borrower keeps it as an ordinary lease, the unit is
    /// not marked dirty, and it follows the normal queue afterwards. That is
    /// the hook for a paused holder that went away (`HolderGone`).
    ///
    /// # Errors
    /// [`SchedulerError::NotFound`]; [`SchedulerError::BadState`] when the
    /// lease already ended.
    pub fn end_lease(
        &self,
        tx: &Tx,
        id: &LeaseId,
        cause: EndCause,
        confirmed: bool,
    ) -> Result<(), SchedulerError> {
        let lease = leases::find_lease(tx, id)?.ok_or(SchedulerError::NotFound)?;
        if lease.state == LeaseState::Ended {
            return Err(SchedulerError::BadState { state: lease.state });
        }
        self.finish(tx, &lease, cause, confirmed, self.now())
    }

    /// [`Scheduler::end_lease`] with the dispatch horizon of the deadline
    /// being processed.
    fn finish(
        &self,
        tx: &Tx,
        lease: &LeaseRow,
        cause: EndCause,
        confirmed: bool,
        horizon: i64,
    ) -> Result<(), SchedulerError> {
        let now = self.now();
        // An offer was never used and a paused holder has nothing on the
        // unit: neither has anything to clean.
        let unused = matches!(lease.state, LeaseState::Offered | LeaseState::Paused);
        leases::set_ended(tx, lease.row, cause, confirmed || unused)?;
        if lease.state == LeaseState::Paused
            && let Some(borrower) = lease.borrower_lease
        {
            leases::clear_paused_for(tx, borrower)?;
        }
        let unit = inventory_sql::unit_by_row(tx, lease.unit_row)?;
        let mut marked: Option<DirtyInfo> = None;
        if let Some(unit) = &unit {
            if lease.state == LeaseState::Paused {
                // The unit is with the borrower.
            } else if lease.state == LeaseState::Offered {
                if let (Some(carried), None) = (&lease.dirty, &unit.dirty) {
                    inventory_sql::set_dirty(tx, unit.row, Some(carried))?;
                }
            } else if !confirmed {
                let dirty = DirtyInfo {
                    reason: dirty_reason(cause),
                    at: now,
                    previous_holder: lease.holder.clone(),
                };
                inventory_sql::set_dirty(tx, unit.row, Some(&dirty))?;
                if cause == EndCause::AdminForced {
                    // Ended with no cleanup window: the holder may still be
                    // on the physical thing, which every linked unit is.
                    // Warn whoever gets it next through any of them.
                    // A partner already dirty keeps its earlier record
                    // (reason and previous holder).
                    for row in group(tx, unit.row)? {
                        if row == unit.row || !leases::live_leases_on(tx, &[row])?.is_empty() {
                            continue;
                        }
                        if inventory_sql::unit_by_row(tx, row)?.is_some_and(|u| u.dirty.is_none()) {
                            inventory_sql::set_dirty(tx, row, Some(&dirty))?;
                        }
                    }
                }
                marked = Some(dirty);
            }
        }
        let action = if lease.state == LeaseState::Paused {
            format!(
                "Lease {} ended while paused, so your claim to get the unit back is forfeited; \
                 acquire again if you still need it.",
                lease.id
            )
        } else if marked.is_some() {
            format!(
                "Lease {} ended without confirmed cleanup, so the unit is marked dirty; stop \
                 using it now.",
                lease.id
            )
        } else {
            format!("Lease {} ended; nothing more to do.", lease.id)
        };
        let mut notice = self.lease_notice(tx, lease, NoticeKind::LeaseEnded, action)?;
        notice.dirty.clone_from(&marked);
        notify(tx, notice, lease.project.as_ref(), None)?;
        audit::record(
            tx,
            now,
            &clerk_actor(),
            "lease_ended",
            &json!({
                "lease": lease.id,
                "holder": lease.holder,
                "cause": cause,
                "cleanup_confirmed": confirmed,
                "dirty": marked,
            }),
        )?;
        self.dispatch_with(tx, &mut HealthCache::new(), horizon)?;
        // A held lease that ended while asked to yield: its requester, if
        // still waiting, moves on to the next holder.
        if lease.state == LeaseState::Held
            && let Some(request) = &lease.yield_request
        {
            self.maybe_request_yield(tx, request)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::clock::ManualClock;
    use crate::config::Config;
    use crate::health::HealthChecker;

    fn scheduler() -> Scheduler {
        let clock = Arc::new(ManualClock::new(0));
        Scheduler::new(
            clock,
            Config::default(),
            Arc::new(HealthChecker::new(Duration::from_secs(1))),
        )
    }

    #[test]
    fn warning_lead_is_a_fifth_of_a_short_lease() {
        assert_eq!(scheduler().warning_lead(600_000), 120_000);
    }

    #[test]
    fn warning_lead_is_capped_at_five_minutes() {
        assert_eq!(scheduler().warning_lead(7_200_000), 300_000);
    }

    #[test]
    fn expiry_cleanup_reads_expired_and_others_window_passed() {
        assert_eq!(dirty_reason(EndCause::Expired), DirtyReason::Expired);
        assert_eq!(dirty_reason(EndCause::Released), DirtyReason::WindowPassed);
        assert_eq!(dirty_reason(EndCause::HolderGone), DirtyReason::HolderGone);
    }
}
