mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use leclerk::clock::ManualClock;
use leclerk::config::Config;
use leclerk::health::HealthResult;
use leclerk::inventory::{Inventory, NewResource, NewUnit, View};
use leclerk::model::{
    Caller, DirtyReason, EndCause, HealthSpec, HolderRef, LeaseId, Priority, ProjectKey,
    ResourceId, ResourceType, Scope, UnitId, UnitSettings,
};
use leclerk::scheduler::{
    AcquireOutcome, AcquireRequest, Confirm, Grant, HealthProbe, Lease, Occupancy, Scheduler,
    SchedulerError, Target,
};
use leclerk::store::audit::{self, AuditEntry};
use leclerk::store::notices::{self, Notice, NoticeKind};
use leclerk::store::{Store, StoreError, Tx};

const T0: i64 = 1_000_000;
const SEC: i64 = 1_000;
const MIN_MS: i64 = 60 * SEC;
const MIN: Duration = Duration::from_secs(60);

fn rid(s: &str) -> ResourceId {
    ResourceId::parse(s).unwrap()
}

fn uid(s: &str) -> UnitId {
    UnitId::parse(s).unwrap()
}

fn project(p: &str) -> ProjectKey {
    ProjectKey(PathBuf::from(p))
}

fn holder(tag: &str) -> HolderRef {
    HolderRef::User { tag: tag.into() }
}

fn caller_in(tag: &str, project_path: Option<&str>) -> Caller {
    Caller {
        holder: holder(tag),
        project: project_path.map(project),
        override_reason: None,
    }
}

fn caller(tag: &str) -> Caller {
    caller_in(tag, None)
}

fn req(resource: &str, duration: Duration, priority: Priority, purpose: &str) -> AcquireRequest {
    AcquireRequest {
        target: Target::Resource(rid(resource)),
        duration,
        interruptible: false,
        priority,
        purpose: purpose.into(),
        override_reason: None,
        skip_checks: false,
    }
}

/// Every unit is healthy.
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
    /// A generic resource in `scope`; unit identity keys are `key-unit`, so
    /// equal keys across scopes link units.
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
                        usage: String::new(),
                        health: None,
                        tags: Vec::new(),
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
                            description: String::new(),
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

    fn queue(&mut self, who: &Caller, r: AcquireRequest) {
        match self.acquire(who, r) {
            AcquireOutcome::Queued { .. } => {}
            other => panic!("expected queued, got {other:?}"),
        }
    }

    fn tick(&mut self) -> Option<i64> {
        self.store.tx(|tx| self.sched.tick(tx)).unwrap()
    }

    fn extend(
        &mut self,
        who: &Caller,
        lease: &LeaseId,
        new_ends_at: i64,
    ) -> Result<Lease, SchedulerError> {
        self.store
            .tx(|tx| self.sched.extend(tx, who, lease, new_ends_at))
    }

    fn release(&mut self, who: &Caller, lease: &LeaseId) -> Result<Lease, SchedulerError> {
        self.store.tx(|tx| self.sched.release(tx, who, lease))
    }

    fn confirm(
        &mut self,
        who: &Caller,
        lease: &LeaseId,
        how: Confirm,
    ) -> Result<(), SchedulerError> {
        self.store.tx(|tx| self.sched.confirm(tx, who, lease, how))
    }

    fn cleanup_extension(
        &mut self,
        who: &Caller,
        lease: &LeaseId,
        reason: &str,
    ) -> Result<Lease, SchedulerError> {
        self.store
            .tx(|tx| self.sched.request_cleanup_extension(tx, who, lease, reason))
    }

    fn end_lease(&mut self, lease: &LeaseId, cause: EndCause, confirmed: bool) {
        self.store
            .tx(|tx| self.sched.end_lease(tx, lease, cause, confirmed))
            .unwrap();
    }

    fn claim(&mut self, who: &Caller, lease: &LeaseId) {
        self.store
            .tx(|tx| self.sched.claim(tx, who, lease))
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

    fn unit(&mut self, resource: &str, unit: &str) -> leclerk::model::Unit {
        let who = caller("admin");
        self.store
            .tx(|tx| self.inv.get_resource(tx, &who, &rid(resource)))
            .unwrap()
            .units
            .into_iter()
            .find(|u| u.id.as_str() == unit)
            .unwrap()
    }

    fn occupancy(&mut self, resource: &str) -> Occupancy {
        let who = caller("admin");
        let status = self
            .store
            .tx(|tx| self.sched.status(tx, &who, View::All))
            .unwrap();
        status
            .resources
            .into_iter()
            .find(|r| r.id.as_str() == resource)
            .unwrap()
            .units
            .remove(0)
            .occupancy
    }

    /// Tick at `ends_at` (expiry) and again when its cleanup window passes,
    /// as a running daemon would.
    fn expire_and_pass(&mut self, ends_at: i64) {
        self.at(ends_at);
        self.tick();
        self.at(ends_at + 2 * MIN_MS);
        self.tick();
    }

    /// The lease offered to `h` by its latest Offered notice.
    fn offered_lease(&mut self, h: &HolderRef) -> LeaseId {
        self.notices_of(h, NoticeKind::Offered)
            .last()
            .and_then(|n| n.lease_id.clone())
            .expect("an Offered notice with a lease id")
    }

    /// Hold `board` as `a` for `d`, then release it at T0 and let the
    /// cleanup start. Returns the lease.
    fn released(&mut self, d: Duration) -> LeaseId {
        self.seed("board", &["u1"]);
        let g = self.grant(&caller("a"), req("board", d, Priority::Normal, "x"));
        self.release(&caller("a"), &g.lease_id).unwrap();
        g.lease_id
    }
}

mod lease_lifecycle {
    use super::*;

    mod extend {
        use super::*;

        mod on_held_lease_no_higher_priority_waiter {
            use super::*;

            #[test]
            fn updates_ends_at() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
                let lease = f
                    .extend(&caller("a"), &g.lease_id, T0 + 20 * MIN_MS)
                    .unwrap();
                assert_eq!(lease.ends_at, Some(T0 + 20 * MIN_MS));
                assert_eq!(f.column::<i64>(&g.lease_id, "ends_at"), T0 + 20 * MIN_MS);
            }

            #[test]
            fn re_arms_the_warning() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
                f.at(T0 + 8 * MIN_MS);
                f.tick();
                assert_eq!(f.count(&holder("a"), NoticeKind::ExpiryWarning), 1);
                f.extend(&caller("a"), &g.lease_id, T0 + 20 * MIN_MS)
                    .unwrap();
                // Duration is now 20 min: lead min(300 s, 4 min) = 4 min.
                assert_eq!(f.tick(), Some(T0 + 16 * MIN_MS));
                f.at(T0 + 16 * MIN_MS);
                f.tick();
                assert_eq!(f.count(&holder("a"), NoticeKind::ExpiryWarning), 2);
            }

            #[test]
            fn writes_extended_audit_entry() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
                f.extend(&caller("a"), &g.lease_id, T0 + 20 * MIN_MS)
                    .unwrap();
                let entries = f.audit("extended");
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].actor, holder("a"));
                assert_eq!(entries[0].detail["ends_at"], T0 + 20 * MIN_MS);
            }

            #[test]
            fn allows_an_equal_priority_waiter() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
                f.queue(&caller("b"), req("board", MIN, Priority::Normal, "y"));
                assert!(
                    f.extend(&caller("a"), &g.lease_id, T0 + 20 * MIN_MS)
                        .is_ok()
                );
            }

            #[test]
            fn allows_an_interruptible_lease_despite_a_higher_priority_waiter() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let mut r = req("board", 10 * MIN, Priority::Normal, "x");
                r.interruptible = true;
                let g = f.grant(&caller("a"), r);
                f.queue(&caller("b"), req("board", MIN, Priority::High, "y"));
                assert!(
                    f.extend(&caller("a"), &g.lease_id, T0 + 20 * MIN_MS)
                        .is_ok()
                );
            }
        }

        mod on_non_interruptible_lease_with_strictly_higher_priority_waiter {
            use super::*;

            #[test]
            fn returns_extend_refused_naming_waiting_purpose() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
                f.queue(&caller("b"), req("board", MIN, Priority::High, "flash fix"));
                let err = f
                    .extend(&caller("a"), &g.lease_id, T0 + 20 * MIN_MS)
                    .unwrap_err();
                assert!(matches!(
                    err,
                    SchedulerError::ExtendRefused { waiting_purpose: Some(ref p) } if p == "flash fix"
                ));
            }

            #[test]
            fn leaves_ends_at_unchanged() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
                f.queue(&caller("b"), req("board", MIN, Priority::High, "y"));
                let _ = f.extend(&caller("a"), &g.lease_id, T0 + 20 * MIN_MS);
                assert_eq!(f.column::<i64>(&g.lease_id, "ends_at"), T0 + 10 * MIN_MS);
            }

            #[test]
            fn hides_the_purpose_of_a_waiter_the_holder_may_not_see() {
                let mut f = fixture();
                let a = caller_in("a", Some("/p/a"));
                let b = caller_in("b", Some("/p/b"));
                f.seed_keyed(&a, "board", &Scope::Project(project("/p/a")), &["u1"], "k");
                f.seed_keyed(&b, "mine", &Scope::Project(project("/p/b")), &["u1"], "k");
                let g = f.grant(&a, req("board", 10 * MIN, Priority::Normal, "x"));
                f.queue(&b, req("mine", MIN, Priority::High, "secret"));
                let err = f.extend(&a, &g.lease_id, T0 + 20 * MIN_MS).unwrap_err();
                assert!(matches!(
                    err,
                    SchedulerError::ExtendRefused {
                        waiting_purpose: None
                    }
                ));
            }
        }

        mod on_wrong_caller_or_state {
            use super::*;

            #[test]
            fn returns_not_holder_for_someone_else() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
                let err = f
                    .extend(&caller("b"), &g.lease_id, T0 + 20 * MIN_MS)
                    .unwrap_err();
                assert!(matches!(err, SchedulerError::NotHolder));
            }

            #[test]
            fn returns_bad_state_for_a_cleaning_lease() {
                let mut f = fixture();
                let lease = f.released(10 * MIN);
                let err = f
                    .extend(&caller("a"), &lease, T0 + 20 * MIN_MS)
                    .unwrap_err();
                assert!(matches!(err, SchedulerError::BadState { .. }));
            }

            #[test]
            fn past_its_end_unticked_returns_bad_state() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
                // Past ends_at and past ends_at + window, but nothing ticked.
                f.at(T0 + 12 * MIN_MS);
                let err = f
                    .extend(&caller("a"), &g.lease_id, T0 + 30 * MIN_MS)
                    .unwrap_err();
                assert!(matches!(err, SchedulerError::BadState { .. }));
                // extend changes nothing: the row stays held with its old end
                // until the next tick expires it (unlike release, which runs
                // the expiry itself).
                assert_eq!(f.state(&g.lease_id), "held");
                assert_eq!(f.column::<i64>(&g.lease_id, "ends_at"), T0 + 10 * MIN_MS);
                assert_eq!(f.audit("extended").len(), 0);
            }

            #[test]
            fn returns_extend_not_later_for_an_earlier_end() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
                let err = f
                    .extend(&caller("a"), &g.lease_id, T0 + 5 * MIN_MS)
                    .unwrap_err();
                assert!(matches!(err, SchedulerError::ExtendNotLater { .. }));
            }
        }
    }

    mod timers {
        use super::*;

        mod on_now_at_ends_at_minus_warning_lead {
            use super::*;

            #[test]
            fn ten_minute_lease_warns_at_eight_minutes() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
                f.at(T0 + 8 * MIN_MS - 1);
                f.tick();
                assert_eq!(f.count(&holder("a"), NoticeKind::ExpiryWarning), 0);
                f.at(T0 + 8 * MIN_MS);
                f.tick();
                assert_eq!(f.count(&holder("a"), NoticeKind::ExpiryWarning), 1);
            }

            #[test]
            fn two_hour_lease_warns_at_one_hour_fifty_five_minutes() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                f.grant(&caller("a"), req("board", 120 * MIN, Priority::Normal, "x"));
                f.at(T0 + 115 * MIN_MS - 1);
                f.tick();
                assert_eq!(f.count(&holder("a"), NoticeKind::ExpiryWarning), 0);
                f.at(T0 + 115 * MIN_MS);
                f.tick();
                assert_eq!(f.count(&holder("a"), NoticeKind::ExpiryWarning), 1);
            }

            #[test]
            fn sends_one_expiry_warning_per_lease_not_repeated() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
                for s in [480, 490, 500, 599] {
                    f.at(T0 + s * SEC);
                    f.tick();
                }
                assert_eq!(f.count(&holder("a"), NoticeKind::ExpiryWarning), 1);
            }

            #[test]
            fn warning_names_the_lease_and_its_end() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
                f.at(T0 + 8 * MIN_MS);
                f.tick();
                let n = &f.notices_of(&holder("a"), NoticeKind::ExpiryWarning)[0];
                assert_eq!(n.lease_id.as_ref(), Some(&g.lease_id));
                assert_eq!(n.deadline, Some(T0 + 10 * MIN_MS));
            }

            #[test]
            fn tick_returns_the_warning_time_as_next_deadline() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
                assert_eq!(f.tick(), Some(T0 + 8 * MIN_MS));
            }

            #[test]
            fn tick_returns_none_when_nothing_is_pending() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                assert_eq!(f.tick(), None);
            }
        }

        mod on_now_at_ends_at_without_extension {
            use super::*;

            #[test]
            fn state_cleaning_with_cause_expired() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
                f.at(T0 + 10 * MIN_MS);
                f.tick();
                assert_eq!(f.state(&g.lease_id), "cleaning");
                assert_eq!(f.column::<String>(&g.lease_id, "cleanup_cause"), "expired");
            }

            #[test]
            fn notice_cleanup_window_started_with_deadline() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
                f.at(T0 + 10 * MIN_MS);
                f.tick();
                let n = f.notices_of(&holder("a"), NoticeKind::CleanupWindowStarted);
                assert_eq!(n.len(), 1);
                assert_eq!(n[0].lease_id.as_ref(), Some(&g.lease_id));
                assert_eq!(n[0].deadline, Some(T0 + 12 * MIN_MS));
            }

            #[test]
            fn expires_even_when_nobody_is_waiting() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req("board", MIN, Priority::Normal, "x"));
                f.at(T0 + MIN_MS);
                f.tick();
                assert_eq!(f.state(&g.lease_id), "cleaning");
            }

            #[test]
            fn tick_returns_the_cleanup_ending_notice_next() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
                f.at(T0 + 10 * MIN_MS);
                // The cleanup window ends at T0 + 12 min; its ending notice
                // is due 30 s before.
                assert_eq!(f.tick(), Some(T0 + 12 * MIN_MS - 30 * SEC));
            }
        }

        mod on_overdue_backlog {
            use super::*;

            /// Leases on separate resources, all overdue at T0 + 1 h:
            /// - `rel`: released at T0 + 1 min, cleanup ends T0 + 3 min;
            /// - `short`: 5 min, expires T0 + 5 min;
            /// - offer on `off`: offered T0 + 2.5 min, claim deadline T0 + 7.5 min;
            /// - `long`: 10 min, expires T0 + 10 min;
            /// - `far`: 2 h, warns at T0 + 115 min (not yet due).
            ///
            /// Expiries processed late get a full cleanup window from now,
            /// so `short` and `long` end up cleaning until T0 + 62 min.
            fn backlog(f: &mut Fixture) -> [LeaseId; 5] {
                for r in ["rel", "short", "off", "long", "far"] {
                    f.seed(r, &["u1"]);
                }
                let rel = f.grant(&caller("rel"), req("rel", 30 * MIN, Priority::Normal, "x"));
                let short = f.grant(
                    &caller("short"),
                    req("short", 5 * MIN, Priority::Normal, "x"),
                );
                let off_holder =
                    f.grant(&caller("o1"), req("off", 30 * MIN, Priority::Normal, "x"));
                f.queue(&caller("o2"), req("off", MIN, Priority::Normal, "x"));
                let long = f.grant(
                    &caller("long"),
                    req("long", 10 * MIN, Priority::Normal, "x"),
                );
                let far = f.grant(&caller("far"), req("far", 120 * MIN, Priority::Normal, "x"));
                f.at(T0 + MIN_MS);
                f.release(&caller("rel"), &rel.lease_id).unwrap();
                f.at(T0 + 2 * MIN_MS + 30 * SEC);
                f.release(&caller("o1"), &off_holder.lease_id).unwrap();
                f.confirm(&caller("o1"), &off_holder.lease_id, Confirm::Clean)
                    .unwrap();
                let offer = f.offered_lease(&holder("o2"));
                f.at(T0 + 60 * MIN_MS);
                [
                    rel.lease_id,
                    short.lease_id,
                    offer,
                    long.lease_id,
                    far.lease_id,
                ]
            }

            #[test]
            fn processes_every_overdue_deadline_in_one_call() {
                let mut f = fixture();
                let [rel, short, offer, long, far] = backlog(&mut f);
                f.tick();
                assert_eq!(f.state(&rel), "ended");
                assert_eq!(f.state(&offer), "ended");
                assert_eq!(f.state(&short), "cleaning");
                assert_eq!(f.state(&long), "cleaning");
                assert_eq!(f.state(&far), "held");
            }

            #[test]
            fn late_expiries_get_a_full_cleanup_window_from_now() {
                let mut f = fixture();
                let [_, short, _, long, _] = backlog(&mut f);
                f.tick();
                for l in [&short, &long] {
                    assert_eq!(f.column::<i64>(l, "cleanup_deadline"), T0 + 62 * MIN_MS);
                }
            }

            #[test]
            fn processes_them_in_deadline_order() {
                let mut f = fixture();
                let [rel, short, offer, long, _] = backlog(&mut f);
                let before = f.store.tx(audit::all).unwrap().len();
                f.tick();
                let order: Vec<(String, String)> = f
                    .store
                    .tx(audit::all)
                    .unwrap()
                    .into_iter()
                    .skip(before)
                    .filter_map(|e| {
                        let lease = e.detail["lease"].as_str()?.to_owned();
                        Some((e.action, lease))
                    })
                    .collect();
                let expected: Vec<(String, String)> = [
                    ("lease_ended", &rel),
                    ("expired", &short),
                    ("offer_expired", &offer),
                    ("expired", &long),
                ]
                .into_iter()
                .map(|(a, l)| (a.to_owned(), l.as_str().to_owned()))
                .collect();
                assert_eq!(order, expected);
            }

            #[test]
            fn ends_each_overdue_cleanup_unconfirmed_with_its_cause() {
                let mut f = fixture();
                backlog(&mut f);
                f.tick();
                let reason = |f: &mut Fixture, r: &str| f.unit(r, "u1").dirty.map(|d| d.reason);
                assert_eq!(reason(&mut f, "rel"), Some(DirtyReason::WindowPassed));
                assert_eq!(reason(&mut f, "off"), None);
                f.at(T0 + 62 * MIN_MS);
                f.tick();
                assert_eq!(reason(&mut f, "short"), Some(DirtyReason::Expired));
                assert_eq!(reason(&mut f, "long"), Some(DirtyReason::Expired));
            }

            #[test]
            fn skips_warnings_for_leases_already_past_their_end() {
                let mut f = fixture();
                backlog(&mut f);
                f.tick();
                assert_eq!(f.count(&holder("short"), NoticeKind::ExpiryWarning), 0);
                assert_eq!(f.count(&holder("long"), NoticeKind::ExpiryWarning), 0);
            }

            #[test]
            fn returns_the_next_pending_deadline() {
                let mut f = fixture();
                backlog(&mut f);
                // The cleanup-ending notices of `short` and `long`.
                assert_eq!(f.tick(), Some(T0 + 62 * MIN_MS - 30 * SEC));
            }

            #[test]
            fn second_tick_at_the_same_time_does_nothing() {
                let mut f = fixture();
                backlog(&mut f);
                f.tick();
                let audit = f.store.tx(audit::all).unwrap().len();
                let notices: usize = ["rel", "short", "o2", "long", "far"]
                    .iter()
                    .map(|h| f.with_tx(|tx| notices::pending(tx, &holder(h)).unwrap().len()))
                    .sum();
                assert_eq!(f.tick(), Some(T0 + 62 * MIN_MS - 30 * SEC));
                assert_eq!(f.store.tx(audit::all).unwrap().len(), audit);
                let again: usize = ["rel", "short", "o2", "long", "far"]
                    .iter()
                    .map(|h| f.with_tx(|tx| notices::pending(tx, &holder(h)).unwrap().len()))
                    .sum();
                assert_eq!(again, notices);
            }

            #[test]
            fn handles_a_large_backlog_in_one_call() {
                let mut f = fixture();
                let mut leases = Vec::new();
                for i in 0..60 {
                    let r = format!("r{i}");
                    f.seed(&r, &["u1"]);
                    let d = Duration::from_secs(60 + i);
                    leases.push(
                        f.grant(&caller(&r), req(&r, d, Priority::Normal, "x"))
                            .lease_id,
                    );
                }
                let now = T0 + 24 * 60 * MIN_MS;
                f.at(now);
                assert_eq!(f.tick(), Some(now + 2 * MIN_MS - 30 * SEC));
                for l in &leases {
                    assert_eq!(f.state(l), "cleaning");
                }
                f.at(now + 2 * MIN_MS);
                assert_eq!(f.tick(), None);
                for l in &leases {
                    assert_eq!(f.state(l), "ended");
                }
            }
        }
    }

    mod downtime {
        use super::*;

        mod on_expiry_and_window_both_passed_while_down {
            use super::*;

            fn after_downtime(f: &mut Fixture) -> LeaseId {
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
                // ends_at + window = T0 + 12 min passed while nothing ticked.
                f.at(T0 + 60 * MIN_MS);
                f.tick();
                g.lease_id
            }

            #[test]
            fn one_tick_leaves_the_lease_cleaning_with_a_window_from_now() {
                let mut f = fixture();
                let lease = after_downtime(&mut f);
                assert_eq!(f.state(&lease), "cleaning");
                assert_eq!(
                    f.column::<i64>(&lease, "cleanup_deadline"),
                    T0 + 60 * MIN_MS + 120 * SEC
                );
            }

            #[test]
            fn confirm_then_ends_it_clean() {
                let mut f = fixture();
                let lease = after_downtime(&mut f);
                f.confirm(&caller("a"), &lease, Confirm::Clean).unwrap();
                assert_eq!(f.state(&lease), "ended");
                assert_eq!(f.column::<String>(&lease, "end_cause"), "expired");
                assert_eq!(f.unit("board", "u1").dirty, None);
            }
        }
    }

    mod cleanup_ending {
        use super::*;

        /// Cleanup window 2 min (default): the notice goes out 30 s before
        /// the deadline. `released` releases at T0, so the deadline is
        /// T0 + 2 min.
        const DEADLINE: i64 = T0 + 2 * MIN_MS;
        const NOTICE_AT: i64 = DEADLINE - 30 * SEC;

        fn endings(f: &mut Fixture) -> Vec<Notice> {
            f.notices_of(&holder("a"), NoticeKind::CleanupEnding)
        }

        mod on_a_cleaning_lease {
            use super::*;

            #[test]
            fn sends_nothing_before_the_lead() {
                let mut f = fixture();
                f.released(MIN);
                f.at(NOTICE_AT - 1);
                f.tick();
                assert_eq!(endings(&mut f).len(), 0);
            }

            #[test]
            fn sends_cleanup_ending_with_the_deadline_at_the_lead() {
                let mut f = fixture();
                let lease = f.released(MIN);
                f.at(NOTICE_AT);
                f.tick();
                let got = endings(&mut f);
                assert_eq!(got.len(), 1);
                assert_eq!(got[0].lease_id.as_ref(), Some(&lease));
                assert_eq!(got[0].deadline, Some(DEADLINE));
            }

            #[test]
            fn sends_it_once_however_often_it_ticks() {
                let mut f = fixture();
                f.released(MIN);
                f.at(NOTICE_AT);
                f.tick();
                f.tick();
                f.at(NOTICE_AT + 10 * SEC);
                f.tick();
                assert_eq!(endings(&mut f).len(), 1);
            }

            #[test]
            fn tick_returns_the_notice_time_as_next_deadline() {
                let mut f = fixture();
                f.released(MIN);
                assert_eq!(f.tick(), Some(NOTICE_AT));
            }

            #[test]
            fn is_skipped_when_the_window_already_passed() {
                let mut f = fixture();
                let lease = f.released(MIN);
                f.at(DEADLINE + MIN_MS);
                f.tick();
                assert_eq!(endings(&mut f).len(), 0);
                assert_eq!(f.state(&lease), "ended");
            }
        }

        mod on_a_short_cleanup_window {
            use super::*;

            #[test]
            fn leads_by_half_the_window() {
                let clock = Arc::new(ManualClock::new(T0));
                let config = Config {
                    cleanup_window: 20,
                    ..Config::default()
                };
                let mut f = Fixture {
                    store: Store::open_in_memory().unwrap(),
                    inv: Inventory::new(clock.clone()),
                    sched: Scheduler::new(clock.clone(), config, Arc::new(Healthy)),
                    clock,
                };
                f.released(MIN);
                f.at(T0 + 10 * SEC - 1);
                f.tick();
                assert_eq!(endings(&mut f).len(), 0);
                f.at(T0 + 10 * SEC);
                f.tick();
                assert_eq!(endings(&mut f).len(), 1);
            }
        }

        mod on_a_cleanup_extension {
            use super::*;

            #[test]
            fn sends_another_before_the_new_deadline() {
                let mut f = fixture();
                let lease = f.released(MIN);
                f.at(NOTICE_AT);
                f.tick();
                f.cleanup_extension(&caller("a"), &lease, "flashing")
                    .unwrap();
                // The extension adds 2 min to the deadline.
                let new_deadline = DEADLINE + 2 * MIN_MS;
                f.at(new_deadline - 30 * SEC - 1);
                f.tick();
                assert_eq!(endings(&mut f).len(), 1);
                f.at(new_deadline - 30 * SEC);
                f.tick();
                let got = endings(&mut f);
                assert_eq!(got.len(), 2);
                assert_eq!(got[1].deadline, Some(new_deadline));
            }
        }

        mod on_a_restart_grace_refresh {
            use super::*;

            #[test]
            fn sends_another_before_the_fresh_deadline() {
                let mut f = fixture();
                f.released(MIN);
                f.at(NOTICE_AT);
                f.tick();
                // The daemon was down past the deadline; the grace gives a
                // fresh 2 min window from now.
                let restart = T0 + 10 * MIN_MS;
                f.at(restart);
                f.store.tx(|tx| f.sched.restart_grace(tx)).unwrap();
                f.at(restart + 2 * MIN_MS - 30 * SEC);
                f.tick();
                assert_eq!(endings(&mut f).len(), 2);
            }
        }
    }

    mod release {
        use super::*;

        mod on_held_lease {
            use super::*;

            #[test]
            fn state_cleaning_with_cause_released() {
                let mut f = fixture();
                let lease = f.released(10 * MIN);
                assert_eq!(f.state(&lease), "cleaning");
                assert_eq!(f.column::<String>(&lease, "cleanup_cause"), "released");
            }

            #[test]
            fn notice_to_holder_with_deadline() {
                let mut f = fixture();
                let lease = f.released(10 * MIN);
                let n = f.notices_of(&holder("a"), NoticeKind::CleanupWindowStarted);
                assert_eq!(n.len(), 1);
                assert_eq!(n[0].lease_id.as_ref(), Some(&lease));
                assert_eq!(n[0].deadline, Some(T0 + 2 * MIN_MS));
            }

            #[test]
            fn returns_the_lease_with_its_cleanup_deadline() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
                let lease = f.release(&caller("a"), &g.lease_id).unwrap();
                assert_eq!(lease.cleanup_deadline, Some(T0 + 2 * MIN_MS));
            }

            #[test]
            fn keeps_the_unit_busy_during_cleanup() {
                let mut f = fixture();
                f.released(10 * MIN);
                assert!(matches!(f.occupancy("board"), Occupancy::Held(_)));
            }
        }

        mod on_other_states {
            use super::*;

            #[test]
            fn returns_not_holder_for_someone_else() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
                let err = f.release(&caller("b"), &g.lease_id).unwrap_err();
                assert!(matches!(err, SchedulerError::NotHolder));
            }

            #[test]
            fn past_its_end_unticked_is_treated_as_expired() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
                f.at(T0 + 10 * MIN_MS);
                let lease = f.release(&caller("a"), &g.lease_id).unwrap();
                assert_eq!(lease.cleanup_deadline, Some(T0 + 12 * MIN_MS));
                assert_eq!(f.state(&g.lease_id), "cleaning");
                assert_eq!(f.column::<String>(&g.lease_id, "cleanup_cause"), "expired");
                assert_eq!(f.audit("released").len(), 0);
                assert_eq!(f.audit("expired").len(), 1);
            }

            #[test]
            fn returns_bad_state_when_already_cleaning() {
                let mut f = fixture();
                let lease = f.released(10 * MIN);
                let err = f.release(&caller("a"), &lease).unwrap_err();
                assert!(matches!(err, SchedulerError::BadState { .. }));
            }
        }
    }

    mod confirm {
        use super::*;

        mod on_cleaning_lease {
            use super::*;

            #[test]
            fn lease_ended() {
                let mut f = fixture();
                let lease = f.released(10 * MIN);
                f.confirm(&caller("a"), &lease, Confirm::Clean).unwrap();
                assert_eq!(f.state(&lease), "ended");
                assert_eq!(f.column::<String>(&lease, "end_cause"), "released");
            }

            #[test]
            fn unit_not_dirty() {
                let mut f = fixture();
                let lease = f.released(10 * MIN);
                f.confirm(&caller("a"), &lease, Confirm::Clean).unwrap();
                assert_eq!(f.unit("board", "u1").dirty, None);
            }

            #[test]
            fn next_dispatch_offers_the_unit_to_a_waiter() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
                f.queue(&caller("b"), req("board", MIN, Priority::Normal, "y"));
                f.release(&caller("a"), &g.lease_id).unwrap();
                assert_eq!(f.count(&holder("b"), NoticeKind::Offered), 0);
                f.confirm(&caller("a"), &g.lease_id, Confirm::Clean)
                    .unwrap();
                let n = f.notices_of(&holder("b"), NoticeKind::Offered);
                assert_eq!(n.len(), 1);
                assert_eq!(n[0].dirty, None);
            }

            #[test]
            fn notifies_the_holder_lease_ended() {
                let mut f = fixture();
                let lease = f.released(10 * MIN);
                f.confirm(&caller("a"), &lease, Confirm::Clean).unwrap();
                let n = f.notices_of(&holder("a"), NoticeKind::LeaseEnded);
                assert_eq!(n.len(), 1);
                assert_eq!(n[0].lease_id.as_ref(), Some(&lease));
            }
        }

        mod on_yielded {
            use super::*;

            #[test]
            fn returns_bad_state_without_a_yield_request() {
                let mut f = fixture();
                let lease = f.released(10 * MIN);
                let err = f
                    .confirm(&caller("a"), &lease, Confirm::Yielded)
                    .unwrap_err();
                assert!(matches!(err, SchedulerError::BadState { .. }));
                assert_eq!(f.state(&lease), "cleaning");
            }
        }

        mod on_wrong_caller_or_state {
            use super::*;

            #[test]
            fn returns_not_holder_for_someone_else() {
                let mut f = fixture();
                let lease = f.released(10 * MIN);
                let err = f.confirm(&caller("b"), &lease, Confirm::Clean).unwrap_err();
                assert!(matches!(err, SchedulerError::NotHolder));
            }

            #[test]
            fn returns_bad_state_for_a_held_lease() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
                let err = f
                    .confirm(&caller("a"), &g.lease_id, Confirm::Clean)
                    .unwrap_err();
                assert!(matches!(err, SchedulerError::BadState { .. }));
            }
        }
    }

    mod request_cleanup_extension {
        use super::*;

        mod on_first_request {
            use super::*;

            #[test]
            fn deadline_grows_by_cleanup_extension() {
                let mut f = fixture();
                let lease = f.released(10 * MIN);
                let l = f
                    .cleanup_extension(&caller("a"), &lease, "flashing takes a while")
                    .unwrap();
                assert_eq!(l.cleanup_deadline, Some(T0 + 4 * MIN_MS));
                f.at(T0 + 3 * MIN_MS);
                f.tick();
                assert_eq!(f.state(&lease), "cleaning");
            }

            #[test]
            fn writes_audit_entry_with_reason() {
                let mut f = fixture();
                let lease = f.released(10 * MIN);
                f.cleanup_extension(&caller("a"), &lease, "flashing takes a while")
                    .unwrap();
                let entries = f.audit("cleanup_extended");
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].actor, holder("a"));
                assert_eq!(entries[0].detail["reason"], "flashing takes a while");
            }
        }

        mod on_second_request {
            use super::*;

            #[test]
            fn returns_extension_used() {
                let mut f = fixture();
                let lease = f.released(10 * MIN);
                f.cleanup_extension(&caller("a"), &lease, "one").unwrap();
                let err = f
                    .cleanup_extension(&caller("a"), &lease, "two")
                    .unwrap_err();
                assert!(matches!(err, SchedulerError::ExtensionUsed));
            }

            #[test]
            fn leaves_the_deadline_unchanged() {
                let mut f = fixture();
                let lease = f.released(10 * MIN);
                f.cleanup_extension(&caller("a"), &lease, "one").unwrap();
                let _ = f.cleanup_extension(&caller("a"), &lease, "two");
                assert_eq!(f.column::<i64>(&lease, "cleanup_deadline"), T0 + 4 * MIN_MS);
            }
        }

        mod on_wrong_caller_or_state {
            use super::*;

            #[test]
            fn returns_not_holder_for_someone_else() {
                let mut f = fixture();
                let lease = f.released(10 * MIN);
                let err = f.cleanup_extension(&caller("b"), &lease, "r").unwrap_err();
                assert!(matches!(err, SchedulerError::NotHolder));
            }

            #[test]
            fn returns_bad_state_for_a_held_lease() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
                let err = f
                    .cleanup_extension(&caller("a"), &g.lease_id, "r")
                    .unwrap_err();
                assert!(matches!(err, SchedulerError::BadState { .. }));
            }
        }
    }

    mod cleanup_window_passing {
        use super::*;

        mod on_no_confirmation {
            use super::*;

            #[test]
            fn lease_ended() {
                let mut f = fixture();
                let lease = f.released(10 * MIN);
                f.at(T0 + 2 * MIN_MS);
                f.tick();
                assert_eq!(f.state(&lease), "ended");
                assert_eq!(f.column::<String>(&lease, "end_cause"), "released");
            }

            #[test]
            fn unit_dirty_window_passed_after_release() {
                let mut f = fixture();
                f.released(10 * MIN);
                f.at(T0 + 2 * MIN_MS);
                f.tick();
                let d = f.unit("board", "u1").dirty.unwrap();
                assert_eq!(d.reason, DirtyReason::WindowPassed);
                assert_eq!(d.at, T0 + 2 * MIN_MS);
                assert_eq!(d.previous_holder, holder("a"));
            }

            #[test]
            fn unit_dirty_expired_after_expiry() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
                f.expire_and_pass(T0 + 10 * MIN_MS);
                assert_eq!(f.state(&g.lease_id), "ended");
                assert_eq!(f.column::<String>(&g.lease_id, "end_cause"), "expired");
                assert_eq!(
                    f.unit("board", "u1").dirty.map(|d| d.reason),
                    Some(DirtyReason::Expired)
                );
            }

            #[test]
            fn dispatches_the_unit_to_a_waiter_with_dirty_info() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
                f.queue(&caller("b"), req("board", MIN, Priority::Normal, "y"));
                f.release(&caller("a"), &g.lease_id).unwrap();
                f.at(T0 + 2 * MIN_MS);
                f.tick();
                let n = f.notices_of(&holder("b"), NoticeKind::Offered);
                assert_eq!(n.len(), 1);
                assert_eq!(
                    n[0].dirty.as_ref().map(|d| d.reason),
                    Some(DirtyReason::WindowPassed)
                );
            }

            #[test]
            fn tells_the_holder_cleanup_was_not_confirmed() {
                let mut f = fixture();
                let lease = f.released(10 * MIN);
                f.at(T0 + 2 * MIN_MS);
                f.tick();
                let n = f.notices_of(&holder("a"), NoticeKind::LeaseEnded);
                assert_eq!(n.len(), 1);
                assert_eq!(n[0].lease_id.as_ref(), Some(&lease));
                assert_eq!(
                    n[0].dirty.as_ref().map(|d| d.reason),
                    Some(DirtyReason::WindowPassed)
                );
            }

            #[test]
            fn confirm_after_the_deadline_is_refused() {
                let mut f = fixture();
                let lease = f.released(10 * MIN);
                f.at(T0 + 2 * MIN_MS);
                let err = f.confirm(&caller("a"), &lease, Confirm::Clean).unwrap_err();
                assert!(matches!(err, SchedulerError::BadState { .. }));
            }
        }

        mod on_nobody_waiting {
            use super::*;

            #[test]
            fn unit_returns_to_the_free_pool_still_dirty() {
                let mut f = fixture();
                f.released(10 * MIN);
                f.at(T0 + 2 * MIN_MS);
                f.tick();
                assert_eq!(f.occupancy("board"), Occupancy::Free);
                assert!(f.unit("board", "u1").dirty.is_some());
            }
        }
    }

    mod dirty_flag {
        use super::*;

        mod on_next_grant_after_unconfirmed_cleanup {
            use super::*;

            fn dirty_unit(f: &mut Fixture) {
                f.seed("board", &["u1"]);
                f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
                f.expire_and_pass(T0 + 10 * MIN_MS);
            }

            #[test]
            fn grant_carries_the_dirty_record() {
                let mut f = fixture();
                dirty_unit(&mut f);
                let g = f.grant(&caller("b"), req("board", MIN, Priority::Normal, "y"));
                let d = g.dirty.unwrap();
                assert_eq!(d.reason, DirtyReason::Expired);
                assert_eq!(d.previous_holder, holder("a"));
            }

            #[test]
            fn flag_cleared_after_delivery() {
                let mut f = fixture();
                dirty_unit(&mut f);
                f.grant(&caller("b"), req("board", MIN, Priority::Normal, "y"));
                assert_eq!(f.unit("board", "u1").dirty, None);
            }

            #[test]
            fn the_grant_after_a_confirmed_cleanup_is_clean() {
                let mut f = fixture();
                dirty_unit(&mut f);
                let g = f.grant(&caller("b"), req("board", MIN, Priority::Normal, "y"));
                f.release(&caller("b"), &g.lease_id).unwrap();
                f.confirm(&caller("b"), &g.lease_id, Confirm::Clean)
                    .unwrap();
                let g = f.grant(&caller("c"), req("board", MIN, Priority::Normal, "z"));
                assert_eq!(g.dirty, None);
            }
        }
    }

    mod end_lease {
        use super::*;

        mod on_unconfirmed_end {
            use super::*;

            #[test]
            fn marks_the_unit_dirty_with_the_mapped_reason() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
                f.end_lease(&g.lease_id, EndCause::HolderGone, false);
                assert_eq!(f.state(&g.lease_id), "ended");
                assert_eq!(
                    f.unit("board", "u1").dirty.map(|d| d.reason),
                    Some(DirtyReason::HolderGone)
                );
            }

            #[test]
            fn writes_audit_entry_recording_unconfirmed_cleanup() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
                f.end_lease(&g.lease_id, EndCause::AdminForced, false);
                let e = &f.audit("lease_ended")[0];
                assert_eq!(e.detail["lease"], g.lease_id.as_str());
                assert_eq!(e.detail["cause"], "admin_forced");
                assert_eq!(e.detail["cleanup_confirmed"], false);
            }

            #[test]
            fn offers_the_unit_to_the_next_waiter() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
                f.queue(&caller("b"), req("board", MIN, Priority::Normal, "y"));
                f.end_lease(&g.lease_id, EndCause::HolderGone, false);
                assert_eq!(f.count(&holder("b"), NoticeKind::Offered), 1);
            }

            #[test]
            fn an_unclaimed_offer_gives_back_its_carried_dirty_record() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
                f.queue(&caller("b"), req("board", MIN, Priority::Normal, "y"));
                f.queue(&caller("c"), req("board", MIN, Priority::Normal, "z"));
                f.end_lease(&g.lease_id, EndCause::Expired, false);
                let offer = f.offered_lease(&holder("b"));
                // b vanishes before claiming: the unit was never touched.
                f.end_lease(&offer, EndCause::HolderGone, false);
                let n = f.notices_of(&holder("c"), NoticeKind::Offered);
                assert_eq!(
                    n[0].dirty
                        .as_ref()
                        .map(|d| (d.reason, d.previous_holder.clone())),
                    Some((DirtyReason::Expired, holder("a")))
                );
            }
        }

        mod on_confirmed_end {
            use super::*;

            #[test]
            fn leaves_the_unit_clean() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
                f.end_lease(&g.lease_id, EndCause::Released, true);
                assert_eq!(f.unit("board", "u1").dirty, None);
                assert_eq!(f.audit("lease_ended")[0].detail["cleanup_confirmed"], true);
            }
        }
    }

    mod queue_moves {
        use super::*;

        mod on_offer_after_cleanup_with_three_waiters {
            use super::*;

            #[test]
            fn waiters_behind_are_told_their_new_positions() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
                for w in ["b", "c", "d"] {
                    f.queue(&caller(w), req("board", MIN, Priority::Normal, "y"));
                }
                f.expire_and_pass(T0 + 10 * MIN_MS);
                assert_eq!(f.state(&g.lease_id), "ended");
                let pos = |f: &mut Fixture, h: &str| {
                    f.notices_of(&holder(h), NoticeKind::PositionChanged)
                        .last()
                        .and_then(|n| n.position)
                };
                assert_eq!(pos(&mut f, "c"), Some(1));
                assert_eq!(pos(&mut f, "d"), Some(2));
            }
        }

        mod on_retiring_sweep_dropping_a_request {
            use super::*;

            #[test]
            fn waiters_behind_are_told_their_new_positions() {
                let mut f = fixture();
                f.seed("board", &["u1", "u2"]);
                let pin = |unit: &str, purpose: &str| AcquireRequest {
                    target: Target::Unit(rid("board"), uid(unit)),
                    ..req("board", 10 * MIN, Priority::Normal, purpose)
                };
                let g = f.grant(&caller("a"), pin("u1", "x"));
                f.grant(&caller("d"), pin("u2", "x"));
                f.queue(&caller("b"), pin("u1", "y"));
                f.queue(&caller("c"), req("board", MIN, Priority::Normal, "z"));
                let admin = caller("admin");
                f.store
                    .tx(|tx| f.inv.retire_unit(tx, &admin, &rid("board"), &uid("u1")))
                    .unwrap();
                f.release(&caller("a"), &g.lease_id).unwrap();
                f.confirm(&caller("a"), &g.lease_id, Confirm::Clean)
                    .unwrap();
                assert_eq!(f.count(&holder("b"), NoticeKind::QueueDropped), 1);
                let n = f.notices_of(&holder("c"), NoticeKind::PositionChanged);
                assert_eq!(n.last().and_then(|n| n.position), Some(1));
            }
        }

        mod on_lapsed_offer_with_three_waiters {
            use super::*;

            #[test]
            fn waiters_behind_are_told_their_new_positions() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
                for w in ["b", "c", "d", "e"] {
                    f.queue(&caller(w), req("board", MIN, Priority::Normal, "y"));
                }
                f.release(&caller("a"), &g.lease_id).unwrap();
                f.confirm(&caller("a"), &g.lease_id, Confirm::Clean)
                    .unwrap();
                // b was offered; it never claims.
                f.at(T0 + 6 * MIN_MS);
                f.tick();
                let offer = f.offered_lease(&holder("c"));
                f.claim(&caller("c"), &offer);
                let last = |f: &mut Fixture, h: &str| {
                    f.notices_of(&holder(h), NoticeKind::PositionChanged)
                        .last()
                        .and_then(|n| n.position)
                };
                assert_eq!(last(&mut f, "d"), Some(1));
                assert_eq!(last(&mut f, "e"), Some(2));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Soft expiry: an agent's lease past its end with nobody waiting stays held
// ---------------------------------------------------------------------------

mod soft_expiry {
    use super::*;

    fn agent(session: &str) -> Caller {
        Caller {
            holder: HolderRef::AgentSession {
                session_id: session.into(),
            },
            project: None,
            override_reason: None,
        }
    }

    /// `s1` holds `board/u1` for 10 minutes, and the end has passed.
    fn overdue(f: &mut Fixture) -> LeaseId {
        f.seed("board", &["u1"]);
        let g = f.grant(&agent("s1"), req("board", 10 * MIN, Priority::Normal, "x"));
        f.at(T0 + 10 * MIN_MS);
        f.tick();
        g.lease_id
    }

    fn dispatch(f: &mut Fixture) {
        f.store.tx(|tx| f.sched.dispatch(tx)).unwrap();
    }

    mod on_an_agent_lease_reaching_its_end {
        use super::*;

        #[test]
        fn with_nobody_queued_it_stays_held_and_overdue() {
            let mut f = fixture();
            let lease = overdue(&mut f);
            assert_eq!(f.column::<String>(&lease, "state"), "held");
            assert!(f.column::<bool>(&lease, "overdue"));
            assert_eq!(f.audit("lease_overdue").len(), 1);
        }

        #[test]
        fn the_timer_does_not_fire_again() {
            let mut f = fixture();
            let lease = overdue(&mut f);
            f.at(T0 + 100 * MIN_MS);
            assert_eq!(f.tick(), None);
            assert_eq!(f.column::<String>(&lease, "state"), "held");
            assert_eq!(f.audit("lease_overdue").len(), 1);
        }

        #[test]
        fn with_someone_queued_it_expires_as_before() {
            let mut f = fixture();
            f.seed("board", &["u1"]);
            let g = f.grant(&agent("s1"), req("board", 10 * MIN, Priority::Normal, "x"));
            f.queue(&caller("w"), req("board", MIN, Priority::Normal, "y"));
            f.at(T0 + 10 * MIN_MS);
            f.tick();
            assert_eq!(f.column::<String>(&g.lease_id, "state"), "cleaning");
            assert_eq!(f.column::<String>(&g.lease_id, "cleanup_cause"), "expired");
        }
    }

    mod on_another_holders_lease_reaching_its_end {
        use super::*;

        #[test]
        fn it_expires_as_before() {
            let mut f = fixture();
            f.seed("board", &["u1"]);
            let g = f.grant(&caller("a"), req("board", 10 * MIN, Priority::Normal, "x"));
            f.at(T0 + 10 * MIN_MS);
            f.tick();
            assert_eq!(f.column::<String>(&g.lease_id, "state"), "cleaning");
        }
    }

    mod on_a_request_queued_for_an_overdue_unit {
        use super::*;

        /// `w` queues for the overdue unit and the queue is settled.
        fn claimed(f: &mut Fixture) -> LeaseId {
            let lease = overdue(f);
            f.queue(&caller("w"), req("board", MIN, Priority::Normal, "y"));
            dispatch(f);
            lease
        }

        #[test]
        fn the_lease_starts_cleaning_as_abandoned_and_the_holder_is_told() {
            let mut f = fixture();
            let lease = claimed(&mut f);
            assert_eq!(f.column::<String>(&lease, "state"), "cleaning");
            assert_eq!(f.column::<String>(&lease, "cleanup_cause"), "abandoned");
            let told = f.notices_of(&agent("s1").holder, NoticeKind::CleanupWindowStarted);
            assert_eq!(told.len(), 1);
            assert!(
                told[0].action.contains("someone is waiting"),
                "{}",
                told[0].action
            );
        }

        #[test]
        fn a_holder_confirming_in_the_window_hands_over_clean() {
            let mut f = fixture();
            let lease = claimed(&mut f);
            f.confirm(&agent("s1"), &lease, Confirm::Clean).unwrap();
            dispatch(&mut f);
            let offers = f.notices_of(&caller("w").holder, NoticeKind::Offered);
            assert_eq!(offers.len(), 1);
            assert_eq!(offers[0].dirty, None);
        }

        #[test]
        fn the_window_ending_marks_the_unit_abandoned_for_the_new_owner() {
            let mut f = fixture();
            claimed(&mut f);
            f.at(T0 + 12 * MIN_MS + SEC);
            f.tick();
            dispatch(&mut f);
            let offers = f.notices_of(&caller("w").holder, NoticeKind::Offered);
            assert_eq!(offers.len(), 1);
            assert_eq!(
                offers[0].dirty.as_ref().unwrap().reason,
                DirtyReason::Abandoned
            );
            assert!(
                offers[0].action.contains("abandoned"),
                "{}",
                offers[0].action
            );
        }
    }

    mod on_an_overdue_lease {
        use super::*;

        #[test]
        fn extend_moves_the_end_and_clears_overdue() {
            let mut f = fixture();
            let lease = overdue(&mut f);
            f.at(T0 + 20 * MIN_MS);
            f.extend(&agent("s1"), &lease, T0 + 30 * MIN_MS).unwrap();
            assert!(!f.column::<bool>(&lease, "overdue"));
            assert_eq!(f.column::<i64>(&lease, "ends_at"), T0 + 30 * MIN_MS);
        }

        #[test]
        fn release_starts_a_normal_cleanup() {
            let mut f = fixture();
            let lease = overdue(&mut f);
            f.release(&agent("s1"), &lease).unwrap();
            assert_eq!(f.column::<String>(&lease, "cleanup_cause"), "released");
        }
    }
}
