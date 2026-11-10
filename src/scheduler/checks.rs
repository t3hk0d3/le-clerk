//! Health checks off the engine thread.
//!
//! A check can take up to `health_timeout`, so it never runs inside a store
//! transaction. Scheduling reads a unit's [`CheckState`] from the
//! [`CheckBook`]; a unit with no fresh result is marked checking and a
//! [`CheckJob`] is queued. The engine hands queued jobs to the [`CheckPool`]
//! once its transaction has ended; the pool's answer comes back as an engine
//! job that records the result and dispatches again.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::Semaphore;

use crate::health::{self, HealthResult};
use crate::model::{HealthSpec, ResourceId, UnitId, UnitSettings};
use crate::scheduler::HealthProbe;

/// Most checks running at once in one pool.
pub const CHECK_CONCURRENCY: usize = 4;
/// How long a healthy result stands.
pub const HEALTHY_TTL: Duration = Duration::from_secs(5);
/// Time past its limit that a check gets to answer for itself: a checker
/// that honours its own timeout has stopped its work and said why by then.
const TIMEOUT_GRACE: Duration = Duration::from_millis(250);

/// Names one check of one unit. A result whose ticket is no longer the
/// unit's current one is dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckTicket {
    /// The unit's row.
    pub row: i64,
    /// Which check of the unit.
    pub token: u64,
}

/// A check to run.
#[derive(Debug, Clone)]
pub struct CheckJob {
    pub ticket: CheckTicket,
    pub spec: HealthSpec,
    pub resource: ResourceId,
    pub unit: UnitId,
    pub settings: UnitSettings,
}

/// What is known about a unit's health right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CheckState {
    /// No result stands: never checked, expired, or checked against other
    /// settings or another health spec.
    Due,
    /// A check is on its way.
    Checking,
    /// A result that still stands.
    Fresh { healthy: bool },
}

#[derive(Debug, Clone, Copy)]
enum Slot {
    Checking { token: u64 },
    Fresh { healthy: bool, until: i64 },
}

/// A unit's slot, with what it was checked against: a unit whose settings
/// or health spec differ (an update, or a reused row id) is checked afresh.
#[derive(Debug)]
struct Entry {
    settings: UnitSettings,
    spec: HealthSpec,
    slot: Slot,
}

/// Check states by unit row, in memory only (a restart forgets them, which
/// only costs a check), and the jobs waiting to go to the pool.
#[derive(Debug, Default)]
pub(super) struct CheckBook {
    entries: HashMap<i64, Entry>,
    next_token: u64,
    queued: Vec<CheckJob>,
}

impl CheckBook {
    pub(super) fn state(
        &self,
        row: i64,
        settings: &UnitSettings,
        spec: &HealthSpec,
        now: i64,
    ) -> CheckState {
        match self.entries.get(&row) {
            Some(e) if e.settings == *settings && e.spec == *spec => match e.slot {
                Slot::Checking { .. } => CheckState::Checking,
                Slot::Fresh { healthy, until } if now < until => CheckState::Fresh { healthy },
                Slot::Fresh { .. } => CheckState::Due,
            },
            _ => CheckState::Due,
        }
    }

    /// Whether a check of the unit at `row` is on its way.
    pub(super) fn checking(&self, row: i64) -> bool {
        self.entries
            .get(&row)
            .is_some_and(|e| matches!(e.slot, Slot::Checking { .. }))
    }

    /// Mark the unit checking and queue a job for it.
    pub(super) fn begin(
        &mut self,
        row: i64,
        resource: &ResourceId,
        unit: &UnitId,
        settings: &UnitSettings,
        spec: &HealthSpec,
    ) {
        self.next_token += 1;
        let token = self.next_token;
        self.entries.insert(
            row,
            Entry {
                settings: settings.clone(),
                spec: spec.clone(),
                slot: Slot::Checking { token },
            },
        );
        self.queued.push(CheckJob {
            ticket: CheckTicket { row, token },
            spec: spec.clone(),
            resource: resource.clone(),
            unit: unit.clone(),
            settings: settings.clone(),
        });
    }

    /// Keep an unhealthy result until `until`, whatever the unit's slot
    /// held.
    pub(super) fn keep_unhealthy(
        &mut self,
        row: i64,
        settings: &UnitSettings,
        spec: &HealthSpec,
        until: i64,
    ) {
        self.entries.insert(
            row,
            Entry {
                settings: settings.clone(),
                spec: spec.clone(),
                slot: Slot::Fresh {
                    healthy: false,
                    until,
                },
            },
        );
    }

    /// Settle the check `ticket` names, keeping its result until the time
    /// `until` picks for it. False when the ticket is out of date.
    pub(super) fn settle(
        &mut self,
        ticket: CheckTicket,
        healthy: bool,
        until: impl FnOnce(bool) -> i64,
    ) -> bool {
        match self.entries.get_mut(&ticket.row) {
            Some(e) if matches!(e.slot, Slot::Checking { token } if token == ticket.token) => {
                e.slot = Slot::Fresh {
                    healthy,
                    until: until(healthy),
                };
                true
            }
            _ => false,
        }
    }

    pub(super) fn forget(&mut self, row: i64) {
        self.entries.remove(&row);
    }

    pub(super) fn take_jobs(&mut self) -> Vec<CheckJob> {
        std::mem::take(&mut self.queued)
    }
}

type Done = dyn Fn(CheckTicket, HealthResult) + Send + Sync;

/// Runs checks on blocking threads, at most [`CHECK_CONCURRENCY`] at once.
pub struct CheckPool {
    runtime: Handle,
    permits: Arc<Semaphore>,
    probe: Arc<dyn HealthProbe>,
    timeout: Duration,
    done: Arc<Done>,
}

impl CheckPool {
    /// A pool running checks with `probe`, each bounded by `timeout`, and
    /// answering through `done`.
    ///
    /// # Errors
    /// [`io::Error`] when the runtime the checks use cannot be started.
    pub fn new(
        probe: Arc<dyn HealthProbe>,
        timeout: Duration,
        done: impl Fn(CheckTicket, HealthResult) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        Ok(Self {
            runtime: health::runtime()?,
            permits: Arc::new(Semaphore::new(CHECK_CONCURRENCY)),
            probe,
            timeout,
            done: Arc::new(done),
        })
    }

    /// Run `job` and report its result; returns at once. A check that has
    /// not answered within `timeout` (per check in its spec) is reported
    /// unhealthy, `timed out`. Its thread keeps its permit until it really
    /// ends, so hung checks never exceed the bound.
    pub fn run(&self, job: CheckJob) {
        let permits = self.permits.clone();
        let probe = self.probe.clone();
        let done = self.done.clone();
        let checks = u32::try_from(job.spec.checks.len().max(1)).unwrap_or(u32::MAX);
        let limit = self.timeout.saturating_mul(checks) + TIMEOUT_GRACE;
        self.runtime.spawn(async move {
            let ticket = job.ticket;
            let Ok(permit) = permits.acquire_owned().await else {
                // The semaphore is never closed.
                return done(ticket, HealthResult::Unhealthy("not checked".to_owned()));
            };
            let work = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                probe.check(&job.spec, &job.resource, &job.unit, &job.settings)
            });
            let result = match tokio::time::timeout(limit, work).await {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => HealthResult::Unhealthy("check failed unexpectedly".to_owned()),
                Err(_) => HealthResult::Unhealthy("timed out".to_owned()),
            };
            done(ticket, result);
        });
    }
}
