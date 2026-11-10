//! Holder liveness: ending everything a holder that went away still has.
//!
//! A `CliProcess` holder is identified by pid and start time (field 22 of
//! `/proc/<pid>/stat`), so a pid the kernel reused for another process does
//! not keep a dead holder's leases alive. [`probe_all`] checks every such
//! holder that has a live lease or a queued request; the engine calls it
//! periodically. `AgentSession` holders end through their liveness streams
//! and the reconnect grace instead ([`crate::scheduler::session`], which
//! reuses [`Scheduler::end_holders`]); `User` holders only through lease
//! expiry.

use std::collections::HashSet;
use std::fs;

use serde_json::json;

use crate::model::{EndCause, HolderRef, LeaseId, LeaseState, RequestId};
use crate::scheduler::dispatch::QueueSnapshot;
use crate::scheduler::{Scheduler, SchedulerError, clerk_actor};
use crate::store::notices::NoticeKind;
use crate::store::{Tx, audit, leases};

/// The process state (field 3) and start time (field 22, clock ticks since
/// boot) from the text of a `/proc/<pid>/stat` file. The command name
/// (field 2) is in parentheses and may itself contain spaces and `)`, so
/// the fields are read after the last `)`.
fn parse_stat(stat: &str) -> Option<(char, u64)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    // Fields 4 to 21 lie between the state and the start time.
    let start_time = fields.nth(18)?.parse().ok()?;
    Some((state, start_time))
}

/// The state and start time of process `pid`, or `None` when it has no
/// readable `/proc` entry.
fn read_stat(pid: u32) -> Option<(char, u64)> {
    parse_stat(&fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// The start time in the text of a `/proc/<pid>/stat` file, or `None` when
/// it cannot be parsed.
fn start_from_stat(stat: &str) -> Option<u64> {
    parse_stat(stat).map(|(_, start)| start)
}

/// The start time of process `pid` (field 22 of `/proc/<pid>/stat`), or
/// `None` when there is no such process.
#[must_use]
pub fn start_time(pid: u32) -> Option<u64> {
    read_stat(pid).map(|(_, start)| start)
}

/// The start time of the current process, used to build a CLI's
/// [`HolderRef::CliProcess`]. `None` when `/proc/self/stat` cannot be read
/// or parsed: the caller must not build a `CliProcess` holder then, since
/// any start time it made up would get its leases ended by the next probe.
#[must_use]
pub fn self_start_time() -> Option<u64> {
    start_from_stat(&fs::read_to_string("/proc/self/stat").ok()?)
}

/// True when process `pid` exists, is not a zombie, and started at
/// `start_time`. A missing process, or a different start time (the pid was
/// reused), means dead.
#[must_use]
pub fn process_alive(pid: u32, start_time: u64) -> bool {
    match read_stat(pid) {
        // A zombie (or a dying task) has exited; only its entry is left.
        Some((state, start)) => !matches!(state, 'Z' | 'X' | 'x') && start == start_time,
        None => false,
    }
}

/// Check every `CliProcess` holder that has a live lease or a queued
/// request, and call [`Scheduler::holder_gone`] for each one whose process
/// is no longer alive. Returns the holders found gone, in the order first
/// seen (leases by row, then requests by row).
///
/// # Errors
/// [`SchedulerError::Store`] on storage failure.
pub fn probe_all(tx: &Tx, sched: &Scheduler) -> Result<Vec<HolderRef>, SchedulerError> {
    let mut seen: HashSet<HolderRef> = HashSet::new();
    let mut holders: Vec<HolderRef> = Vec::new();
    let lease_holders = leases::live_leases(tx)?.into_iter().map(|l| l.holder);
    let request_holders = leases::queued_requests(tx)?.into_iter().map(|r| r.holder);
    for holder in lease_holders.chain(request_holders) {
        if matches!(holder, HolderRef::CliProcess { .. }) && seen.insert(holder.clone()) {
            holders.push(holder);
        }
    }
    let mut gone = Vec::new();
    for holder in holders {
        let HolderRef::CliProcess { pid, start_time } = holder else {
            continue;
        };
        if process_alive(pid, start_time) {
            continue;
        }
        tracing::info!(pid, start_time, "holder process is gone; ending its leases");
        sched.holder_gone(tx, &holder)?;
        gone.push(holder);
    }
    Ok(gone)
}

/// What [`Scheduler::end_holders`] took away.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HolderEnded {
    /// The queued requests dropped.
    pub requests: Vec<RequestId>,
    /// The leases ended.
    pub leases: Vec<LeaseId>,
}

impl Scheduler {
    /// `holder` went away: drop its queued requests (a holder asked to
    /// yield for one is told the ask is withdrawn, and waiters behind are
    /// told their new positions), then end each of its live leases with
    /// cause `HolderGone` and cleanup unconfirmed, through
    /// [`Scheduler::end_lease`]. A held or cleaning lease's unit is marked
    /// dirty (`holder_gone`); a paused lease forfeits its claim, so its
    /// borrower keeps the unit; a borrower's unit returns to the paused
    /// holder. One `holder_gone` audit entry lists what was dropped and
    /// ended, and the call ends with a dispatch. All in `tx`.
    ///
    /// # Errors
    /// [`SchedulerError::Store`] on storage failure.
    pub fn holder_gone(&self, tx: &Tx, holder: &HolderRef) -> Result<(), SchedulerError> {
        let ended = self.end_holders(tx, &|h| h == holder, EndCause::HolderGone, false)?;
        audit::record(
            tx,
            self.now(),
            &clerk_actor(),
            "holder_gone",
            &json!({
                "holder": holder,
                "requests_dropped": ended.requests,
                "leases_ended": ended.leases,
            }),
        )?;
        self.dispatch(tx)
    }

    /// End everything of every holder `matches` accepts: drop its queued
    /// requests through `remove_request` (so a holder asked to yield for one
    /// is told the ask is withdrawn; waiters behind are told their new
    /// positions), then end each of its live leases with `cause` and cleanup
    /// unconfirmed, through [`Scheduler::end_lease`]. With `tell`, each
    /// dropped request's holder gets `QueueDropped` (a holder that is gone
    /// cannot read it). The caller audits and dispatches.
    ///
    /// # Errors
    /// [`SchedulerError::Store`] on storage failure.
    pub fn end_holders(
        &self,
        tx: &Tx,
        matches: &dyn Fn(&HolderRef) -> bool,
        cause: EndCause,
        tell: bool,
    ) -> Result<HolderEnded, SchedulerError> {
        // Requests go first, so no dispatch below offers the holder a unit.
        let requests: Vec<_> = leases::queued_requests(tx)?
            .into_iter()
            .filter(|r| matches(&r.holder))
            .collect();
        let dropped = if tell {
            self.drop_requests(tx, &requests, NoticeKind::QueueDropped, &|resource| {
                format!(
                    "An administrator removed you from the queue for {resource}; acquire again \
                     if you still need it."
                )
            })?
        } else {
            let before = QueueSnapshot::load(tx)?;
            for request in &requests {
                self.remove_request(tx, request)?;
            }
            let after = QueueSnapshot::load(tx)?;
            self.publish_moves(tx, &before, &after)?;
            requests.into_iter().map(|r| r.id).collect()
        };
        let live: Vec<_> = leases::live_leases(tx)?
            .into_iter()
            .filter(|l| matches(&l.holder))
            .map(|l| l.id)
            .collect();
        let mut ended = Vec::new();
        for id in live {
            // Re-read: ending one lease dispatches, which may change another.
            let still_live =
                leases::find_lease(tx, &id)?.is_some_and(|l| l.state != LeaseState::Ended);
            if still_live {
                self.end_lease(tx, &id, cause, false)?;
                ended.push(id);
            }
        }
        Ok(HolderEnded {
            requests: dropped,
            leases: ended,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stat line with fields 3..=22 numbered by position, so field 22
    /// reads 22.
    fn stat_with_comm(comm: &str) -> String {
        let tail: Vec<String> = (4..=52).map(|n| n.to_string()).collect();
        format!("1234 ({comm}) S {}", tail.join(" "))
    }

    #[test]
    fn reads_state_and_start_time() {
        assert_eq!(parse_stat(&stat_with_comm("sleep")), Some(('S', 22)));
    }

    #[test]
    fn comm_with_spaces_and_parens_is_skipped_whole() {
        assert_eq!(parse_stat(&stat_with_comm("a) (b c) 7")), Some(('S', 22)));
    }

    #[test]
    fn start_time_of_an_unparsable_stat_is_none() {
        assert_eq!(start_from_stat(""), None);
        assert_eq!(start_from_stat("1234 (sleep) S 1 2 3"), None);
        assert_eq!(start_from_stat("1234 (sleep) S x"), None);
        assert_eq!(start_from_stat(&stat_with_comm("sleep")), Some(22));
    }

    #[test]
    fn truncated_stat_is_unreadable() {
        assert_eq!(parse_stat("1234 (sleep) S 1 2 3"), None);
        assert_eq!(parse_stat("garbage"), None);
    }
}
