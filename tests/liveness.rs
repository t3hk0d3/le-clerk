mod common;

use std::process::{Child, Command};
use std::sync::Arc;
use std::time::Duration;

use leclerk::clock::ManualClock;
use leclerk::config::Config;
use leclerk::health::HealthResult;
use leclerk::inventory::{Inventory, NewResource, NewUnit};
use leclerk::model::{
    Caller, DirtyInfo, DirtyReason, HealthSpec, HolderRef, LeaseId, Priority, RequestId,
    ResourceId, ResourceType, Scope, UnitId, UnitSettings,
};
use leclerk::scheduler::{
    AcquireOutcome, AcquireRequest, Confirm, Grant, HealthProbe, Scheduler, Target, liveness,
};
use leclerk::store::audit::{self, AuditEntry};
use leclerk::store::notices::{self, Notice, NoticeKind};
use leclerk::store::{Store, StoreError, Tx};

const T0: i64 = 1_000_000;
const MIN_MS: i64 = 60_000;
const MIN: Duration = Duration::from_secs(60);

fn rid(s: &str) -> ResourceId {
    ResourceId::parse(s).unwrap()
}

fn uid(s: &str) -> UnitId {
    UnitId::parse(s).unwrap()
}

fn user(tag: &str) -> HolderRef {
    HolderRef::User { tag: tag.into() }
}

fn as_caller(holder: HolderRef) -> Caller {
    Caller {
        holder,
        project: None,
        override_reason: None,
    }
}

fn caller(tag: &str) -> Caller {
    as_caller(user(tag))
}

fn req(resource: &str, priority: Priority) -> AcquireRequest {
    AcquireRequest {
        target: Target::Resource(rid(resource)),
        duration: 60 * MIN,
        interruptible: false,
        priority,
        purpose: format!("{priority:?} job"),
        override_reason: None,
    }
}

fn ireq(resource: &str) -> AcquireRequest {
    AcquireRequest {
        interruptible: true,
        ..req(resource, Priority::Normal)
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
    let mut f = Fixture {
        store: Store::open_in_memory().unwrap(),
        inv: Inventory::new(clock.clone()),
        sched: Scheduler::new(clock.clone(), Config::default(), Arc::new(Healthy)),
        clock,
    };
    f.seed("board", &["u1"]);
    f
}

impl Fixture {
    fn seed(&mut self, id: &str, units: &[&str]) {
        let who = caller("admin");
        self.store
            .tx(|tx| -> Result<(), leclerk::inventory::InventoryError> {
                self.inv.create_resource(
                    tx,
                    &who,
                    &NewResource {
                        id: rid(id),
                        ty: ResourceType::Generic,
                        scope: Scope::User,
                        description: String::new(),
                        usage: String::new(),
                        health: None,
                    },
                )?;
                for u in units {
                    self.inv.add_unit(
                        tx,
                        &who,
                        &rid(id),
                        &NewUnit {
                            id: uid(u),
                            settings: UnitSettings::Generic {
                                identity_key: format!("{id}-{u}"),
                            },
                            description: String::new(),
                        },
                    )?;
                }
                Ok(())
            })
            .unwrap();
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

    fn queue(&mut self, who: &Caller, r: AcquireRequest) -> RequestId {
        match self.acquire(who, r) {
            AcquireOutcome::Queued { request_id, .. } => request_id,
            other => panic!("expected queued, got {other:?}"),
        }
    }

    fn release(&mut self, who: &Caller, lease: &LeaseId) {
        self.store
            .tx(|tx| self.sched.release(tx, who, lease))
            .unwrap();
    }

    fn claim(&mut self, who: &Caller, lease: &LeaseId) {
        self.store
            .tx(|tx| self.sched.claim(tx, who, lease))
            .unwrap();
    }

    fn confirm(&mut self, who: &Caller, lease: &LeaseId, how: Confirm) {
        self.store
            .tx(|tx| self.sched.confirm(tx, who, lease, how))
            .unwrap();
    }

    fn gone(&mut self, h: &HolderRef) {
        self.store.tx(|tx| self.sched.holder_gone(tx, h)).unwrap();
    }

    fn probe(&mut self) -> Vec<HolderRef> {
        self.store
            .tx(|tx| liveness::probe_all(tx, &self.sched))
            .unwrap()
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

    fn queued_for(&mut self, h: &HolderRef) -> usize {
        let text = serde_json::to_string(h).unwrap();
        self.with_tx(|tx| {
            tx.conn()
                .query_row(
                    "SELECT COUNT(*) FROM requests WHERE holder = ?1",
                    [text],
                    |r| r.get::<_, i64>(0),
                )
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

    fn lease_ended_audits(&mut self, lease: &LeaseId) -> usize {
        self.audit("lease_ended")
            .into_iter()
            .filter(|e| e.detail["lease"] == lease.as_str())
            .count()
    }

    fn dirty(&mut self, unit: &str) -> Option<DirtyInfo> {
        let who = caller("admin");
        self.store
            .tx(|tx| self.inv.get_resource(tx, &who, &rid("board")))
            .unwrap()
            .units
            .into_iter()
            .find(|u| u.id.as_str() == unit)
            .unwrap()
            .dirty
    }

    /// Link `a` and `b` (resource, unit) as one physical thing.
    fn link(&mut self, a: (&str, &str), b: (&str, &str)) {
        let who = caller("admin");
        let mut row = |(r, u): (&str, &str)| {
            self.store
                .tx(|tx| self.inv.get_resource(tx, &who, &rid(r)))
                .unwrap()
                .units
                .into_iter()
                .find(|x| x.id.as_str() == u)
                .unwrap()
                .row
        };
        let (a, b) = (row(a), row(b));
        self.store.tx(|tx| Inventory::link(tx, a, b)).unwrap();
    }

    /// The lease offered to `h` by its latest Offered notice.
    fn offered_lease(&mut self, h: &HolderRef) -> LeaseId {
        self.notices_of(h, NoticeKind::Offered)
            .last()
            .and_then(|n| n.lease_id.clone())
            .expect("an Offered notice with a lease id")
    }

    /// `a` holds `board/u1` interruptibly from T0 for 60 min; at T0 + 10 min
    /// `b` (high) asks, `a` yields and `b` claims. Returns (a's paused
    /// lease, b's borrowing lease).
    fn borrowed(&mut self) -> (LeaseId, LeaseId) {
        let paused = self.grant(&caller("a"), ireq("board")).lease_id;
        self.at(T0 + 10 * MIN_MS);
        let mut high = req("board", Priority::High);
        high.duration = 10 * MIN;
        self.queue(&caller("b"), high);
        self.confirm(&caller("a"), &paused, Confirm::Yielded);
        let borrowed = self.offered_lease(&user("b"));
        self.claim(&caller("b"), &borrowed);
        (paused, borrowed)
    }
}

/// A real child process (`sleep 1000`), killed and reaped on drop if the
/// test did not.
struct Sleeper(Child);

impl Sleeper {
    fn spawn() -> Self {
        Self(
            Command::new("sleep")
                .arg("1000")
                .spawn()
                .expect("spawn sleep"),
        )
    }

    fn holder(&self) -> HolderRef {
        let pid = self.0.id();
        HolderRef::CliProcess {
            pid,
            start_time: liveness::start_time(pid).expect("child start time"),
        }
    }

    /// SIGKILL and reap.
    fn kill(&mut self) {
        self.0.kill().expect("SIGKILL child");
        self.0.wait().expect("reap child");
    }
}

impl Drop for Sleeper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn self_holder() -> HolderRef {
    HolderRef::CliProcess {
        pid: std::process::id(),
        start_time: liveness::self_start_time().expect("own start time"),
    }
}

mod holder_liveness {
    use super::*;

    mod holder_gone {
        use super::*;

        mod on_holder_with_queued_requests {
            use super::*;

            /// `h` holds u1; `g` and then `w` wait for it.
            fn waiting() -> (Fixture, LeaseId) {
                let mut f = fixture();
                let lease = f
                    .grant(&caller("h"), req("board", Priority::Normal))
                    .lease_id;
                f.queue(&caller("g"), req("board", Priority::Normal));
                f.queue(&caller("w"), req("board", Priority::Normal));
                (f, lease)
            }

            #[test]
            fn requests_dropped() {
                let (mut f, _) = waiting();
                f.gone(&user("g"));
                assert_eq!(f.queued_for(&user("g")), 0);
                assert_eq!(f.queued_for(&user("w")), 1);
            }

            #[test]
            fn waiters_behind_are_told_their_new_position() {
                let (mut f, _) = waiting();
                f.gone(&user("g"));
                let moved = f.notices_of(&user("w"), NoticeKind::PositionChanged);
                assert_eq!(moved.last().and_then(|n| n.position), Some(1));
            }

            #[test]
            fn holder_asked_to_yield_for_one_is_sent_yield_withdrawn() {
                let mut f = fixture();
                let lease = f.grant(&caller("h"), ireq("board")).lease_id;
                f.queue(&caller("g"), req("board", Priority::High));
                assert_eq!(f.count(&user("h"), NoticeKind::YieldRequested), 1);
                f.gone(&user("g"));
                let withdrawn = f.notices_of(&user("h"), NoticeKind::YieldWithdrawn);
                assert_eq!(withdrawn.len(), 1);
                assert_eq!(withdrawn[0].lease_id.as_ref(), Some(&lease));
                let deadline: Option<i64> = f.column(&lease, "yield_deadline");
                assert_eq!(deadline, None);
            }

            #[test]
            fn holders_held_lease_is_untouched() {
                let (mut f, lease) = waiting();
                f.gone(&user("g"));
                assert_eq!(f.state(&lease), "held");
            }
        }

        mod on_holder_with_held_lease {
            use super::*;

            /// `g` holds u1 and `w` waits.
            fn gone() -> (Fixture, LeaseId) {
                let mut f = fixture();
                let lease = f
                    .grant(&caller("g"), req("board", Priority::Normal))
                    .lease_id;
                f.queue(&caller("w"), req("board", Priority::Normal));
                f.at(T0 + MIN_MS);
                f.gone(&user("g"));
                (f, lease)
            }

            #[test]
            fn lease_ends_at_once_with_cause_holder_gone_unconfirmed() {
                let (mut f, lease) = gone();
                assert_eq!(f.state(&lease), "ended");
                let cause: String = f.column(&lease, "end_cause");
                assert_eq!(cause, "holder_gone");
                let confirmed: bool = f.column(&lease, "cleanup_confirmed");
                assert!(!confirmed);
            }

            #[test]
            fn unit_marked_dirty_holder_gone_naming_the_holder() {
                let (mut f, _) = gone();
                // The next waiter's offer carries the dirty record.
                let offered = f.offered_lease(&user("w"));
                let carried: String = f.column(&offered, "dirty_info");
                let dirty: DirtyInfo = serde_json::from_str(&carried).unwrap();
                assert_eq!(dirty.reason, DirtyReason::HolderGone);
                assert_eq!(dirty.previous_holder, user("g"));
                assert_eq!(dirty.at, T0 + MIN_MS);
            }

            #[test]
            fn next_waiter_is_offered_the_unit() {
                let (mut f, _) = gone();
                let offered = f.offered_lease(&user("w"));
                assert_eq!(f.state(&offered), "offered");
            }

            #[test]
            fn end_is_audited_once() {
                let (mut f, lease) = gone();
                assert_eq!(f.lease_ended_audits(&lease), 1);
            }
        }

        mod on_holder_with_cleaning_lease {
            use super::*;

            #[test]
            fn lease_ends_and_unit_marked_dirty_holder_gone() {
                let mut f = fixture();
                let lease = f
                    .grant(&caller("g"), req("board", Priority::Normal))
                    .lease_id;
                f.release(&caller("g"), &lease);
                assert_eq!(f.state(&lease), "cleaning");
                f.gone(&user("g"));
                assert_eq!(f.state(&lease), "ended");
                let dirty = f.dirty("u1").expect("unit dirty");
                assert_eq!(dirty.reason, DirtyReason::HolderGone);
            }
        }

        mod on_holder_with_offered_lease {
            use super::*;

            #[test]
            fn offer_ends_and_next_waiter_is_offered() {
                let mut f = fixture();
                let lease = f
                    .grant(&caller("h"), req("board", Priority::Normal))
                    .lease_id;
                f.queue(&caller("g"), req("board", Priority::Normal));
                f.queue(&caller("w"), req("board", Priority::Normal));
                f.release(&caller("h"), &lease);
                f.confirm(&caller("h"), &lease, Confirm::Clean);
                let offer = f.offered_lease(&user("g"));
                f.gone(&user("g"));
                assert_eq!(f.state(&offer), "ended");
                let next = f.offered_lease(&user("w"));
                assert_eq!(f.state(&next), "offered");
            }
        }

        mod on_paused_holder {
            use super::*;

            #[test]
            fn paused_lease_ends_forfeiting_its_claim() {
                let mut f = fixture();
                let (paused, _) = f.borrowed();
                f.gone(&user("a"));
                assert_eq!(f.state(&paused), "ended");
                let cause: String = f.column(&paused, "end_cause");
                assert_eq!(cause, "holder_gone");
            }

            #[test]
            fn borrower_keeps_the_unit_as_an_ordinary_lease() {
                let mut f = fixture();
                let (_, borrowed) = f.borrowed();
                f.gone(&user("a"));
                assert_eq!(f.state(&borrowed), "held");
                let link: Option<i64> = f.column(&borrowed, "paused_for_lease");
                assert_eq!(link, None);
            }

            #[test]
            fn unit_is_not_marked_dirty() {
                let mut f = fixture();
                f.borrowed();
                f.gone(&user("a"));
                assert_eq!(f.dirty("u1"), None);
            }

            #[test]
            fn after_the_borrower_the_normal_queue_resumes() {
                let mut f = fixture();
                let (_, borrowed) = f.borrowed();
                f.queue(&caller("w"), req("board", Priority::Normal));
                f.gone(&user("a"));
                f.release(&caller("b"), &borrowed);
                f.confirm(&caller("b"), &borrowed, Confirm::Clean);
                let next = f.offered_lease(&user("w"));
                assert_eq!(f.state(&next), "offered");
                assert_eq!(f.count(&user("a"), NoticeKind::UnitReturned), 0);
            }
        }

        mod on_gone_borrower {
            use super::*;

            #[test]
            fn borrower_lease_ends_dirty_holder_gone() {
                let mut f = fixture();
                let (_, borrowed) = f.borrowed();
                f.gone(&user("b"));
                assert_eq!(f.state(&borrowed), "ended");
                let cause: String = f.column(&borrowed, "end_cause");
                assert_eq!(cause, "holder_gone");
            }

            #[test]
            fn unit_returns_to_the_paused_holder() {
                let mut f = fixture();
                let (paused, _) = f.borrowed();
                f.gone(&user("b"));
                assert_eq!(f.state(&paused), "held");
                let returned = f.notices_of(&user("a"), NoticeKind::UnitReturned);
                assert_eq!(returned.len(), 1);
                assert_eq!(
                    returned[0].dirty.as_ref().map(|d| d.reason),
                    Some(DirtyReason::HolderGone)
                );
            }
        }

        mod on_holder_both_paused_owner_and_waiter {
            use super::*;

            /// `a` is paused on board/u1 (lent to `b`) and also waits for
            /// `spare/s1`, held by `h`.
            fn both() -> (Fixture, LeaseId, LeaseId) {
                let mut f = fixture();
                let (paused, borrowed) = f.borrowed();
                f.seed("spare", &["s1"]);
                f.grant(&caller("h"), req("spare", Priority::Normal));
                f.queue(&caller("a"), req("spare", Priority::Normal));
                f.gone(&user("a"));
                (f, paused, borrowed)
            }

            #[test]
            fn paused_lease_ends_exactly_once() {
                let (mut f, paused, _) = both();
                assert_eq!(f.state(&paused), "ended");
                assert_eq!(f.lease_ended_audits(&paused), 1);
                assert_eq!(f.count(&user("a"), NoticeKind::LeaseEnded), 1);
            }

            #[test]
            fn request_dropped() {
                let (mut f, _, _) = both();
                assert_eq!(f.queued_for(&user("a")), 0);
            }

            #[test]
            fn borrower_keeps_the_unit() {
                let (mut f, _, borrowed) = both();
                assert_eq!(f.state(&borrowed), "held");
            }

            #[test]
            fn one_holder_gone_audit_lists_both() {
                let (mut f, paused, _) = both();
                let entries = f.audit("holder_gone");
                assert_eq!(entries.len(), 1);
                let d = &entries[0].detail;
                assert_eq!(d["leases_ended"].as_array().unwrap().len(), 1);
                assert_eq!(d["leases_ended"][0], paused.as_str());
                assert_eq!(d["requests_dropped"].as_array().unwrap().len(), 1);
            }
        }

        mod on_holder_with_several_held_leases {
            use super::*;

            /// `g` holds board/u1, spare/s1 and alias/v1 (v1 linked to
            /// other/x1), with a waiter on each.
            fn several() -> (Fixture, Vec<LeaseId>) {
                let mut f = fixture();
                f.seed("spare", &["s1"]);
                f.seed("alias", &["v1"]);
                f.seed("other", &["x1"]);
                f.link(("alias", "v1"), ("other", "x1"));
                let leases: Vec<LeaseId> = ["board", "spare", "alias"]
                    .into_iter()
                    .map(|r| f.grant(&caller("g"), req(r, Priority::Normal)).lease_id)
                    .collect();
                f.queue(&caller("w"), req("board", Priority::Normal));
                f.queue(&caller("w"), req("spare", Priority::Normal));
                f.queue(&caller("w"), req("other", Priority::Normal));
                f.gone(&user("g"));
                (f, leases)
            }

            #[test]
            fn every_lease_ends_exactly_once() {
                let (mut f, leases) = several();
                for lease in &leases {
                    assert_eq!(f.state(lease), "ended");
                    assert_eq!(f.lease_ended_audits(lease), 1);
                }
                assert_eq!(f.count(&user("g"), NoticeKind::LeaseEnded), 3);
            }

            #[test]
            fn one_holder_gone_audit_lists_all_three() {
                let (mut f, leases) = several();
                let entries = f.audit("holder_gone");
                assert_eq!(entries.len(), 1);
                let listed: Vec<String> = entries[0].detail["leases_ended"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap().to_owned())
                    .collect();
                let expected: Vec<String> = leases.iter().map(|l| l.as_str().to_owned()).collect();
                assert_eq!(listed, expected);
            }

            #[test]
            fn each_freed_group_is_offered_to_the_waiter() {
                let (mut f, _) = several();
                assert_eq!(f.count(&user("w"), NoticeKind::Offered), 3);
            }
        }

        mod audit {
            use super::*;

            #[test]
            fn one_entry_names_the_holder_and_what_was_dropped_and_ended() {
                let mut f = fixture();
                let lease = f
                    .grant(&caller("g"), req("board", Priority::Normal))
                    .lease_id;
                f.seed("spare", &["s1"]);
                f.grant(&caller("h"), req("spare", Priority::Normal));
                let request = f.queue(&caller("g"), req("spare", Priority::Normal));
                f.gone(&user("g"));
                let entries = f.audit("holder_gone");
                assert_eq!(entries.len(), 1);
                let d = &entries[0].detail;
                assert_eq!(d["holder"], serde_json::to_value(user("g")).unwrap());
                assert_eq!(d["requests_dropped"][0], request.as_str());
                assert_eq!(d["leases_ended"][0], lease.as_str());
            }
        }
    }

    mod cli_process_liveness_probe {
        use super::*;

        mod on_pid_no_longer_alive {
            use super::*;

            /// A real `sleep` holds u1 with `w` queued behind it; then it is
            /// sent `SIGKILL`, reaped, and the probe runs.
            fn killed() -> (Fixture, LeaseId, HolderRef, Vec<HolderRef>) {
                let mut f = fixture();
                let mut child = Sleeper::spawn();
                let holder = child.holder();
                let lease = f
                    .grant(&as_caller(holder.clone()), req("board", Priority::Normal))
                    .lease_id;
                f.queue(&caller("w"), req("board", Priority::Normal));
                assert_eq!(f.probe(), Vec::new(), "a live child is not gone");
                assert_eq!(f.state(&lease), "held");
                child.kill();
                let gone = f.probe();
                (f, lease, holder, gone)
            }

            #[test]
            fn lease_ends_dirty_holder_gone() {
                let (mut f, lease, holder, _) = killed();
                assert_eq!(f.state(&lease), "ended");
                let cause: String = f.column(&lease, "end_cause");
                assert_eq!(cause, "holder_gone");
                let offered = f.offered_lease(&user("w"));
                let carried: String = f.column(&offered, "dirty_info");
                let dirty: DirtyInfo = serde_json::from_str(&carried).unwrap();
                assert_eq!(dirty.reason, DirtyReason::HolderGone);
                assert_eq!(dirty.previous_holder, holder);
            }

            #[test]
            fn next_waiter_is_offered() {
                let (mut f, _, _, _) = killed();
                let offered = f.offered_lease(&user("w"));
                assert_eq!(f.state(&offered), "offered");
            }

            #[test]
            fn probe_reports_the_gone_holder() {
                let (_, _, holder, gone) = killed();
                assert_eq!(gone, vec![holder]);
            }

            #[test]
            fn queued_request_of_a_dead_process_is_dropped() {
                let mut f = fixture();
                f.grant(&caller("h"), req("board", Priority::Normal));
                let mut child = Sleeper::spawn();
                let holder = child.holder();
                f.queue(&as_caller(holder.clone()), req("board", Priority::Normal));
                child.kill();
                assert_eq!(f.probe(), vec![holder.clone()]);
                assert_eq!(f.queued_for(&holder), 0);
            }
        }

        mod on_pid_reused {
            use super::*;

            #[test]
            fn process_with_a_different_start_time_is_dead() {
                let fake = liveness::self_start_time().expect("own start time") + 1;
                assert!(!liveness::process_alive(std::process::id(), fake));
            }

            #[test]
            fn holder_gone_fired_for_the_stale_holder() {
                let mut f = fixture();
                let stale = HolderRef::CliProcess {
                    pid: std::process::id(),
                    start_time: liveness::self_start_time().expect("own start time") + 1,
                };
                let lease = f
                    .grant(&as_caller(stale.clone()), req("board", Priority::Normal))
                    .lease_id;
                assert_eq!(f.probe(), vec![stale]);
                assert_eq!(f.state(&lease), "ended");
            }
        }

        mod on_live_process {
            use super::*;

            #[test]
            fn own_process_is_alive() {
                assert!(liveness::process_alive(
                    std::process::id(),
                    liveness::self_start_time().expect("own start time")
                ));
            }

            #[test]
            fn its_lease_is_left_alone() {
                let mut f = fixture();
                let lease = f
                    .grant(&as_caller(self_holder()), req("board", Priority::Normal))
                    .lease_id;
                assert_eq!(f.probe(), Vec::new());
                assert_eq!(f.state(&lease), "held");
                assert_eq!(f.audit("holder_gone"), Vec::new());
            }
        }

        mod on_missing_pid {
            use super::*;

            #[test]
            fn process_is_dead() {
                // Above the kernel's pid_max ceiling (2^22), so never in use.
                assert!(!liveness::process_alive(u32::MAX - 1, 1));
            }
        }
    }

    mod agent_session_and_user_holders {
        use super::*;

        #[test]
        fn are_never_probed() {
            let mut f = fixture();
            let session = HolderRef::AgentSession {
                session_id: "s-1".into(),
            };
            let lease = f
                .grant(&as_caller(session), req("board", Priority::Normal))
                .lease_id;
            f.queue(&caller("w"), req("board", Priority::Normal));
            assert_eq!(f.probe(), Vec::new());
            assert_eq!(f.state(&lease), "held");
            assert_eq!(f.queued_for(&user("w")), 1);
        }
    }
}
