//! Agent-session hosts and liveness.
//!
//! An `AgentSession` holder is the Claude Code session
//! (`CLAUDE_CODE_SESSION_ID`). Every agent call and hook also reports its
//! **host**, the Claude Code process (`CLAUDE_PID`, with its start time so
//! a reused pid is never mistaken for it); the latest report wins. A process
//! runs one session at a time, so a report of another session from the same
//! process leaves the sessions it hosted before **unhosted** (`/clear`,
//! `/resume` of another conversation).
//!
//! A session is alive while it has a host whose process runs. When it stops
//! being alive (unhosted, or its process gone as [`Sessions::probe`] finds),
//! the reconnect grace starts; a report of the same session from any
//! process within it re-hosts the session and cancels the grace (`--continue`
//! and `--resume` keep the session id). When the grace runs out the
//! session's leases end with cause `holder_gone` and its requests are
//! dropped ([`Sessions::expire`]). `SessionEnd` ends a session at once, but
//! only from its current host ([`Sessions::end`]).
//!
//! ```text
//! on report(s, h):  for t != s hosted by h: unhost t; grace[t] = now + grace
//!                   host[s] = h; cancel grace[s]
//! on probe:         for s hosted by a dead process: unhost s; grace[s] = now + grace
//! on tick(now):     for s with grace[s] <= now: end_holder(AgentSession(s)); forget s
//! on daemon start:  hosts loaded from the store; every AgentSession with live
//!                   leases or requests and no running host: grace[s] = boot + grace
//! ```
//!
//! Hosts are kept in the store (`agent_hosts`), so a daemon restart keeps
//! a running agent's leases without waiting for it to call again.
//!
//! Work that writes the store goes through [`Sessions::transact`]: it runs
//! on a copy of the record, which replaces the record only once the store
//! transaction has committed, so a failed commit leaves memory and store
//! in agreement.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::json;

use crate::model::{EndCause, HolderRef, Host, LeaseId};
use crate::scheduler::liveness::{HolderEnded, process_alive};
use crate::scheduler::{Scheduler, SchedulerError, clerk_actor};
use crate::store::{Store, StoreError, Tx, audit, leases, sessions as host_store};

/// What [`Sessions::report`] did besides recording the host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reported {
    /// The sessions the reporting process hosted before and left, whose
    /// grace started.
    pub unhosted: Vec<String>,
}

/// One agent session as the liveness record knows it.
#[derive(Debug, Clone, Default)]
struct Session {
    /// The process running it; `None` once it left or died.
    host: Option<Host>,
    /// When the grace runs out, unix ms; set only while it has no host.
    grace_until: Option<i64>,
}

/// The liveness record of every agent session the engine has seen.
#[derive(Debug, Clone)]
pub struct Sessions {
    grace_ms: i64,
    known: HashMap<String, Session>,
    /// The leases each process's last ended session released
    /// ([`Sessions::end`]), for the next session's brief.
    released: HashMap<Host, Vec<LeaseId>>,
}

impl Sessions {
    /// An empty record with a reconnect grace of `reconnect_grace`.
    #[must_use]
    pub fn new(reconnect_grace: Duration) -> Self {
        Self {
            grace_ms: i64::try_from(reconnect_grace.as_millis()).unwrap_or(i64::MAX),
            known: HashMap::new(),
            released: HashMap::new(),
        }
    }

    /// Run `f` on a copy of this record inside a transaction of `store`;
    /// the copy replaces the record only once the transaction committed.
    /// On any error (from `f`, or the commit) the record is unchanged.
    ///
    /// # Errors
    /// `f`'s error, or the store's.
    pub fn transact<T, E: From<StoreError>>(
        &mut self,
        store: &mut Store,
        f: impl FnOnce(&Tx<'_>, &mut Sessions) -> Result<T, E>,
    ) -> Result<T, E> {
        let mut draft = self.clone();
        let value = store.tx(|tx| f(tx, &mut draft))?;
        *self = draft;
        Ok(value)
    }

    /// `session_id` was reported by `host` at `now_ms`: `host` runs it now
    /// (any grace is cancelled), and every other session `host` ran before
    /// is unhosted and gets the grace.
    ///
    /// # Errors
    /// [`StoreError`] on storage failure.
    pub fn report(
        &mut self,
        tx: &Tx,
        session_id: &str,
        host: Host,
        now_ms: i64,
    ) -> Result<Reported, StoreError> {
        let mut unhosted: Vec<String> = self
            .known
            .iter()
            .filter(|(id, s)| id.as_str() != session_id && s.host == Some(host))
            .map(|(id, _)| id.clone())
            .collect();
        unhosted.sort();
        for id in &unhosted {
            self.unhost(tx, id, now_ms)?;
        }
        let session = self.known.entry(session_id.to_owned()).or_default();
        let changed = session.host != Some(host) || session.grace_until.is_some();
        session.host = Some(host);
        session.grace_until = None;
        if changed {
            host_store::set_host(tx, session_id, host, now_ms)?;
        }
        Ok(Reported { unhosted })
    }

    /// Whether a notice wait of `session_id` from `host` may run, at
    /// `now_ms`. A wait never takes a session back from the session its
    /// process runs now (a wait started before `/clear` must not undo it):
    /// refused when `host` runs another session and not this one.
    /// Otherwise `host` is reported as the session's host (a wait may be
    /// the first call of a resumed session) and the wait may run.
    ///
    /// # Errors
    /// [`StoreError`] on storage failure.
    pub fn admit_wait(
        &mut self,
        tx: &Tx,
        session_id: &str,
        host: Host,
        now_ms: i64,
    ) -> Result<bool, StoreError> {
        if self.host_of(session_id) == Some(host) {
            return Ok(true);
        }
        let elsewhere = self
            .known
            .iter()
            .any(|(id, s)| id.as_str() != session_id && s.host == Some(host));
        if elsewhere {
            return Ok(false);
        }
        self.report(tx, session_id, host, now_ms)?;
        Ok(true)
    }

    /// Unhost every session whose host process is gone at `now_ms`; their
    /// grace starts. Returns those sessions.
    ///
    /// # Errors
    /// [`StoreError`] on storage failure.
    pub fn probe(&mut self, tx: &Tx, now_ms: i64) -> Result<Vec<String>, StoreError> {
        let mut gone: Vec<String> = self
            .known
            .iter()
            .filter(|(_, s)| s.host.is_some_and(|h| !process_alive(h.pid, h.start_time)))
            .map(|(id, _)| id.clone())
            .collect();
        gone.sort();
        for id in &gone {
            tracing::info!(session = %id, "agent session's host process is gone");
            self.unhost(tx, id, now_ms)?;
        }
        Ok(gone)
    }

    /// `session_id` has no host from `now_ms`; its grace starts.
    fn unhost(&mut self, tx: &Tx, session_id: &str, now_ms: i64) -> Result<(), StoreError> {
        let session = self.known.entry(session_id.to_owned()).or_default();
        session.host = None;
        session.grace_until = Some(now_ms.saturating_add(self.grace_ms));
        host_store::clear_host(tx, session_id, now_ms)
    }

    /// True while `session_id` has a host (whose process ran at the last
    /// report or probe).
    #[must_use]
    pub fn is_alive(&self, session_id: &str) -> bool {
        self.host_of(session_id).is_some()
    }

    /// The process running `session_id`, if any.
    #[must_use]
    pub fn host_of(&self, session_id: &str) -> Option<Host> {
        self.known.get(session_id).and_then(|s| s.host)
    }

    /// When `session_id`'s grace runs out, if one is running.
    #[must_use]
    pub fn grace_ends(&self, session_id: &str) -> Option<i64> {
        self.known.get(session_id).and_then(|s| s.grace_until)
    }

    /// The earliest running grace's end, for the engine's timer.
    #[must_use]
    pub fn next_expiry(&self) -> Option<i64> {
        self.known.values().filter_map(|s| s.grace_until).min()
    }

    /// Daemon start at `now_ms`: hosts are loaded from the store, and every
    /// `AgentSession` holder with a live lease or a queued request and no
    /// running host gets the grace from now. Returns the sessions graced.
    ///
    /// # Errors
    /// [`StoreError`] on storage failure.
    pub fn boot(&mut self, tx: &Tx, now_ms: i64) -> Result<Vec<String>, StoreError> {
        for (id, host) in host_store::hosts(tx)? {
            if process_alive(host.pid, host.start_time) {
                self.known.entry(id).or_default().host = Some(host);
            }
        }
        let until = now_ms.saturating_add(self.grace_ms);
        let mut graced = Vec::new();
        for id in agent_holders(tx)? {
            let session = self.known.entry(id.clone()).or_default();
            if session.host.is_none() && session.grace_until.is_none() {
                session.grace_until = Some(until);
                graced.push(id);
            }
        }
        Ok(graced)
    }

    /// End every session whose grace ran out by `now_ms`: its leases end
    /// (`holder_gone`) and its requests are dropped, and the next waiters
    /// are dispatched. The session is forgotten. Returns the sessions
    /// ended.
    ///
    /// # Errors
    /// [`SchedulerError::Store`] on storage failure.
    pub fn expire(
        &mut self,
        tx: &Tx,
        sched: &Scheduler,
        now_ms: i64,
    ) -> Result<Vec<String>, SchedulerError> {
        let mut due: Vec<String> = self
            .known
            .iter()
            .filter(|(_, s)| s.grace_until.is_some_and(|at| at <= now_ms))
            .map(|(id, _)| id.clone())
            .collect();
        due.sort();
        for id in &due {
            tracing::info!(session = %id, "agent session's grace is over");
            end_session(tx, sched, id, "holder_gone", &json!({}))?;
            self.forget(tx, id)?;
        }
        Ok(due)
    }

    /// `SessionEnd` of `session_id` with `reason`, from `host`: end its
    /// leases and requests now, without grace, and forget it. Refused
    /// (`None`, nothing changes) when another process runs the session
    /// now: a second Claude Code on the same session keeps it.
    ///
    /// # Errors
    /// [`SchedulerError::Store`] on storage failure.
    pub fn end(
        &mut self,
        tx: &Tx,
        sched: &Scheduler,
        session_id: &str,
        host: Option<Host>,
        reason: &str,
    ) -> Result<Option<HolderEnded>, SchedulerError> {
        if let Some(current) = self.host_of(session_id)
            && host != Some(current)
        {
            return Ok(None);
        }
        let ended = end_session(
            tx,
            sched,
            session_id,
            "session_ended",
            &json!({ "session": session_id, "reason": reason }),
        )?;
        self.forget(tx, session_id)?;
        if let Some(host) = host
            && !ended.leases.is_empty()
        {
            self.released.insert(host, ended.leases.clone());
        }
        Ok(Some(ended))
    }

    /// The leases the last session `host` ended released, once: the next
    /// session's brief says so (`/clear`).
    pub fn take_released(&mut self, host: Host) -> Vec<LeaseId> {
        self.released.remove(&host).unwrap_or_default()
    }

    /// Drop everything known about `session_id`.
    fn forget(&mut self, tx: &Tx, session_id: &str) -> Result<(), StoreError> {
        self.known.remove(session_id);
        host_store::forget(tx, session_id)
    }
}

/// The session ids of every `AgentSession` holder with a live lease or a
/// queued request, without duplicates.
fn agent_holders(tx: &Tx) -> Result<Vec<String>, StoreError> {
    let lease_holders = leases::live_leases(tx)?.into_iter().map(|l| l.holder);
    let request_holders = leases::queued_requests(tx)?.into_iter().map(|r| r.holder);
    let mut ids: Vec<String> = Vec::new();
    for holder in lease_holders.chain(request_holders) {
        if let HolderRef::AgentSession { session_id } = holder
            && !ids.contains(&session_id)
        {
            ids.push(session_id);
        }
    }
    Ok(ids)
}

/// End `session_id`'s leases and requests as [`Scheduler::holder_gone`]
/// does, auditing `action` with `extra` merged into the details; nothing is
/// audited when it has neither.
fn end_session(
    tx: &Tx,
    sched: &Scheduler,
    session_id: &str,
    action: &str,
    extra: &serde_json::Value,
) -> Result<HolderEnded, SchedulerError> {
    let holder = HolderRef::AgentSession {
        session_id: session_id.to_owned(),
    };
    let has_lease = leases::live_leases(tx)?.iter().any(|l| l.holder == holder);
    let has_request = leases::queued_requests(tx)?
        .iter()
        .any(|r| r.holder == holder);
    if !has_lease && !has_request {
        return Ok(HolderEnded::default());
    }
    let ended = sched.end_holders(tx, &|h| *h == holder, EndCause::HolderGone, false)?;
    let mut details = json!({
        "holder": holder,
        "requests_dropped": ended.requests,
        "leases_ended": ended.leases,
    });
    if let (Some(d), Some(e)) = (details.as_object_mut(), extra.as_object()) {
        d.extend(e.clone());
    }
    audit::record(tx, sched.now(), &clerk_actor(), action, &details)?;
    sched.dispatch(tx)?;
    Ok(ended)
}
