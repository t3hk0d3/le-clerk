mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use leclerk::clock::ManualClock;
use leclerk::config::Config;
use leclerk::health::HealthResult;
use leclerk::inventory::{Inventory, NewResource, NewUnit};
use leclerk::model::{
    Caller, DirtyReason, EndCause, HealthSpec, HolderRef, LeaseId, LeaseState, Priority,
    ProjectKey, RequestId, ResourceId, ResourceType, Scope, UnitId, UnitSettings,
};
use leclerk::scheduler::{
    AcquireOutcome, AcquireRequest, Confirm, Grant, HealthProbe, Lease, Scheduler, SchedulerError,
    Target,
};
use leclerk::store::audit::{self, AuditEntry};
use leclerk::store::notices::{self, Notice, NoticeKind};
use leclerk::store::{Store, StoreError, Tx};

const T0: i64 = 1_000_000;
const SEC: i64 = 1_000;
const MIN_MS: i64 = 60 * SEC;
const MIN: Duration = Duration::from_secs(60);
/// Default yield and cleanup windows.
const WINDOW_MS: i64 = 2 * MIN_MS;

fn rid(s: &str) -> ResourceId {
    ResourceId::parse(s).unwrap()
}

fn uid(s: &str) -> UnitId {
    UnitId::parse(s).unwrap()
}

fn holder(tag: &str) -> HolderRef {
    HolderRef::User { tag: tag.into() }
}

fn caller_in(tag: &str, project: Option<&str>) -> Caller {
    Caller {
        holder: holder(tag),
        project: project.map(|p| ProjectKey(PathBuf::from(p))),
        override_reason: None,
    }
}

fn caller(tag: &str) -> Caller {
    caller_in(tag, None)
}

fn req(resource: &str, duration: Duration, priority: Priority) -> AcquireRequest {
    AcquireRequest {
        target: Target::Resource(rid(resource)),
        duration,
        interruptible: false,
        priority,
        purpose: format!("{priority:?} job"),
        override_reason: None,
    }
}

/// An interruptible request.
fn ireq(resource: &str, duration: Duration, priority: Priority) -> AcquireRequest {
    AcquireRequest {
        interruptible: true,
        ..req(resource, duration, priority)
    }
}

fn override_req(resource: &str, reason: &str) -> AcquireRequest {
    AcquireRequest {
        override_reason: Some(reason.into()),
        ..req(resource, 10 * MIN, Priority::Normal)
    }
}

struct Healthy;

impl HealthProbe for Healthy {
    fn check(
        &self,
        _spec: &HealthSpec,
        _resource: &ResourceId,
        _unit: &UnitId,
        _settings: &UnitSettings,
    ) -> HealthResult {
        HealthResult::Healthy
    }
}

struct Fixture {
    store: Store,
    inv: Inventory,
    sched: Scheduler,
    clock: Arc<ManualClock>,
}

fn fixture() -> Fixture {
    let clock = Arc::new(ManualClock::new(T0));
    Fixture {
        store: Store::open_in_memory().unwrap(),
        inv: Inventory::new(clock.clone()),
        sched: Scheduler::new(clock.clone(), Config::default(), Arc::new(Healthy)),
        clock,
    }
}

impl Fixture {
    fn seed_keyed(&mut self, who: &Caller, id: &str, scope: &Scope, units: &[&str], key: &str) {
        self.store
            .tx(|tx| -> Result<(), leclerk::inventory::InventoryError> {
                self.inv.create_resource(
                    tx,
                    who,
                    &NewResource {
                        id: rid(id),
                        ty: ResourceType::Generic,
                        scope: scope.clone(),
                        description: String::new(),
                        notes: String::new(),
                        health: None,
                    },
                )?;
                for u in units {
                    self.inv.add_unit(
                        tx,
                        who,
                        &rid(id),
                        &NewUnit {
                            id: uid(u),
                            settings: UnitSettings::Generic {
                                identity_key: format!("{key}-{u}"),
                            },
                            notes: String::new(),
                        },
                    )?;
                }
                Ok(())
            })
            .unwrap();
    }

    fn seed(&mut self, id: &str, units: &[&str]) {
        self.seed_keyed(&caller("admin"), id, &Scope::User, units, id);
    }

    fn at(&self, ms: i64) {
        self.clock.set(ms);
    }

    fn acquire(&mut self, who: &Caller, r: AcquireRequest) -> AcquireOutcome {
        self.store.tx(|tx| self.sched.acquire(tx, who, r)).unwrap()
    }

    fn grant(&mut self, who: &Caller, r: AcquireRequest) -> Grant {
        match self.acquire(who, r) {
            AcquireOutcome::Granted(g) => g,
            other => panic!("expected grant, got {other:?}"),
        }
    }

    fn queue(&mut self, who: &Caller, r: AcquireRequest) -> (RequestId, u32) {
        match self.acquire(who, r) {
            AcquireOutcome::Queued {
                request_id,
                position,
                ..
            } => (request_id, position),
            other => panic!("expected queued, got {other:?}"),
        }
    }

    fn tick(&mut self) -> Option<i64> {
        self.store.tx(|tx| self.sched.tick(tx)).unwrap()
    }

    fn confirm(
        &mut self,
        who: &Caller,
        lease: &LeaseId,
        how: Confirm,
    ) -> Result<(), SchedulerError> {
        self.store.tx(|tx| self.sched.confirm(tx, who, lease, how))
    }

    fn refuse(&mut self, who: &Caller, lease: &LeaseId) -> Result<(), SchedulerError> {
        self.store.tx(|tx| self.sched.refuse_yield(tx, who, lease))
    }

    fn reclaim(&mut self, who: &Caller, lease: &LeaseId) -> Result<(), SchedulerError> {
        self.store.tx(|tx| self.sched.reclaim(tx, who, lease))
    }

    fn set_interruptible(
        &mut self,
        who: &Caller,
        lease: &LeaseId,
        on: bool,
    ) -> Result<Lease, SchedulerError> {
        self.store
            .tx(|tx| self.sched.set_interruptible(tx, who, lease, on))
    }

    fn cancel(&mut self, who: &Caller, request: &RequestId) {
        self.store
            .tx(|tx| self.sched.cancel(tx, who, request))
            .unwrap();
    }

    fn retire(&mut self, resource: &str, unit: &str) {
        self.store
            .tx(|tx| -> Result<(), SchedulerError> {
                self.inv
                    .retire_unit(tx, &caller("admin"), &rid(resource), &uid(unit))
                    .unwrap();
                self.sched.dispatch(tx)
            })
            .unwrap();
    }

    fn total_notices(&mut self, h: &HolderRef) -> usize {
        self.with_tx(|tx| notices::pending(tx, h).unwrap()).len()
    }

    fn try_release(&mut self, who: &Caller, lease: &LeaseId) -> Result<Lease, SchedulerError> {
        self.store.tx(|tx| self.sched.release(tx, who, lease))
    }

    fn release(&mut self, who: &Caller, lease: &LeaseId) {
        self.store
            .tx(|tx| self.sched.release(tx, who, lease))
            .unwrap();
    }

    fn extend(&mut self, who: &Caller, lease: &LeaseId, new_ends_at: i64) {
        self.store
            .tx(|tx| self.sched.extend(tx, who, lease, new_ends_at))
            .unwrap();
    }

    fn claim(&mut self, who: &Caller, lease: &LeaseId) {
        self.store
            .tx(|tx| self.sched.claim(tx, who, lease))
            .unwrap();
    }

    fn end_lease(&mut self, lease: &LeaseId, cause: EndCause, confirmed: bool) {
        self.store
            .tx(|tx| self.sched.end_lease(tx, lease, cause, confirmed))
            .unwrap();
    }

    fn with_tx<T>(&mut self, f: impl FnOnce(&Tx) -> T) -> T {
        self.store
            .tx(|tx| -> Result<T, StoreError> { Ok(f(tx)) })
            .unwrap()
    }

    fn column<T: rusqlite::types::FromSql>(&mut self, lease: &LeaseId, col: &str) -> T {
        self.with_tx(|tx| {
            tx.conn()
                .query_row(
                    &format!("SELECT {col} FROM leases WHERE lease_id = ?1"),
                    [lease.as_str()],
                    |r| r.get(0),
                )
                .unwrap()
        })
    }

    fn state(&mut self, lease: &LeaseId) -> String {
        self.column(lease, "state")
    }

    fn lease_row(&mut self, lease: &LeaseId) -> i64 {
        self.column(lease, "row_id")
    }

    fn queued(&mut self) -> usize {
        self.with_tx(|tx| {
            tx.conn()
                .query_row("SELECT COUNT(*) FROM requests", [], |r| r.get::<_, i64>(0))
                .unwrap()
        })
        .try_into()
        .unwrap()
    }

    fn notices_of(&mut self, h: &HolderRef, kind: NoticeKind) -> Vec<Notice> {
        self.with_tx(|tx| notices::pending(tx, h).unwrap())
            .into_iter()
            .filter(|n| n.kind == kind)
            .collect()
    }

    fn count(&mut self, h: &HolderRef, kind: NoticeKind) -> usize {
        self.notices_of(h, kind).len()
    }

    fn audit(&mut self, action: &str) -> Vec<AuditEntry> {
        self.store
            .tx(audit::all)
            .unwrap()
            .into_iter()
            .filter(|e| e.action == action)
            .collect()
    }

    fn unit_dirty(&mut self, resource: &str, unit: &str) -> bool {
        let who = caller("admin");
        self.store
            .tx(|tx| self.inv.get_resource(tx, &who, &rid(resource)))
            .unwrap()
            .units
            .into_iter()
            .find(|u| u.id.as_str() == unit)
            .unwrap()
            .dirty
            .is_some()
    }

    /// The lease offered to `h` by its latest Offered notice.
    fn offered_lease(&mut self, h: &HolderRef) -> LeaseId {
        self.notices_of(h, NoticeKind::Offered)
            .last()
            .and_then(|n| n.lease_id.clone())
            .expect("an Offered notice with a lease id")
    }

    /// `a` holds `board` (one unit) interruptibly for 60 min from T0.
    fn held_interruptibly(&mut self) -> LeaseId {
        self.seed("board", &["u1"]);
        self.grant(&caller("a"), ireq("board", 60 * MIN, Priority::Normal))
            .lease_id
    }

    /// `a` holds interruptibly, `b` (high, 10 min) asks at T0 + 10 min and
    /// `a` yields at once. Returns (paused lease, borrower's offered lease).
    fn lent(&mut self) -> (LeaseId, LeaseId) {
        let paused = self.held_interruptibly();
        self.at(T0 + 10 * MIN_MS);
        self.queue(&caller("b"), req("board", 10 * MIN, Priority::High));
        self.confirm(&caller("a"), &paused, Confirm::Yielded)
            .unwrap();
        let borrowed = self.offered_lease(&holder("b"));
        (paused, borrowed)
    }

    /// [`Fixture::lent`] and `b` claims at once (T0 + 10 min).
    fn borrowed(&mut self) -> (LeaseId, LeaseId) {
        let (paused, borrowed) = self.lent();
        self.claim(&caller("b"), &borrowed);
        (paused, borrowed)
    }

    /// [`Fixture::borrowed`], then `b` extends to T0 + 40 min and the clock
    /// passes its declared end (T0 + 20 min) with a tick.
    fn overran(&mut self) -> (LeaseId, LeaseId) {
        let (paused, borrowed) = self.borrowed();
        self.extend(&caller("b"), &borrowed, T0 + 40 * MIN_MS);
        self.at(T0 + 21 * MIN_MS);
        self.tick();
        (paused, borrowed)
    }

    /// `a` holds `board` non-interruptibly from T0 for 60 min; at T0 + 5 min
    /// `o` overrides. Returns (a's lease, o's request).
    fn overridden(&mut self) -> (LeaseId, RequestId) {
        self.seed("board", &["u1"]);
        let g = self.grant(&caller("a"), req("board", 60 * MIN, Priority::Normal));
        self.at(T0 + 5 * MIN_MS);
        let (request, _) = self.queue(&caller("o"), override_req("board", "user said so"));
        (g.lease_id, request)
    }
}

mod interruption {
    use super::*;

    mod acquire_against_fully_held_resource {
        use super::*;

        mod on_interruptible_holder_with_lower_priority_than_request {
            use super::*;

            #[test]
            fn sends_yield_requested_to_that_holder() {
                let mut f = fixture();
                let lease = f.held_interruptibly();
                f.at(T0 + MIN_MS);
                f.queue(&caller("b"), req("board", 10 * MIN, Priority::High));
                let n = f.notices_of(&holder("a"), NoticeKind::YieldRequested);
                assert_eq!(n.len(), 1);
                assert_eq!(n[0].lease_id.as_ref(), Some(&lease));
                assert_eq!(n[0].deadline, Some(T0 + MIN_MS + WINDOW_MS));
                assert_eq!(n[0].other_purpose.as_deref(), Some("High job"));
            }

            #[test]
            fn request_waits_at_head() {
                let mut f = fixture();
                f.held_interruptibly();
                let (_, position) = f.queue(&caller("b"), req("board", 10 * MIN, Priority::High));
                assert_eq!(position, 1);
                assert_eq!(f.queued(), 1);
            }

            #[test]
            fn audits_the_yield_request() {
                let mut f = fixture();
                let lease = f.held_interruptibly();
                let (request, _) = f.queue(&caller("b"), req("board", 10 * MIN, Priority::High));
                let entries = f.audit("yield_requested");
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].detail["lease"], lease.as_str());
                assert_eq!(entries[0].detail["request"], request.as_str());
            }
        }

        mod on_equal_priority_and_request_duration_below_holder_remaining_time {
            use super::*;

            #[test]
            fn sends_yield_requested() {
                let mut f = fixture();
                f.held_interruptibly();
                f.queue(&caller("b"), req("board", 10 * MIN, Priority::Normal));
                assert_eq!(f.count(&holder("a"), NoticeKind::YieldRequested), 1);
            }
        }

        mod on_equal_priority_and_request_duration_not_below_remaining_time {
            use super::*;

            #[test]
            fn sends_no_yield_request() {
                let mut f = fixture();
                f.held_interruptibly();
                f.at(T0 + 50 * MIN_MS);
                f.queue(&caller("b"), req("board", 10 * MIN, Priority::Normal));
                assert_eq!(f.count(&holder("a"), NoticeKind::YieldRequested), 0);
            }
        }

        mod on_request_of_lower_priority_than_holder {
            use super::*;

            #[test]
            fn sends_no_yield_request() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                f.grant(&caller("a"), ireq("board", 60 * MIN, Priority::High));
                f.queue(&caller("b"), req("board", MIN, Priority::Normal));
                assert_eq!(f.count(&holder("a"), NoticeKind::YieldRequested), 0);
            }
        }

        mod on_several_interruptible_holders {
            use super::*;

            #[test]
            fn asks_the_one_with_most_remaining_time() {
                let mut f = fixture();
                f.seed("board", &["u1", "u2"]);
                f.grant(&caller("a"), ireq("board", 30 * MIN, Priority::Normal));
                f.grant(&caller("c"), ireq("board", 60 * MIN, Priority::Normal));
                f.queue(&caller("b"), req("board", 10 * MIN, Priority::High));
                assert_eq!(f.count(&holder("a"), NoticeKind::YieldRequested), 0);
                assert_eq!(f.count(&holder("c"), NoticeKind::YieldRequested), 1);
            }

            #[test]
            fn on_equal_remaining_time_asks_the_lowest_priority() {
                let mut f = fixture();
                f.seed("board", &["u1", "u2"]);
                f.grant(&caller("a"), ireq("board", 60 * MIN, Priority::Normal));
                f.grant(&caller("c"), ireq("board", 60 * MIN, Priority::Low));
                f.queue(&caller("b"), req("board", 10 * MIN, Priority::High));
                assert_eq!(f.count(&holder("a"), NoticeKind::YieldRequested), 0);
                assert_eq!(f.count(&holder("c"), NoticeKind::YieldRequested), 1);
            }

            #[test]
            fn asks_only_one_holder_at_a_time() {
                let mut f = fixture();
                f.seed("board", &["u1", "u2"]);
                f.grant(&caller("a"), ireq("board", 30 * MIN, Priority::Normal));
                f.grant(&caller("c"), ireq("board", 60 * MIN, Priority::Normal));
                f.queue(&caller("b"), req("board", 10 * MIN, Priority::High));
                let asked = f.count(&holder("a"), NoticeKind::YieldRequested)
                    + f.count(&holder("c"), NoticeKind::YieldRequested);
                assert_eq!(asked, 1);
            }
        }

        mod on_no_interruptible_holder {
            use super::*;

            #[test]
            fn plain_queue() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                f.grant(&caller("a"), req("board", 60 * MIN, Priority::Low));
                let (_, position) = f.queue(&caller("b"), req("board", MIN, Priority::High));
                assert_eq!(position, 1);
                assert_eq!(f.count(&holder("a"), NoticeKind::YieldRequested), 0);
                assert_eq!(f.audit("yield_requested").len(), 0);
            }
        }

        mod on_holder_already_asked_for_another_request {
            use super::*;

            #[test]
            fn is_not_asked_twice() {
                let mut f = fixture();
                f.held_interruptibly();
                f.queue(&caller("b"), req("board", 10 * MIN, Priority::High));
                f.queue(&caller("c"), req("board", 10 * MIN, Priority::High));
                assert_eq!(f.count(&holder("a"), NoticeKind::YieldRequested), 1);
            }
        }
    }

    mod holder_answers {
        use super::*;

        mod on_confirm_yielded {
            use super::*;

            #[test]
            fn holder_lease_is_paused() {
                let mut f = fixture();
                let (paused, _) = f.lent();
                assert_eq!(f.state(&paused), "paused");
            }

            #[test]
            fn stores_the_remaining_time() {
                let mut f = fixture();
                let (paused, _) = f.lent();
                assert_eq!(f.column::<i64>(&paused, "paused_remaining_ms"), 50 * MIN_MS);
            }

            #[test]
            fn stops_the_clock() {
                let mut f = fixture();
                let (paused, borrowed) = f.lent();
                f.claim(&caller("b"), &borrowed);
                f.extend(&caller("b"), &borrowed, T0 + 200 * MIN_MS);
                // Far past the paused lease's original end (T0 + 60 min).
                f.at(T0 + 90 * MIN_MS);
                f.tick();
                assert_eq!(f.state(&paused), "paused");
                assert_eq!(f.count(&holder("a"), NoticeKind::ExpiryWarning), 0);
                assert_eq!(f.count(&holder("a"), NoticeKind::CleanupWindowStarted), 0);
            }

            #[test]
            fn offers_the_unit_to_the_requester_as_borrower() {
                let mut f = fixture();
                let (_, borrowed) = f.lent();
                assert_eq!(f.state(&borrowed), "offered");
                let n = f.notices_of(&holder("b"), NoticeKind::Offered);
                assert_eq!(n.len(), 1);
                assert_eq!(n[0].deadline, Some(T0 + 10 * MIN_MS + 5 * MIN_MS));
                assert_eq!(f.queued(), 0);
            }

            #[test]
            fn links_borrower_and_paused_leases() {
                let mut f = fixture();
                let (paused, borrowed) = f.lent();
                let paused_row = f.lease_row(&paused);
                let borrowed_row = f.lease_row(&borrowed);
                assert_eq!(f.column::<i64>(&borrowed, "paused_for_lease"), paused_row);
                assert_eq!(f.column::<i64>(&paused, "borrower_lease"), borrowed_row);
            }

            #[test]
            fn audits_the_yield() {
                let mut f = fixture();
                let (paused, borrowed) = f.lent();
                let entries = f.audit("yielded");
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].actor, holder("a"));
                assert_eq!(entries[0].detail["lease"], paused.as_str());
                assert_eq!(entries[0].detail["borrower_lease"], borrowed.as_str());
            }
        }

        mod on_confirm_yielded_without_a_yield_request {
            use super::*;

            #[test]
            fn returns_bad_state() {
                let mut f = fixture();
                let lease = f.held_interruptibly();
                let err = f
                    .confirm(&caller("a"), &lease, Confirm::Yielded)
                    .unwrap_err();
                assert!(matches!(err, SchedulerError::BadState { .. }));
                assert_eq!(f.state(&lease), "held");
            }
        }

        mod on_confirm_yielded_by_someone_else {
            use super::*;

            #[test]
            fn returns_not_holder() {
                let mut f = fixture();
                let lease = f.held_interruptibly();
                f.queue(&caller("b"), req("board", 10 * MIN, Priority::High));
                let err = f
                    .confirm(&caller("b"), &lease, Confirm::Yielded)
                    .unwrap_err();
                assert!(matches!(err, SchedulerError::NotHolder));
            }
        }

        mod on_confirm_yielded_after_the_yield_window {
            use super::*;

            #[test]
            fn returns_bad_state() {
                let mut f = fixture();
                let lease = f.held_interruptibly();
                f.queue(&caller("b"), req("board", 10 * MIN, Priority::High));
                f.at(T0 + WINDOW_MS);
                let err = f
                    .confirm(&caller("a"), &lease, Confirm::Yielded)
                    .unwrap_err();
                assert!(matches!(err, SchedulerError::BadState { .. }));
                assert_eq!(f.state(&lease), "held");
            }
        }

        mod on_refuse_yield {
            use super::*;

            fn refused() -> Fixture {
                let mut f = fixture();
                f.seed("board", &["u1", "u2"]);
                let first = f
                    .grant(&caller("c"), ireq("board", 60 * MIN, Priority::Normal))
                    .lease_id;
                f.grant(&caller("a"), ireq("board", 30 * MIN, Priority::Normal));
                f.queue(&caller("b"), req("board", 10 * MIN, Priority::High));
                f.refuse(&caller("c"), &first).unwrap();
                f
            }

            #[test]
            fn asks_the_next_interruptible_holder() {
                let mut f = refused();
                assert_eq!(f.count(&holder("a"), NoticeKind::YieldRequested), 1);
            }

            #[test]
            fn refusing_holder_keeps_its_lease() {
                let mut f = refused();
                let lease = f.notices_of(&holder("c"), NoticeKind::YieldRequested)[0]
                    .lease_id
                    .clone()
                    .unwrap();
                assert_eq!(f.state(&lease), "held");
                assert_eq!(f.column::<Option<i64>>(&lease, "yield_deadline"), None);
            }

            #[test]
            fn audits_the_refusal() {
                let mut f = refused();
                let entries = f.audit("yield_refused");
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].actor, holder("c"));
            }

            #[test]
            fn refusing_holder_is_not_asked_again_for_that_request() {
                let mut f = refused();
                let second = f.notices_of(&holder("a"), NoticeKind::YieldRequested)[0]
                    .lease_id
                    .clone()
                    .unwrap();
                f.refuse(&caller("a"), &second).unwrap();
                assert_eq!(f.count(&holder("c"), NoticeKind::YieldRequested), 1);
                assert_eq!(f.count(&holder("a"), NoticeKind::YieldRequested), 1);
            }

            #[test]
            fn request_stays_at_head_once_every_holder_refused() {
                let mut f = refused();
                let second = f.notices_of(&holder("a"), NoticeKind::YieldRequested)[0]
                    .lease_id
                    .clone()
                    .unwrap();
                f.refuse(&caller("a"), &second).unwrap();
                let (_, position) = f.queue(&caller("d"), req("board", 10 * MIN, Priority::Low));
                assert_eq!(f.queued(), 2);
                assert_eq!(position, 2);
            }
        }

        mod on_refuse_yield_without_a_yield_request {
            use super::*;

            #[test]
            fn returns_bad_state() {
                let mut f = fixture();
                let lease = f.held_interruptibly();
                let err = f.refuse(&caller("a"), &lease).unwrap_err();
                assert!(matches!(err, SchedulerError::BadState { .. }));
            }
        }

        mod on_yield_window_passing {
            use super::*;

            fn timed_out() -> Fixture {
                let mut f = fixture();
                f.seed("board", &["u1", "u2"]);
                f.grant(&caller("c"), ireq("board", 60 * MIN, Priority::Normal));
                f.grant(&caller("a"), ireq("board", 30 * MIN, Priority::Normal));
                f.queue(&caller("b"), req("board", 10 * MIN, Priority::High));
                f.at(T0 + WINDOW_MS);
                f.tick();
                f
            }

            #[test]
            fn asks_the_next_interruptible_holder() {
                let mut f = timed_out();
                let n = f.notices_of(&holder("a"), NoticeKind::YieldRequested);
                assert_eq!(n.len(), 1);
                assert_eq!(n[0].deadline, Some(T0 + 2 * WINDOW_MS));
            }

            #[test]
            fn audits_the_timeout() {
                let mut f = timed_out();
                assert_eq!(f.audit("yield_timed_out").len(), 1);
            }

            #[test]
            fn tick_reports_the_yield_deadline() {
                let mut f = fixture();
                f.held_interruptibly();
                f.queue(&caller("b"), req("board", 10 * MIN, Priority::High));
                assert_eq!(f.tick(), Some(T0 + WINDOW_MS));
            }

            #[test]
            fn request_stays_queued_once_every_holder_timed_out() {
                let mut f = timed_out();
                f.at(T0 + 2 * WINDOW_MS);
                f.tick();
                assert_eq!(f.queued(), 1);
                assert_eq!(f.count(&holder("c"), NoticeKind::YieldRequested), 1);
                assert_eq!(f.count(&holder("a"), NoticeKind::YieldRequested), 1);
            }
        }

        mod on_asked_holder_releasing_instead {
            use super::*;

            #[test]
            fn ask_lapses_and_requester_gets_the_unit_after_cleanup() {
                let mut f = fixture();
                let lease = f.held_interruptibly();
                f.queue(&caller("b"), req("board", 10 * MIN, Priority::High));
                f.release(&caller("a"), &lease);
                assert_eq!(f.column::<Option<i64>>(&lease, "yield_deadline"), None);
                f.confirm(&caller("a"), &lease, Confirm::Clean).unwrap();
                assert_eq!(f.count(&holder("b"), NoticeKind::Offered), 1);
            }
        }

        mod yield_withdrawn {
            use super::*;

            fn withdrawn(f: &mut Fixture, lease: &LeaseId) {
                let n = f.notices_of(&holder("a"), NoticeKind::YieldWithdrawn);
                assert_eq!(n.len(), 1);
                assert_eq!(n[0].lease_id.as_ref(), Some(lease));
                assert_eq!(f.column::<Option<i64>>(lease, "yield_deadline"), None);
            }

            #[test]
            fn on_request_cancelled() {
                let mut f = fixture();
                let lease = f.held_interruptibly();
                let (request, _) = f.queue(&caller("b"), req("board", 10 * MIN, Priority::High));
                f.cancel(&caller("b"), &request);
                withdrawn(&mut f, &lease);
            }

            #[test]
            fn on_request_served_by_another_unit() {
                let mut f = fixture();
                f.seed("board", &["u1", "u2"]);
                let lease = f
                    .grant(&caller("a"), ireq("board", 60 * MIN, Priority::Normal))
                    .lease_id;
                let other = f
                    .grant(&caller("c"), req("board", 60 * MIN, Priority::Normal))
                    .lease_id;
                f.queue(&caller("b"), req("board", 10 * MIN, Priority::High));
                f.release(&caller("c"), &other);
                f.confirm(&caller("c"), &other, Confirm::Clean).unwrap();
                assert_eq!(f.count(&holder("b"), NoticeKind::Offered), 1);
                withdrawn(&mut f, &lease);
            }

            #[test]
            fn on_set_interruptible_false() {
                let mut f = fixture();
                let lease = f.held_interruptibly();
                f.queue(&caller("b"), req("board", 10 * MIN, Priority::High));
                f.set_interruptible(&caller("a"), &lease, false).unwrap();
                withdrawn(&mut f, &lease);
            }

            #[test]
            fn on_request_dropped_by_the_retiring_sweep() {
                let mut f = fixture();
                let lease = f.held_interruptibly();
                f.queue(&caller("b"), req("board", 10 * MIN, Priority::High));
                f.retire("board", "u1");
                assert_eq!(f.queued(), 0);
                withdrawn(&mut f, &lease);
            }

            #[test]
            fn not_sent_on_refusal() {
                let mut f = fixture();
                let lease = f.held_interruptibly();
                f.queue(&caller("b"), req("board", 10 * MIN, Priority::High));
                f.refuse(&caller("a"), &lease).unwrap();
                assert_eq!(f.count(&holder("a"), NoticeKind::YieldWithdrawn), 0);
            }
        }

        mod on_confirm_yielded_past_the_lease_end {
            use super::*;

            #[test]
            fn returns_bad_state_naming_the_held_state() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let lease = f
                    .grant(&caller("a"), ireq("board", MIN, Priority::Normal))
                    .lease_id;
                f.queue(&caller("b"), req("board", MIN, Priority::High));
                f.at(T0 + MIN_MS);
                let err = f
                    .confirm(&caller("a"), &lease, Confirm::Yielded)
                    .unwrap_err();
                assert!(matches!(
                    err,
                    SchedulerError::BadState {
                        state: LeaseState::Held
                    }
                ));
            }
        }

        mod on_request_leaving_the_queue_while_asking {
            use super::*;

            #[test]
            fn withdraws_the_yield_request() {
                let mut f = fixture();
                let lease = f.held_interruptibly();
                let (request, _) = f.queue(&caller("b"), req("board", 10 * MIN, Priority::High));
                f.store
                    .tx(|tx| f.sched.cancel(tx, &caller("b"), &request))
                    .unwrap();
                assert_eq!(f.column::<Option<i64>>(&lease, "yield_deadline"), None);
                let err = f
                    .confirm(&caller("a"), &lease, Confirm::Yielded)
                    .unwrap_err();
                assert!(matches!(err, SchedulerError::BadState { .. }));
            }
        }
    }

    mod borrower_finishes {
        use super::*;

        mod on_borrower_release_and_confirmed_cleanup {
            use super::*;

            /// Borrow for 30 min (T0 + 10 → T0 + 40), then release and
            /// confirm at T0 + 41.
            fn returned() -> (Fixture, LeaseId) {
                let mut f = fixture();
                let (paused, borrowed) = f.borrowed();
                f.extend(&caller("b"), &borrowed, T0 + 45 * MIN_MS);
                f.at(T0 + 40 * MIN_MS);
                f.release(&caller("b"), &borrowed);
                f.at(T0 + 41 * MIN_MS);
                f.confirm(&caller("b"), &borrowed, Confirm::Clean).unwrap();
                (f, paused)
            }

            #[test]
            fn unit_returns_to_the_paused_holder() {
                let (mut f, paused) = returned();
                assert_eq!(f.state(&paused), "held");
            }

            #[test]
            fn paused_clock_preserves_remaining_time_exactly_across_a_30_minute_borrow() {
                let (mut f, paused) = returned();
                // 50 min were left at the yield (T0 + 10 min).
                assert_eq!(
                    f.column::<i64>(&paused, "ends_at"),
                    T0 + 41 * MIN_MS + 50 * MIN_MS
                );
            }

            #[test]
            fn re_arms_the_expiry_warning() {
                let (mut f, paused) = returned();
                assert!(!f.column::<bool>(&paused, "warning_sent"));
                // Lead is 5 min for a 60 min lease.
                f.at(T0 + 86 * MIN_MS);
                f.tick();
                assert_eq!(f.count(&holder("a"), NoticeKind::ExpiryWarning), 1);
            }

            #[test]
            fn publishes_unit_returned_with_cleanup_confirmed() {
                let (mut f, paused) = returned();
                let n = f.notices_of(&holder("a"), NoticeKind::UnitReturned);
                assert_eq!(n.len(), 1);
                assert_eq!(n[0].lease_id.as_ref(), Some(&paused));
                assert_eq!(n[0].cleanup_confirmed, Some(true));
                assert_eq!(n[0].dirty, None);
                assert_eq!(n[0].deadline, Some(T0 + 91 * MIN_MS));
            }

            #[test]
            fn returns_before_any_queue() {
                let mut f = fixture();
                let (paused, borrowed) = f.borrowed();
                f.queue(&caller("w"), req("board", MIN, Priority::High));
                f.release(&caller("b"), &borrowed);
                f.confirm(&caller("b"), &borrowed, Confirm::Clean).unwrap();
                assert_eq!(f.state(&paused), "held");
                assert_eq!(f.count(&holder("w"), NoticeKind::Offered), 0);
                assert_eq!(f.queued(), 1);
            }

            #[test]
            fn clears_the_pause_links() {
                let (mut f, paused) = returned();
                assert_eq!(f.column::<Option<i64>>(&paused, "borrower_lease"), None);
                assert_eq!(
                    f.column::<Option<i64>>(&paused, "paused_remaining_ms"),
                    None
                );
            }
        }

        mod on_borrower_release_without_confirmed_cleanup {
            use super::*;

            fn returned_dirty() -> (Fixture, LeaseId) {
                let mut f = fixture();
                let (paused, borrowed) = f.borrowed();
                f.at(T0 + 15 * MIN_MS);
                f.release(&caller("b"), &borrowed);
                f.at(T0 + 15 * MIN_MS + WINDOW_MS);
                f.tick();
                (f, paused)
            }

            #[test]
            fn unit_returns_to_the_paused_holder() {
                let (mut f, paused) = returned_dirty();
                assert_eq!(f.state(&paused), "held");
                assert_eq!(
                    f.column::<i64>(&paused, "ends_at"),
                    T0 + 15 * MIN_MS + WINDOW_MS + 50 * MIN_MS
                );
            }

            #[test]
            fn unit_returned_tells_the_holder_it_is_dirty() {
                let (mut f, _) = returned_dirty();
                let n = f.notices_of(&holder("a"), NoticeKind::UnitReturned);
                assert_eq!(n.len(), 1);
                assert_eq!(n[0].cleanup_confirmed, Some(false));
                let dirty = n[0].dirty.as_ref().expect("dirty record");
                assert_eq!(dirty.reason, DirtyReason::WindowPassed);
                assert_eq!(dirty.previous_holder, holder("b"));
            }

            #[test]
            fn dirty_flag_moves_to_the_returning_holder() {
                let (mut f, _) = returned_dirty();
                assert!(!f.unit_dirty("board", "u1"));
            }
        }

        mod on_borrower_offer_lapsing {
            use super::*;

            #[test]
            fn unit_returns_to_the_paused_holder() {
                let mut f = fixture();
                let (paused, borrowed) = f.lent();
                f.at(T0 + 10 * MIN_MS + 5 * MIN_MS + 1);
                f.tick();
                assert_eq!(f.state(&borrowed), "ended");
                assert_eq!(f.state(&paused), "held");
                assert_eq!(
                    f.column::<i64>(&paused, "ends_at"),
                    T0 + 15 * MIN_MS + 1 + 50 * MIN_MS
                );
                let n = f.notices_of(&holder("a"), NoticeKind::UnitReturned);
                assert_eq!(n.len(), 1);
                assert_eq!(n[0].cleanup_confirmed, Some(true));
            }
        }

        mod on_borrower_offer_ended_unclaimed {
            use super::*;

            #[test]
            fn unit_returns_to_the_paused_holder() {
                let mut f = fixture();
                let (paused, borrowed) = f.lent();
                f.end_lease(&borrowed, EndCause::Unclaimed, false);
                assert_eq!(f.state(&paused), "held");
                assert_eq!(f.count(&holder("a"), NoticeKind::UnitReturned), 1);
            }
        }
    }

    mod borrower_expires {
        use super::*;

        #[test]
        fn unit_returns_to_the_paused_holder_marked_dirty() {
            let mut f = fixture();
            let (paused, borrowed) = f.borrowed();
            f.at(T0 + 20 * MIN_MS);
            f.tick();
            assert_eq!(f.state(&borrowed), "cleaning");
            f.at(T0 + 20 * MIN_MS + WINDOW_MS);
            f.tick();
            assert_eq!(f.state(&borrowed), "ended");
            assert_eq!(f.state(&paused), "held");
            assert_eq!(
                f.column::<i64>(&paused, "ends_at"),
                T0 + 20 * MIN_MS + WINDOW_MS + 50 * MIN_MS
            );
            let n = f.notices_of(&holder("a"), NoticeKind::UnitReturned);
            assert_eq!(n.len(), 1);
            assert_eq!(n[0].cleanup_confirmed, Some(false));
            assert_eq!(n[0].dirty.as_ref().unwrap().reason, DirtyReason::Expired);
        }
    }

    mod visibility {
        use super::*;

        /// `a` (no project) holds user-scope `board` interruptibly; `p` in
        /// project /p asks through its linked project resource `pboard`.
        fn asked_across_projects() -> (Fixture, LeaseId) {
            let mut f = fixture();
            let p = caller_in("p", Some("/p"));
            f.seed_keyed(&caller("admin"), "board", &Scope::User, &["u1"], "k");
            f.seed_keyed(
                &p,
                "pboard",
                &Scope::Project(ProjectKey(PathBuf::from("/p"))),
                &["u1"],
                "k",
            );
            let lease = f
                .grant(&caller("a"), ireq("board", 60 * MIN, Priority::Normal))
                .lease_id;
            f.at(T0 + 10 * MIN_MS);
            f.queue(&p, req("pboard", 10 * MIN, Priority::High));
            (f, lease)
        }

        #[test]
        fn yield_requested_hides_an_invisible_requesters_purpose() {
            let (mut f, _) = asked_across_projects();
            let n = f.notices_of(&holder("a"), NoticeKind::YieldRequested);
            assert_eq!(n.len(), 1);
            assert_eq!(n[0].other_purpose, None);
        }

        #[test]
        fn borrower_overran_hides_an_invisible_borrowers_purpose() {
            let (mut f, lease) = asked_across_projects();
            let p = caller_in("p", Some("/p"));
            f.confirm(&caller("a"), &lease, Confirm::Yielded).unwrap();
            let borrowed = f.offered_lease(&holder("p"));
            f.claim(&p, &borrowed);
            f.extend(&p, &borrowed, T0 + 40 * MIN_MS);
            f.at(T0 + 21 * MIN_MS);
            f.tick();
            let n = f.notices_of(&holder("a"), NoticeKind::BorrowerOverran);
            assert_eq!(n.len(), 1);
            assert_eq!(n[0].other_purpose, None);
        }
    }

    mod tick_idempotence {
        use super::*;

        #[test]
        fn second_tick_with_yield_and_overrun_due_at_the_same_instant_does_nothing() {
            let mut f = fixture();
            f.seed("board", &["u1", "u2"]);
            let paused = f
                .grant(&caller("a"), ireq("board", 60 * MIN, Priority::Normal))
                .lease_id;
            f.grant(&caller("c"), ireq("board", 60 * MIN, Priority::Normal));
            f.at(T0 + 10 * MIN_MS);
            f.queue(&caller("b"), req("board", 10 * MIN, Priority::High));
            f.confirm(&caller("a"), &paused, Confirm::Yielded).unwrap();
            let borrowed = f.offered_lease(&holder("b"));
            f.claim(&caller("b"), &borrowed);
            f.extend(&caller("b"), &borrowed, T0 + 40 * MIN_MS);
            // d's ask to c has its deadline at T0 + 20 min, b's due time.
            f.at(T0 + 18 * MIN_MS);
            f.queue(&caller("d"), req("board", 10 * MIN, Priority::High));
            assert_eq!(f.count(&holder("c"), NoticeKind::YieldRequested), 1);
            f.at(T0 + 20 * MIN_MS);
            let first = f.tick();
            assert_eq!(f.count(&holder("a"), NoticeKind::BorrowerOverran), 1);
            assert_eq!(f.audit("yield_timed_out").len(), 1);
            let before: Vec<usize> = ["a", "b", "c", "d"]
                .iter()
                .map(|h| f.total_notices(&holder(h)))
                .collect();
            let audits = f.store.tx(audit::all).unwrap().len();
            let second = f.tick();
            let after: Vec<usize> = ["a", "b", "c", "d"]
                .iter()
                .map(|h| f.total_notices(&holder(h)))
                .collect();
            assert_eq!(before, after);
            assert_eq!(f.store.tx(audit::all).unwrap().len(), audits);
            assert_eq!(first, second);
        }
    }

    mod borrower_overruns {
        use super::*;

        #[test]
        fn paused_holder_gets_borrower_overran_once() {
            let mut f = fixture();
            let (paused, borrowed) = f.overran();
            f.at(T0 + 25 * MIN_MS);
            f.tick();
            let n = f.notices_of(&holder("a"), NoticeKind::BorrowerOverran);
            assert_eq!(n.len(), 1);
            assert_eq!(n[0].lease_id.as_ref(), Some(&paused));
            assert_eq!(n[0].deadline, Some(T0 + 20 * MIN_MS));
            assert_eq!(f.state(&borrowed), "held");
        }

        #[test]
        fn no_overran_notice_within_the_declared_duration() {
            let mut f = fixture();
            let (_, borrowed) = f.borrowed();
            f.extend(&caller("b"), &borrowed, T0 + 40 * MIN_MS);
            f.at(T0 + 20 * MIN_MS - 1);
            f.tick();
            assert_eq!(f.count(&holder("a"), NoticeKind::BorrowerOverran), 0);
        }

        mod on_reclaim_by_paused_holder {
            use super::*;

            #[test]
            fn borrower_lease_enters_cleaning_with_cause_reclaimed() {
                let mut f = fixture();
                let (paused, borrowed) = f.overran();
                f.reclaim(&caller("a"), &paused).unwrap();
                assert_eq!(f.state(&borrowed), "cleaning");
                assert_eq!(f.column::<String>(&borrowed, "cleanup_cause"), "reclaimed");
                let n = f.notices_of(&holder("b"), NoticeKind::CleanupWindowStarted);
                assert_eq!(
                    n.last().unwrap().deadline,
                    Some(T0 + 21 * MIN_MS + WINDOW_MS)
                );
            }

            #[test]
            fn audits_the_reclaim() {
                let mut f = fixture();
                let (paused, _) = f.overran();
                f.reclaim(&caller("a"), &paused).unwrap();
                let entries = f.audit("reclaimed");
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].actor, holder("a"));
            }

            #[test]
            fn then_unit_returns() {
                let mut f = fixture();
                let (paused, borrowed) = f.overran();
                f.reclaim(&caller("a"), &paused).unwrap();
                f.at(T0 + 21 * MIN_MS + WINDOW_MS);
                f.tick();
                assert_eq!(f.state(&borrowed), "ended");
                assert_eq!(f.column::<String>(&borrowed, "end_cause"), "reclaimed");
                assert_eq!(f.state(&paused), "held");
                let n = f.notices_of(&holder("a"), NoticeKind::UnitReturned);
                assert_eq!(n[0].cleanup_confirmed, Some(false));
            }
        }

        mod on_reclaim_before_overrun {
            use super::*;

            #[test]
            fn returns_borrower_not_overrun() {
                let mut f = fixture();
                let (paused, borrowed) = f.borrowed();
                let err = f.reclaim(&caller("a"), &paused).unwrap_err();
                assert!(matches!(
                    err,
                    SchedulerError::BorrowerNotOverrun { due_at } if due_at == Some(T0 + 20 * MIN_MS)
                ));
                assert_eq!(f.state(&borrowed), "held");
            }

            #[test]
            fn returns_borrower_not_overrun_while_offered() {
                let mut f = fixture();
                let (paused, _) = f.lent();
                let err = f.reclaim(&caller("a"), &paused).unwrap_err();
                assert!(matches!(err, SchedulerError::BorrowerNotOverrun { .. }));
            }
        }

        mod on_reclaim_by_someone_else {
            use super::*;

            #[test]
            fn returns_not_holder() {
                let mut f = fixture();
                let (paused, _) = f.overran();
                let err = f.reclaim(&caller("b"), &paused).unwrap_err();
                assert!(matches!(err, SchedulerError::NotHolder));
            }
        }

        mod on_reclaim_of_a_lease_that_is_not_paused {
            use super::*;

            #[test]
            fn returns_bad_state() {
                let mut f = fixture();
                let lease = f.held_interruptibly();
                let err = f.reclaim(&caller("a"), &lease).unwrap_err();
                assert!(matches!(err, SchedulerError::BadState { .. }));
            }
        }
    }

    mod double_grant_guard {
        use super::*;

        #[test]
        fn third_party_cannot_get_a_group_with_paused_owner_and_offered_borrower() {
            let mut f = fixture();
            f.lent();
            f.queue(&caller("c"), req("board", MIN, Priority::High));
        }

        #[test]
        fn third_party_cannot_get_a_group_with_paused_owner_and_held_borrower() {
            let mut f = fixture();
            f.borrowed();
            f.queue(&caller("c"), req("board", MIN, Priority::High));
            f.tick();
            assert_eq!(f.count(&holder("c"), NoticeKind::Offered), 0);
        }

        #[test]
        fn third_party_cannot_get_a_linked_unit_through_another_resource() {
            let mut f = fixture();
            let p = caller_in("p", Some("/p"));
            f.seed_keyed(&caller("admin"), "board", &Scope::User, &["u1"], "k");
            f.seed_keyed(
                &p,
                "pboard",
                &Scope::Project(ProjectKey(PathBuf::from("/p"))),
                &["u1"],
                "k",
            );
            let paused = f
                .grant(&caller("a"), ireq("board", 60 * MIN, Priority::Normal))
                .lease_id;
            f.queue(&caller("b"), req("board", 10 * MIN, Priority::High));
            f.confirm(&caller("a"), &paused, Confirm::Yielded).unwrap();
            let borrowed = f.offered_lease(&holder("b"));
            f.claim(&caller("b"), &borrowed);
            f.queue(&p, req("pboard", MIN, Priority::High));
            assert_eq!(f.count(&holder("p"), NoticeKind::Offered), 0);
        }

        #[test]
        fn paused_holder_cannot_be_asked_to_yield_again() {
            let mut f = fixture();
            f.borrowed();
            f.queue(&caller("c"), req("board", MIN, Priority::High));
            assert_eq!(f.count(&holder("a"), NoticeKind::YieldRequested), 1);
            assert_eq!(f.count(&holder("b"), NoticeKind::YieldRequested), 0);
        }

        #[test]
        fn borrower_is_never_asked_to_yield() {
            let mut f = fixture();
            let paused = f.held_interruptibly();
            f.queue(&caller("b"), ireq("board", 10 * MIN, Priority::Normal));
            f.confirm(&caller("a"), &paused, Confirm::Yielded).unwrap();
            let borrowed = f.offered_lease(&holder("b"));
            f.claim(&caller("b"), &borrowed);
            f.queue(&caller("c"), req("board", MIN, Priority::High));
            assert_eq!(f.count(&holder("b"), NoticeKind::YieldRequested), 0);
        }
    }

    mod override_ {
        use super::*;

        mod on_acquire_with_override_reason_against_non_interruptible_holder {
            use super::*;

            #[test]
            fn holder_lease_enters_cleaning_with_cause_override() {
                let mut f = fixture();
                let (lease, _) = f.overridden();
                assert_eq!(f.state(&lease), "cleaning");
                assert_eq!(f.column::<String>(&lease, "cleanup_cause"), "override");
                assert_eq!(
                    f.column::<i64>(&lease, "cleanup_deadline"),
                    T0 + 5 * MIN_MS + WINDOW_MS
                );
            }

            #[test]
            fn holder_gets_overridden_notice() {
                let mut f = fixture();
                let (lease, _) = f.overridden();
                let n = f.notices_of(&holder("a"), NoticeKind::Overridden);
                assert_eq!(n.len(), 1);
                assert_eq!(n[0].lease_id.as_ref(), Some(&lease));
                assert_eq!(n[0].deadline, Some(T0 + 5 * MIN_MS + WINDOW_MS));
            }

            #[test]
            fn audits_override_with_reason_and_actor() {
                let mut f = fixture();
                let (lease, request) = f.overridden();
                let entries = f.audit("override");
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].actor, holder("o"));
                assert_eq!(entries[0].detail["reason"], "user said so");
                assert_eq!(entries[0].detail["lease"], lease.as_str());
                assert_eq!(entries[0].detail["request"], request.as_str());
            }

            #[test]
            fn targets_the_lease_that_started_earliest() {
                let mut f = fixture();
                f.seed("board", &["u1", "u2"]);
                let early = f
                    .grant(&caller("a"), req("board", 60 * MIN, Priority::Normal))
                    .lease_id;
                f.at(T0 + MIN_MS);
                let late = f
                    .grant(&caller("c"), req("board", 60 * MIN, Priority::Normal))
                    .lease_id;
                f.queue(&caller("o"), override_req("board", "now"));
                assert_eq!(f.state(&early), "cleaning");
                assert_eq!(f.state(&late), "held");
            }

            #[test]
            fn overrider_is_at_the_head_of_the_queue() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                f.grant(&caller("a"), req("board", 60 * MIN, Priority::Normal));
                f.queue(&caller("w"), req("board", MIN, Priority::High));
                let (_, position) = f.queue(&caller("o"), override_req("board", "now"));
                assert_eq!(position, 1);
            }

            #[test]
            fn after_the_window_unit_is_offered_to_overrider_ahead_of_other_waiters() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                f.grant(&caller("a"), req("board", 60 * MIN, Priority::Normal));
                f.queue(&caller("w"), req("board", MIN, Priority::High));
                f.queue(&caller("o"), override_req("board", "now"));
                f.at(T0 + WINDOW_MS);
                f.tick();
                assert_eq!(f.count(&holder("o"), NoticeKind::Offered), 1);
                assert_eq!(f.count(&holder("w"), NoticeKind::Offered), 0);
            }

            #[test]
            fn overrider_is_told_the_unit_is_dirty_when_cleanup_was_unconfirmed() {
                let mut f = fixture();
                f.overridden();
                f.at(T0 + 5 * MIN_MS + WINDOW_MS);
                f.tick();
                let n = f.notices_of(&holder("o"), NoticeKind::Offered);
                assert_eq!(n.len(), 1);
                let dirty = n[0].dirty.as_ref().expect("dirty record");
                assert_eq!(dirty.reason, DirtyReason::Override);
                assert_eq!(dirty.previous_holder, holder("a"));
            }

            #[test]
            fn holder_lease_ends_rather_than_pauses() {
                let mut f = fixture();
                let (lease, _) = f.overridden();
                f.at(T0 + 5 * MIN_MS + WINDOW_MS);
                f.tick();
                assert_eq!(f.state(&lease), "ended");
                assert_eq!(f.column::<String>(&lease, "end_cause"), "override");
                assert_eq!(f.column::<Option<i64>>(&lease, "paused_remaining_ms"), None);
                assert_eq!(f.count(&holder("a"), NoticeKind::UnitReturned), 0);
            }

            #[test]
            fn on_confirm_overrider_gets_a_clean_unit() {
                let mut f = fixture();
                let (lease, _) = f.overridden();
                f.confirm(&caller("a"), &lease, Confirm::Clean).unwrap();
                assert_eq!(f.state(&lease), "ended");
                let n = f.notices_of(&holder("o"), NoticeKind::Offered);
                assert_eq!(n.len(), 1);
                assert_eq!(n[0].dirty, None);
            }
        }

        mod on_override_against_a_lent_unit {
            use super::*;

            #[test]
            fn paused_lease_is_not_overridden() {
                let mut f = fixture();
                let (paused, borrowed) = f.borrowed();
                f.queue(&caller("o"), override_req("board", "now"));
                assert_eq!(f.state(&paused), "paused");
                assert_eq!(f.state(&borrowed), "cleaning");
            }

            #[test]
            fn overrider_borrows_in_place_of_the_borrower() {
                let mut f = fixture();
                let (paused, borrowed) = f.borrowed();
                f.queue(&caller("o"), override_req("board", "now"));
                f.confirm(&caller("b"), &borrowed, Confirm::Clean).unwrap();
                let taken = f.offered_lease(&holder("o"));
                assert_eq!(f.state(&paused), "paused");
                let paused_row = f.lease_row(&paused);
                assert_eq!(f.column::<i64>(&taken, "paused_for_lease"), paused_row);
                let taken_row = f.lease_row(&taken);
                assert_eq!(f.column::<i64>(&paused, "borrower_lease"), taken_row);
            }

            #[test]
            fn unit_returns_to_the_paused_holder_after_the_overrider() {
                let mut f = fixture();
                let (paused, borrowed) = f.borrowed();
                f.queue(&caller("o"), override_req("board", "now"));
                f.confirm(&caller("b"), &borrowed, Confirm::Clean).unwrap();
                let taken = f.offered_lease(&holder("o"));
                f.claim(&caller("o"), &taken);
                f.release(&caller("o"), &taken);
                f.confirm(&caller("o"), &taken, Confirm::Clean).unwrap();
                assert_eq!(f.state(&paused), "held");
            }
        }

        mod on_override_against_a_cleaning_lease {
            use super::*;

            #[test]
            fn links_to_it_without_cleaning_it_again() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let lease = f
                    .grant(&caller("a"), req("board", 60 * MIN, Priority::Normal))
                    .lease_id;
                f.release(&caller("a"), &lease);
                let (request, position) = f.queue(&caller("o"), override_req("board", "now"));
                assert_eq!(position, 1);
                assert_eq!(f.column::<String>(&lease, "cleanup_cause"), "released");
                assert_eq!(f.column::<i64>(&lease, "cleanup_deadline"), T0 + WINDOW_MS);
                assert_eq!(f.count(&holder("a"), NoticeKind::Overridden), 0);
                let entries = f.audit("override");
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].detail["request"], request.as_str());
            }

            #[test]
            fn leaves_a_later_held_lease_alone_and_overrider_gets_the_cleaning_unit() {
                let mut f = fixture();
                f.seed("board", &["u1", "u2"]);
                let first = f
                    .grant(&caller("a"), req("board", 60 * MIN, Priority::Normal))
                    .lease_id;
                f.at(T0 + MIN_MS);
                let second = f
                    .grant(&caller("c"), req("board", 60 * MIN, Priority::Normal))
                    .lease_id;
                f.release(&caller("a"), &first);
                f.queue(&caller("o"), override_req("board", "now"));
                assert_eq!(f.state(&second), "held");
                assert_eq!(f.count(&holder("c"), NoticeKind::Overridden), 0);
                f.confirm(&caller("a"), &first, Confirm::Clean).unwrap();
                let offered = f.notices_of(&holder("o"), NoticeKind::Offered);
                assert_eq!(offered.len(), 1);
                assert_eq!(offered[0].unit.as_ref().map(UnitId::as_str), Some("u1"));
            }
        }

        mod on_paused_holder_overriding_its_own_borrower {
            use super::*;

            #[test]
            fn borrower_enters_cleaning_with_cause_reclaimed() {
                let mut f = fixture();
                let (paused, borrowed) = f.borrowed();
                let out = f.acquire(&caller("a"), override_req("board", "need it back"));
                assert_eq!(
                    out,
                    AcquireOutcome::Reclaiming {
                        lease_id: paused.clone()
                    }
                );
                assert_eq!(f.state(&borrowed), "cleaning");
                assert_eq!(f.column::<String>(&borrowed, "cleanup_cause"), "reclaimed");
                assert_eq!(f.queued(), 0);
            }

            #[test]
            fn unit_returns_without_a_self_borrow() {
                let mut f = fixture();
                let (paused, borrowed) = f.borrowed();
                f.acquire(&caller("a"), override_req("board", "need it back"));
                f.confirm(&caller("b"), &borrowed, Confirm::Clean).unwrap();
                assert_eq!(f.state(&paused), "held");
                assert_eq!(f.count(&holder("a"), NoticeKind::Offered), 0);
                assert_eq!(f.count(&holder("a"), NoticeKind::UnitReturned), 1);
            }

            #[test]
            fn audits_the_reclaim_with_the_reason() {
                let mut f = fixture();
                let (paused, _) = f.borrowed();
                f.acquire(&caller("a"), override_req("board", "need it back"));
                let entries = f.audit("reclaimed");
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].detail["lease"], paused.as_str());
                assert_eq!(entries[0].detail["reason"], "need it back");
            }
        }
    }

    mod set_interruptible {
        use super::*;

        #[test]
        fn updates_the_flag_and_audits() {
            let mut f = fixture();
            f.seed("board", &["u1"]);
            let lease = f
                .grant(&caller("a"), req("board", 60 * MIN, Priority::Normal))
                .lease_id;
            let view = f.set_interruptible(&caller("a"), &lease, true).unwrap();
            assert!(view.interruptible);
            assert!(f.column::<bool>(&lease, "interruptible"));
            assert_eq!(f.audit("interruptible_set").len(), 1);
        }

        #[test]
        fn on_true_with_a_waiting_request_asks_the_holder_to_yield() {
            let mut f = fixture();
            f.seed("board", &["u1"]);
            let lease = f
                .grant(&caller("a"), req("board", 60 * MIN, Priority::Normal))
                .lease_id;
            f.queue(&caller("b"), req("board", 10 * MIN, Priority::High));
            f.set_interruptible(&caller("a"), &lease, true).unwrap();
            assert_eq!(f.count(&holder("a"), NoticeKind::YieldRequested), 1);
        }

        #[test]
        fn on_false_with_a_pending_ask_withdraws_it_and_asks_the_next() {
            let mut f = fixture();
            f.seed("board", &["u1", "u2"]);
            let first = f
                .grant(&caller("c"), ireq("board", 60 * MIN, Priority::Normal))
                .lease_id;
            f.grant(&caller("a"), ireq("board", 30 * MIN, Priority::Normal));
            f.queue(&caller("b"), req("board", 10 * MIN, Priority::High));
            f.set_interruptible(&caller("c"), &first, false).unwrap();
            assert_eq!(f.column::<Option<i64>>(&first, "yield_deadline"), None);
            assert_eq!(f.count(&holder("a"), NoticeKind::YieldRequested), 1);
        }

        #[test]
        fn by_someone_else_returns_not_holder() {
            let mut f = fixture();
            let lease = f.held_interruptibly();
            let err = f
                .set_interruptible(&caller("b"), &lease, false)
                .unwrap_err();
            assert!(matches!(err, SchedulerError::NotHolder));
        }
    }

    mod paused_holder_gone {
        use super::*;

        #[test]
        fn release_of_a_paused_lease_forfeits_the_claim() {
            let mut f = fixture();
            let (paused, borrowed) = f.borrowed();
            let view = f.try_release(&caller("a"), &paused).unwrap();
            assert_eq!(view.state, LeaseState::Ended);
            assert!(f.column::<bool>(&paused, "cleanup_confirmed"));
            assert_eq!(f.state(&borrowed), "held");
            assert_eq!(f.column::<Option<i64>>(&borrowed, "paused_for_lease"), None);
            assert!(!f.unit_dirty("board", "u1"));
        }

        #[test]
        fn pause_claim_is_forfeited_and_unit_stays_with_borrower() {
            let mut f = fixture();
            let (paused, borrowed) = f.borrowed();
            f.end_lease(&paused, EndCause::HolderGone, false);
            assert_eq!(f.state(&paused), "ended");
            assert_eq!(f.state(&borrowed), "held");
            assert_eq!(f.column::<Option<i64>>(&borrowed, "paused_for_lease"), None);
            assert!(!f.unit_dirty("board", "u1"));
        }

        #[test]
        fn then_the_unit_follows_the_normal_queue() {
            let mut f = fixture();
            let (paused, borrowed) = f.borrowed();
            f.queue(&caller("w"), req("board", MIN, Priority::Low));
            f.end_lease(&paused, EndCause::HolderGone, false);
            f.release(&caller("b"), &borrowed);
            f.confirm(&caller("b"), &borrowed, Confirm::Clean).unwrap();
            assert_eq!(f.count(&holder("w"), NoticeKind::Offered), 1);
            assert_eq!(f.count(&holder("a"), NoticeKind::UnitReturned), 0);
        }
    }
}
