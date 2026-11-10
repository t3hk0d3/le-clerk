//! Agent-session liveness.
//!
//! An `AgentSession` holder is alive while at least one of its liveness
//! streams (`GET /v1/session/live`, opened by `clerk mcp`) is open. When the
//! last one closes, the reconnect grace starts; a stream of the session
//! opening inside it cancels it, and when it runs out the session's leases
//! end with cause `holder_gone` and its requests are dropped, as for a CLI
//! process that went away ([`Scheduler::holder_gone`]). After a daemon
//! restart every `AgentSession` holder with a live lease or a queued request
//! gets the same grace from boot, so its proxy can reconnect.
//!
//! [`Sessions`] is the engine's in-memory record of that; the engine drives
//! [`Sessions::expire`] from its clock. Grace algorithm:
//!
//! ```text
//! on connect(s, c):   open[s] += {c}; cancel grace[s]
//! on disconnect(c):   s = owner(c); open[s] -= {c}
//!                     if open[s] empty: grace[s] = now + reconnect_grace
//! on tick(now):       for s with grace[s] <= now: end_holder(AgentSession(s)); forget s
//! on daemon start:    for each AgentSession with live leases or requests:
//!                         grace[s] = boot + reconnect_grace
//! ```
//!
//! Hooks run as children of the Claude Code process, whose session id may
//! differ from the proxy's (`--continue`, `/clear`). [`Sessions::resolve`]
//! maps a hook to the live proxy session with the same Claude Code PID, and
//! [`Sessions::alias`] (the SessionStart-resume hook) hands a resumed
//! session's leases, requests and pending notices to the live proxy session.
//! Aliases are kept in the store (`session_aliases`) and loaded at boot, so
//! a chain of resumes (`--continue` twice) resolves across daemon restarts.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use serde::Serialize;
use serde_json::json;
use ulid::Ulid;

use crate::model::{EndCause, HolderRef, ProjectKey};
use crate::scheduler::liveness::HolderEnded;
use crate::scheduler::{Scheduler, SchedulerError, clerk_actor};
use crate::store::{StoreError, Tx, audit, leases, notices, sessions as alias_store};

/// What [`Sessions::alias`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "outcome", content = "reason")]
pub enum Aliased {
    /// The live proxy session took the resumed session over.
    TakenOver,
    /// Both ids already stand for the same session.
    Unchanged,
    /// Nothing moved; a `session_alias_refused` audit entry says why.
    Refused(AliasRefusal),
}

/// Why an alias was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AliasRefusal {
    /// The new proxy session has no open liveness stream (yet): what it
    /// took over would never get a grace.
    NotConnected,
    /// The resumed session still has an open liveness stream: another
    /// Claude Code process is running it.
    ResumedStillConnected,
    /// The two sessions belong to different projects.
    ProjectMismatch,
}

/// One agent session as the liveness record knows it.
#[derive(Debug, Default)]
struct Session {
    /// Open liveness streams, by connection (a connection may briefly have
    /// two while a proxy reconnects).
    open: HashMap<Ulid, usize>,
    /// The Claude Code process its proxy last named.
    claude_pid: Option<u32>,
    /// When the reconnect grace runs out, unix ms; set only while no stream
    /// is open.
    grace_until: Option<i64>,
    /// Order of the latest connect, to prefer the newest proxy in
    /// [`Sessions::resolve`].
    connected: u64,
    /// Whether a liveness stream of it ever opened (in this daemon).
    streamed: bool,
    /// The project of its latest liveness stream.
    project: Option<ProjectKey>,
}

/// The liveness record of every agent session the engine has seen.
#[derive(Debug)]
pub struct Sessions {
    grace_ms: i64,
    known: HashMap<String, Session>,
    /// Which session each open connection belongs to.
    owners: HashMap<Ulid, String>,
    /// Resumed session id → the session that took it over (a mirror of
    /// the store's `session_aliases`, loaded at boot).
    aliases: HashMap<String, String>,
    connects: u64,
}

impl Sessions {
    /// An empty record with a reconnect grace of `reconnect_grace`.
    #[must_use]
    pub fn new(reconnect_grace: Duration) -> Self {
        Self {
            grace_ms: i64::try_from(reconnect_grace.as_millis()).unwrap_or(i64::MAX),
            known: HashMap::new(),
            owners: HashMap::new(),
            aliases: HashMap::new(),
            connects: 0,
        }
    }

    /// The id `session_id` stands for: the end of its chain of resume
    /// aliases (itself when it has none).
    fn follow(&self, session_id: &str) -> String {
        let mut id = session_id;
        let mut seen = HashSet::new();
        while let Some(next) = self.aliases.get(id) {
            // Aliases only ever point at chain ends, so a cycle cannot be
            // written; the guard keeps a damaged table from hanging us.
            if !seen.insert(id) {
                break;
            }
            id = next;
        }
        id.to_owned()
    }

    /// A liveness stream of `session_id` (after alias resolution) opened on
    /// `connection` in `project`: the session is alive and any grace is
    /// cancelled. Returns the session the stream counts for.
    pub fn connect(
        &mut self,
        session_id: &str,
        connection: Ulid,
        claude_pid: Option<u32>,
        project: Option<ProjectKey>,
    ) -> String {
        let id = self.follow(session_id);
        self.connects += 1;
        let session = self.known.entry(id.clone()).or_default();
        *session.open.entry(connection).or_insert(0) += 1;
        session.grace_until = None;
        session.connected = self.connects;
        session.streamed = true;
        session.project = project;
        if claude_pid.is_some() {
            session.claude_pid = claude_pid;
        }
        self.owners.insert(connection, id.clone());
        id
    }

    /// A liveness stream on `connection` closed at `now_ms`. When it was its
    /// session's last open stream, the reconnect grace starts.
    pub fn disconnect(&mut self, connection: Ulid, now_ms: i64) {
        let Some(id) = self.owners.get(&connection).cloned() else {
            return;
        };
        let Some(session) = self.known.get_mut(&id) else {
            self.owners.remove(&connection);
            return;
        };
        if let Some(n) = session.open.get_mut(&connection) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                session.open.remove(&connection);
                self.owners.remove(&connection);
            }
        }
        if session.open.is_empty() {
            session.grace_until = Some(now_ms.saturating_add(self.grace_ms));
        }
    }

    /// True while `session_id` (after alias resolution) has an open
    /// liveness stream.
    #[must_use]
    pub fn is_alive(&self, session_id: &str) -> bool {
        self.known
            .get(&self.follow(session_id))
            .is_some_and(|s| !s.open.is_empty())
    }

    /// When `session_id`'s reconnect grace runs out, if one is running.
    #[must_use]
    pub fn grace_ends(&self, session_id: &str) -> Option<i64> {
        self.known.get(session_id).and_then(|s| s.grace_until)
    }

    /// The earliest running grace's end, for the engine's timer.
    #[must_use]
    pub fn next_expiry(&self) -> Option<i64> {
        self.known.values().filter_map(|s| s.grace_until).min()
    }

    /// The session a hook (or request) of `session_id` from Claude Code
    /// process `claude_pid` acts for: the live proxy session with that
    /// Claude PID (the session id itself when it is one of them, else the
    /// newest), otherwise `session_id` after alias resolution.
    #[must_use]
    pub fn resolve(&self, claude_pid: Option<u32>, session_id: &str) -> String {
        let own = self.follow(session_id);
        let Some(pid) = claude_pid else {
            return own;
        };
        let mut live: Vec<(&String, &Session)> = self
            .known
            .iter()
            .filter(|(_, s)| !s.open.is_empty() && s.claude_pid == Some(pid))
            .collect();
        if live.iter().any(|(id, _)| **id == own) {
            return own;
        }
        live.sort_by_key(|(_, s)| s.connected);
        live.last().map_or(own, |(id, _)| (*id).clone())
    }

    /// Daemon start at `now_ms`: the resume aliases are loaded from the
    /// store, and every `AgentSession` holder with a live lease or a queued
    /// request gets the reconnect grace from now. Returns the sessions
    /// graced.
    ///
    /// # Errors
    /// [`StoreError`] on storage failure.
    pub fn boot(&mut self, tx: &Tx, now_ms: i64) -> Result<Vec<String>, StoreError> {
        self.aliases.extend(alias_store::aliases(tx)?);
        let until = now_ms.saturating_add(self.grace_ms);
        let mut graced = Vec::new();
        for id in agent_holders(tx)? {
            let session = self.known.entry(id.clone()).or_default();
            if session.open.is_empty() && session.grace_until.is_none() {
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
            tracing::info!(session = %id, "agent session's reconnect grace is over");
            end_session(tx, sched, id)?;
            self.forget(id);
        }
        Ok(due)
    }

    /// `SessionEnd`: end `session_id`'s leases and requests now, without
    /// grace. Its open streams, if any, still count it as connected.
    ///
    /// # Errors
    /// [`SchedulerError::Store`] on storage failure.
    pub fn end_now(
        &mut self,
        tx: &Tx,
        sched: &Scheduler,
        session_id: &str,
    ) -> Result<HolderEnded, SchedulerError> {
        if let Some(session) = self.known.get_mut(session_id) {
            session.grace_until = None;
            if session.open.is_empty() {
                self.forget(session_id);
            }
        }
        end_session(tx, sched, session_id)
    }

    /// `SessionStart` with `source: "resume"`: the live proxy session `from`
    /// (Claude Code's start-up id) takes over the resumed session `to`,
    /// both resolved through the alias chain. The leases, queued requests
    /// and pending notices of `to` are handed to `from`, `to`'s grace is
    /// cancelled, and `to` resolves to `from` from now on (persisted).
    ///
    /// Refused, moving nothing and auditing `session_alias_refused`, when
    /// `from` has no open liveness stream, when `to` still has one, or when
    /// they belong to different projects (as their streams, leases and
    /// requests say).
    ///
    /// # Errors
    /// [`SchedulerError::Store`] on storage failure.
    pub fn alias(
        &mut self,
        tx: &Tx,
        sched: &Scheduler,
        from: &str,
        to: &str,
    ) -> Result<Aliased, SchedulerError> {
        let live = self.follow(from);
        let resumed = self.follow(to);
        if live == resumed {
            return Ok(Aliased::Unchanged);
        }
        let resumed_holder = HolderRef::AgentSession {
            session_id: resumed.clone(),
        };
        if let Some(reason) = self.refusal(tx, &live, &resumed, &resumed_holder)? {
            tracing::info!(from = %live, to = %resumed, ?reason, "resume alias refused");
            audit::record(
                tx,
                sched.now(),
                &clerk_actor(),
                "session_alias_refused",
                &json!({ "from": from, "to": to, "session": live, "resumed": resumed,
                         "reason": reason }),
            )?;
            return Ok(Aliased::Refused(reason));
        }
        sched.take_over_holder(
            tx,
            &resumed_holder,
            &HolderRef::AgentSession {
                session_id: live.clone(),
            },
        )?;
        alias_store::insert_alias(tx, &resumed, &live, sched.now())?;
        self.known.remove(&resumed);
        self.aliases.insert(resumed, live);
        Ok(Aliased::TakenOver)
    }

    /// Why `live` may not take over `resumed`, if it may not.
    fn refusal(
        &self,
        tx: &Tx,
        live: &str,
        resumed: &str,
        resumed_holder: &HolderRef,
    ) -> Result<Option<AliasRefusal>, StoreError> {
        let Some(session) = self.known.get(live).filter(|s| !s.open.is_empty()) else {
            return Ok(Some(AliasRefusal::NotConnected));
        };
        if self.is_alive(resumed) {
            return Ok(Some(AliasRefusal::ResumedStillConnected));
        }
        let project = session.project.clone();
        let mut theirs: Vec<Option<ProjectKey>> = Vec::new();
        if let Some(recorded) = self.known.get(resumed).filter(|s| s.streamed) {
            theirs.push(recorded.project.clone());
        }
        theirs.extend(
            leases::live_leases(tx)?
                .into_iter()
                .filter(|l| l.holder == *resumed_holder)
                .map(|l| l.project),
        );
        theirs.extend(
            leases::queued_requests(tx)?
                .into_iter()
                .filter(|r| r.holder == *resumed_holder)
                .map(|r| r.project),
        );
        if theirs.iter().any(|p| *p != project) {
            return Ok(Some(AliasRefusal::ProjectMismatch));
        }
        Ok(None)
    }

    /// Drop everything known about `session_id`.
    fn forget(&mut self, session_id: &str) {
        if let Some(session) = self.known.remove(session_id) {
            for connection in session.open.keys() {
                self.owners.remove(connection);
            }
        }
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
/// does, when it has any (nothing is audited otherwise).
fn end_session(
    tx: &Tx,
    sched: &Scheduler,
    session_id: &str,
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
    audit::record(
        tx,
        sched.now(),
        &clerk_actor(),
        "holder_gone",
        &json!({
            "holder": holder,
            "requests_dropped": ended.requests,
            "leases_ended": ended.leases,
        }),
    )?;
    sched.dispatch(tx)?;
    Ok(ended)
}

impl Scheduler {
    /// Hand everything `old` has to `new`: its live leases, its queued
    /// requests (keeping their place) and its undelivered notices. One
    /// `session_resumed` audit entry lists what moved.
    ///
    /// # Errors
    /// [`SchedulerError::Store`] on storage failure.
    pub fn take_over_holder(
        &self,
        tx: &Tx,
        old: &HolderRef,
        new: &HolderRef,
    ) -> Result<(), SchedulerError> {
        let mut moved_leases = Vec::new();
        for lease in leases::live_leases(tx)? {
            if lease.holder == *old {
                leases::set_holder(tx, lease.row, new)?;
                moved_leases.push(lease.id);
            }
        }
        let mut moved_requests = Vec::new();
        for request in leases::queued_requests(tx)? {
            if request.holder == *old {
                leases::set_request_holder(tx, request.row, new)?;
                moved_requests.push(request.id);
            }
        }
        let moved_notices = notices::rehome_all(tx, old, new)?;
        audit::record(
            tx,
            self.now(),
            &clerk_actor(),
            "session_resumed",
            &json!({
                "from": old,
                "to": new,
                "leases": moved_leases,
                "requests": moved_requests,
                "notices": moved_notices,
            }),
        )?;
        Ok(())
    }
}
