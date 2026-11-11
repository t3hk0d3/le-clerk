use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use leclerk::clock::ManualClock;
use leclerk::config::Config;
use leclerk::health::HealthResult;
use leclerk::inventory::{Inventory, NewResource, NewUnit, Promotions};
use leclerk::model::{
    Caller, HealthSpec, HolderRef, LeaseId, NoticeId, Priority, ProjectKey, ResourceId,
    ResourceType, Scope, UnitId, UnitSettings,
};
use leclerk::scheduler::{
    AcquireOutcome, AcquireRequest, Confirm, Grant, HealthProbe, Scheduler, Target,
};
use leclerk::store::audit::{self, AuditEntry, AuditFilter};
use leclerk::store::notices::{self, Notice, NoticeFilter, NoticeKind};
use leclerk::store::{Store, StoreError, Tx};

const T0: i64 = 1_000_000;
const MIN_MS: i64 = 60_000;
/// The delivery time every `Fixture::take` passes.
const DELIVERED_AT: i64 = T0 + 777;
const MIN: Duration = Duration::from_secs(60);

fn rid(s: &str) -> ResourceId {
    ResourceId::parse(s).unwrap()
}

fn uid(s: &str) -> UnitId {
    UnitId::parse(s).unwrap()
}

fn holder(tag: &str) -> HolderRef {
    HolderRef::User { tag: tag.into() }
}

fn project(path: &str) -> ProjectKey {
    ProjectKey(PathBuf::from(path))
}

fn caller_in(tag: &str, path: Option<&str>) -> Caller {
    Caller {
        holder: holder(tag),
        project: path.map(project),
        override_reason: None,
    }
}

fn caller(tag: &str) -> Caller {
    caller_in(tag, None)
}

fn req(resource: &str, priority: Priority, interruptible: bool, purpose: &str) -> AcquireRequest {
    AcquireRequest {
        target: Target::Resource(rid(resource)),
        duration: 10 * MIN,
        interruptible,
        priority,
        purpose: purpose.into(),
        override_reason: None,
        skip_checks: false,
    }
}

struct Healthy;

impl HealthProbe for Healthy {
    fn check(&self, _: &HealthSpec, _: &ResourceId, _: &UnitId, _: &UnitSettings) -> HealthResult {
        HealthResult::Healthy
    }
}

struct Fixture {
    store: Store,
    inv: Inventory,
    promos: Promotions,
    sched: Scheduler,
    clock: Arc<ManualClock>,
}

fn fixture() -> Fixture {
    let clock = Arc::new(ManualClock::new(T0));
    Fixture {
        store: Store::open_in_memory().unwrap(),
        inv: Inventory::new(clock.clone()),
        promos: Promotions::new(clock.clone()),
        sched: Scheduler::new(clock.clone(), Config::default(), Arc::new(Healthy)),
        clock,
    }
}

impl Fixture {
    fn seed(&mut self, who: &Caller, id: &str, scope: &Scope, key: &str) {
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
                self.inv.add_unit(
                    tx,
                    who,
                    &rid(id),
                    &NewUnit {
                        id: uid("u1"),
                        settings: UnitSettings::Generic {
                            identity_key: key.into(),
                        },
                        description: String::new(),
                    },
                )?;
                Ok(())
            })
            .unwrap();
    }

    fn seed_user(&mut self, id: &str) {
        self.seed(&caller("admin"), id, &Scope::User, id);
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

    fn confirm(&mut self, who: &Caller, lease: &LeaseId, how: Confirm) {
        self.store
            .tx(|tx| self.sched.confirm(tx, who, lease, how))
            .unwrap();
    }

    fn with_tx<T>(&mut self, f: impl FnOnce(&Tx) -> T) -> T {
        self.store
            .tx(|tx| -> Result<T, StoreError> { Ok(f(tx)) })
            .unwrap()
    }

    fn take(&mut self, who: &HolderRef, limit: u32) -> Vec<Notice> {
        self.with_tx(|tx| notices::take(tx, who, limit, DELIVERED_AT).unwrap())
    }

    fn tick(&mut self) {
        self.store.tx(|tx| self.sched.tick(tx)).unwrap();
    }

    fn delivered_at(&mut self, n: &Notice) -> Option<i64> {
        self.with_tx(|tx| {
            tx.conn()
                .query_row(
                    "SELECT delivered_at FROM notices WHERE notice_id = ?1",
                    [n.id.as_str()],
                    |r| r.get(0),
                )
                .unwrap()
        })
    }

    fn publish(&mut self, n: &Notice) {
        self.with_tx(|tx| notices::publish(tx, n).unwrap());
    }

    fn audit(&mut self, filter: &AuditFilter) -> Vec<AuditEntry> {
        self.with_tx(|tx| audit::list(tx, filter).unwrap())
    }

    fn audit_of(&mut self, action: &str) -> Vec<AuditEntry> {
        self.audit(&AuditFilter {
            action: Some(action.into()),
            ..AuditFilter::default()
        })
    }
}

fn notice(to: &HolderRef, kind: NoticeKind, created_at: i64) -> Notice {
    Notice::new(to.clone(), kind, "do it".into(), created_at)
}

mod notice_queue {
    use super::*;

    mod publish {
        use super::*;

        mod on_scheduler_event {
            use super::*;

            #[test]
            fn stores_the_notice_for_its_holder_with_kind_lease_unit_deadline_and_action() {
                let mut f = fixture();
                f.seed_user("board");
                let a = f
                    .grant(&caller("a"), req("board", Priority::Normal, true, "mine"))
                    .lease_id;
                f.clock.set(T0 + MIN_MS);
                f.acquire(&caller("b"), req("board", Priority::High, false, "theirs"));
                let got = f.take(&holder("a"), 10);
                assert_eq!(got.len(), 1);
                assert_eq!(got[0].kind, NoticeKind::YieldRequested);
                assert_eq!(got[0].holder, holder("a"));
                assert_eq!(got[0].lease_id.as_ref(), Some(&a));
                assert_eq!(got[0].resource, Some(rid("board")));
                assert_eq!(got[0].unit, Some(uid("u1")));
                assert_eq!(got[0].deadline, Some(T0 + MIN_MS + 2 * MIN_MS));
                assert_ne!(got[0].action, "");
            }

            #[test]
            fn fills_other_purpose_when_the_other_party_is_visible() {
                let mut f = fixture();
                let a = caller_in("a", Some("/a"));
                let scope = Scope::Project(project("/a"));
                f.seed(&a, "aboard", &scope, "k");
                f.grant(&a, req("aboard", Priority::Normal, true, "mine"));
                f.clock.set(T0 + MIN_MS);
                f.acquire(
                    &caller_in("b", Some("/a")),
                    req("aboard", Priority::High, false, "theirs"),
                );
                let got = f.take(&holder("a"), 10);
                assert_eq!(got.len(), 1);
                assert_eq!(got[0].other_purpose.as_deref(), Some("theirs"));
            }

            #[test]
            fn omits_other_purpose_when_the_other_party_is_in_another_project() {
                let mut f = fixture();
                let a = caller_in("a", Some("/a"));
                let b = caller_in("b", Some("/b"));
                f.seed(&a, "aboard", &Scope::Project(project("/a")), "k");
                f.seed(&b, "bboard", &Scope::Project(project("/b")), "k");
                f.grant(&a, req("aboard", Priority::Normal, true, "mine"));
                f.clock.set(T0 + MIN_MS);
                f.acquire(&b, req("bboard", Priority::High, false, "secret"));
                let got = f.take(&holder("a"), 10);
                assert_eq!(got.len(), 1);
                assert_eq!(got[0].kind, NoticeKind::YieldRequested);
                assert_eq!(got[0].other_purpose, None);
            }
        }
    }

    mod take {
        use super::*;

        mod on_pending_notices {
            use super::*;

            #[test]
            fn returns_them_in_insertion_order_whatever_their_clock_or_id_says() {
                let mut f = fixture();
                let h = holder("h");
                // Inserted newest first, with ids whose ULID order is the
                // exact reverse of the expected order.
                let mut late = notice(&h, NoticeKind::LeaseEnded, T0 + 20);
                let mut tie_a = notice(&h, NoticeKind::Offered, T0 + 10);
                let mut tie_b = notice(&h, NoticeKind::QueueDropped, T0 + 10);
                late.id = NoticeId::parse("ntc_00000000000000000000000001").unwrap();
                tie_b.id = NoticeId::parse("ntc_00000000000000000000000002").unwrap();
                tie_a.id = NoticeId::parse("ntc_00000000000000000000000003").unwrap();
                for n in [&late, &tie_a, &tie_b] {
                    f.publish(n);
                }
                let ids: Vec<_> = f.take(&h, 10).into_iter().map(|n| n.id).collect();
                assert_eq!(ids, vec![late.id, tie_a.id, tie_b.id]);
            }

            #[test]
            fn marks_them_delivered_so_a_second_take_returns_nothing() {
                let mut f = fixture();
                let h = holder("h");
                f.publish(&notice(&h, NoticeKind::Offered, T0));
                assert_eq!(f.take(&h, 10).len(), 1);
                assert_eq!(f.take(&h, 10).len(), 0);
            }

            #[test]
            fn records_the_given_delivery_time() {
                let mut f = fixture();
                let h = holder("h");
                let n = notice(&h, NoticeKind::Offered, T0);
                f.publish(&n);
                assert_eq!(f.delivered_at(&n), None);
                f.take(&h, 10);
                assert_eq!(f.delivered_at(&n), Some(DELIVERED_AT));
            }

            #[test]
            fn marks_them_delivered_for_pending_too() {
                let mut f = fixture();
                let h = holder("h");
                f.publish(&notice(&h, NoticeKind::Offered, T0));
                f.take(&h, 10);
                assert_eq!(f.with_tx(|tx| notices::pending(tx, &h).unwrap()).len(), 0);
            }

            #[test]
            fn returns_the_notice_unchanged() {
                let mut f = fixture();
                let h = holder("h");
                let mut n = notice(&h, NoticeKind::PositionChanged, T0);
                n.position = Some(3);
                n.other_purpose = Some("why".into());
                f.publish(&n);
                assert_eq!(f.take(&h, 10), vec![n]);
            }
        }

        mod on_more_pending_than_the_limit {
            use super::*;

            #[test]
            fn returns_only_the_oldest_up_to_the_limit() {
                let mut f = fixture();
                let h = holder("h");
                let first = notice(&h, NoticeKind::Offered, T0);
                f.publish(&first);
                f.publish(&notice(&h, NoticeKind::LeaseEnded, T0 + 1));
                f.publish(&notice(&h, NoticeKind::LeaseEnded, T0 + 2));
                let got = f.take(&h, 1);
                assert_eq!(got.len(), 1);
                assert_eq!(got[0].id, first.id);
            }

            #[test]
            fn leaves_the_rest_undelivered() {
                let mut f = fixture();
                let h = holder("h");
                for i in 0..3 {
                    f.publish(&notice(&h, NoticeKind::LeaseEnded, T0 + i));
                }
                f.take(&h, 2);
                assert_eq!(f.take(&h, 10).len(), 1);
            }
        }

        mod on_a_damaged_row_beyond_the_first_page {
            use super::*;

            #[test]
            fn still_takes_the_first_page() {
                let mut f = fixture();
                let h = holder("h");
                for i in 0..3 {
                    f.publish(&notice(&h, NoticeKind::LeaseEnded, T0 + i));
                }
                insert_damaged(&mut f, &h, T0 + 10);
                assert_eq!(f.take(&h, 3).len(), 3);
            }
        }

        mod on_none_pending {
            use super::*;

            #[test]
            fn returns_empty() {
                let mut f = fixture();
                assert_eq!(f.take(&holder("nobody"), 10).len(), 0);
            }
        }

        mod on_notices_of_other_holders {
            use super::*;

            #[test]
            fn returns_only_the_requested_holders_notices_and_leaves_the_others() {
                let mut f = fixture();
                f.publish(&notice(&holder("a"), NoticeKind::Offered, T0));
                f.publish(&notice(&holder("b"), NoticeKind::Offered, T0));
                assert_eq!(f.take(&holder("a"), 10).len(), 1);
                assert_eq!(f.take(&holder("b"), 10).len(), 1);
            }

            #[test]
            fn does_not_match_a_different_holder_kind_with_the_same_text() {
                let mut f = fixture();
                let agent = HolderRef::AgentSession {
                    session_id: "x".into(),
                };
                f.publish(&notice(&holder("x"), NoticeKind::Offered, T0));
                assert_eq!(f.take(&agent, 10).len(), 0);
                assert_eq!(f.take(&holder("x"), 10).len(), 1);
            }

            #[test]
            fn matches_a_cli_process_by_pid_and_start_time() {
                let mut f = fixture();
                let p = HolderRef::CliProcess {
                    pid: 7,
                    start_time: 100,
                    boot_id: None,
                };
                let other = HolderRef::CliProcess {
                    pid: 7,
                    start_time: 101,
                    boot_id: None,
                };
                f.publish(&notice(&p, NoticeKind::Offered, T0));
                assert_eq!(f.take(&other, 10).len(), 0);
                assert_eq!(f.take(&p, 10).len(), 1);
            }
        }
    }
}

/// A row of `h` whose payload is not a notice, created at `at`.
fn insert_damaged(f: &mut Fixture, h: &HolderRef, at: i64) {
    let holder = serde_json::to_string(h).unwrap();
    f.with_tx(|tx| {
        tx.conn()
            .execute(
                "INSERT INTO notices (notice_id, holder, kind, payload, created_at) \
                 VALUES ('ntc_bad', ?1, 'offered', 'not json', ?2)",
                rusqlite::params![holder, at],
            )
            .unwrap();
    });
}

mod take_matching {
    use super::*;

    fn filter_for(lease: &LeaseId) -> NoticeFilter {
        NoticeFilter {
            leases: vec![lease.clone()],
            requests: Vec::new(),
            kinds: None,
        }
    }

    fn take_for(f: &mut Fixture, h: &HolderRef, lease: &LeaseId, limit: u32) -> Vec<Notice> {
        let filter = filter_for(lease);
        f.with_tx(|tx| notices::take_matching(tx, h, Some(&filter), limit, DELIVERED_AT).unwrap())
    }

    fn about(h: &HolderRef, lease: &LeaseId, at: i64) -> Notice {
        let mut n = notice(h, NoticeKind::LeaseEnded, at);
        n.lease_id = Some(lease.clone());
        n
    }

    mod on_a_filter_with_kinds {
        use super::*;

        #[test]
        fn returns_only_notices_of_those_kinds() {
            let mut f = fixture();
            let h = holder("h");
            let ours = LeaseId::new();
            let ended = about(&h, &ours, T0 + 1);
            let mut warning = about(&h, &ours, T0);
            warning.kind = NoticeKind::ExpiryWarning;
            f.publish(&warning);
            f.publish(&ended);
            let filter = NoticeFilter {
                kinds: Some(vec![NoticeKind::LeaseEnded]),
                ..filter_for(&ours)
            };
            let got = f.with_tx(|tx| notices::returned(tx, &h, Some(&filter), 10, T0).unwrap());
            assert_eq!(got.iter().map(|n| &n.id).collect::<Vec<_>>(), [&ended.id]);
        }
    }

    mod on_a_filter {
        use super::*;

        #[test]
        fn takes_only_matching_notices_up_to_the_limit_oldest_first() {
            let mut f = fixture();
            let h = holder("h");
            let (ours, other) = (LeaseId::new(), LeaseId::new());
            let first = about(&h, &ours, T0 + 1);
            f.publish(&about(&h, &other, T0));
            f.publish(&first);
            f.publish(&about(&h, &ours, T0 + 2));
            let got = take_for(&mut f, &h, &ours, 1);
            assert_eq!(got.iter().map(|n| &n.id).collect::<Vec<_>>(), [&first.id]);
        }

        #[test]
        fn leaves_the_others_pending() {
            let mut f = fixture();
            let h = holder("h");
            let (ours, other) = (LeaseId::new(), LeaseId::new());
            f.publish(&about(&h, &other, T0));
            f.publish(&about(&h, &ours, T0 + 1));
            take_for(&mut f, &h, &ours, 10);
            let left = f.with_tx(|tx| notices::pending(tx, &h).unwrap());
            assert_eq!(left.len(), 1);
            assert_eq!(left[0].lease_id, Some(other));
        }
    }

    mod on_a_damaged_row_that_does_not_match {
        use super::*;

        #[test]
        fn still_takes_the_matching_notices() {
            let mut f = fixture();
            let h = holder("h");
            let ours = LeaseId::new();
            insert_damaged(&mut f, &h, T0);
            f.publish(&about(&h, &ours, T0 + 1));
            assert_eq!(take_for(&mut f, &h, &ours, 10).len(), 1);
        }
    }
}

mod holders_after {
    use super::*;

    mod on_an_unreadable_holder_row {
        use super::*;

        #[test]
        fn skips_it_and_returns_the_readable_holders() {
            let mut store = Store::open_in_memory().unwrap();
            let good = HolderRef::User { tag: "good".into() };
            let holders = store
                .tx(|tx| {
                    tx.conn().execute(
                        "INSERT INTO notices (notice_id, holder, kind, payload, created_at) \
                         VALUES ('ntc_bad', 'not json', 'offered', '{}', 1)",
                        [],
                    )?;
                    notices::publish(
                        tx,
                        &Notice::new(good.clone(), NoticeKind::Offered, "claim".into(), 2),
                    )?;
                    notices::holders_after(tx, 0)
                })
                .unwrap();
            assert_eq!(holders, vec![good]);
        }
    }
}

mod audit_log {
    use super::*;

    fn record(f: &mut Fixture, who: &Caller, at: i64, action: &str) {
        f.with_tx(|tx| audit::record(tx, at, who, action, &serde_json::json!({})).unwrap());
    }

    mod list {
        use super::*;

        mod on_several_entries {
            use super::*;

            #[test]
            fn returns_newest_first() {
                let mut f = fixture();
                for (i, a) in ["one", "two", "three"].iter().enumerate() {
                    record(&mut f, &caller("a"), T0 + i64::try_from(i).unwrap(), a);
                }
                let actions: Vec<_> = f
                    .audit(&AuditFilter::default())
                    .into_iter()
                    .map(|e| e.action)
                    .collect();
                assert_eq!(actions, ["three", "two", "one"]);
            }

            #[test]
            fn carries_time_actor_action_detail_and_override_reason() {
                let mut f = fixture();
                let who = Caller {
                    override_reason: Some("because".into()),
                    ..caller("a")
                };
                f.with_tx(|tx| {
                    audit::record(tx, T0, &who, "thing", &serde_json::json!({"k": 1})).unwrap();
                });
                let e = f.audit(&AuditFilter::default()).remove(0);
                assert_eq!(e.at, T0);
                assert_eq!(e.actor, holder("a"));
                assert_eq!(e.action, "thing");
                assert_eq!(e.detail, serde_json::json!({"k": 1}));
                assert_eq!(e.override_reason.as_deref(), Some("because"));
            }
        }

        mod with_a_since_filter {
            use super::*;

            #[test]
            fn returns_only_entries_at_or_after_since() {
                let mut f = fixture();
                record(&mut f, &caller("a"), T0, "old");
                record(&mut f, &caller("a"), T0 + 5, "edge");
                record(&mut f, &caller("a"), T0 + 9, "new");
                let actions: Vec<_> = f
                    .audit(&AuditFilter {
                        since: Some(T0 + 5),
                        ..AuditFilter::default()
                    })
                    .into_iter()
                    .map(|e| e.action)
                    .collect();
                assert_eq!(actions, ["new", "edge"]);
            }
        }

        mod with_an_actor_filter {
            use super::*;

            #[test]
            fn returns_only_that_actors_entries() {
                let mut f = fixture();
                record(&mut f, &caller("a"), T0, "by_a");
                record(&mut f, &caller("b"), T0 + 1, "by_b");
                let got = f.audit(&AuditFilter {
                    actor: Some(holder("a")),
                    ..AuditFilter::default()
                });
                assert_eq!(got.len(), 1);
                assert_eq!(got[0].action, "by_a");
            }

            #[test]
            fn does_not_match_another_holder_kind_with_the_same_text() {
                let mut f = fixture();
                record(&mut f, &caller("x"), T0, "by_user");
                let got = f.audit(&AuditFilter {
                    actor: Some(HolderRef::AgentSession {
                        session_id: "x".into(),
                    }),
                    ..AuditFilter::default()
                });
                assert_eq!(got.len(), 0);
            }
        }

        mod with_an_action_filter {
            use super::*;

            #[test]
            fn returns_only_that_action() {
                let mut f = fixture();
                record(&mut f, &caller("a"), T0, "x");
                record(&mut f, &caller("a"), T0 + 1, "y");
                let got = f.audit_of("y");
                assert_eq!(got.len(), 1);
                assert_eq!(got[0].action, "y");
            }
        }

        mod with_a_limit {
            use super::*;

            fn many(f: &mut Fixture, n: i64) {
                f.with_tx(|tx| {
                    for i in 0..n {
                        audit::record(tx, T0 + i, &caller("a"), "x", &serde_json::json!({}))
                            .unwrap();
                    }
                });
            }

            #[test]
            fn returns_at_most_that_many_newest() {
                let mut f = fixture();
                many(&mut f, 5);
                let got = f.audit(&AuditFilter {
                    limit: Some(2),
                    ..AuditFilter::default()
                });
                let ats: Vec<_> = got.iter().map(|e| e.at).collect();
                assert_eq!(ats, [T0 + 4, T0 + 3]);
            }

            #[test]
            fn defaults_to_100() {
                let mut f = fixture();
                many(&mut f, 150);
                assert_eq!(f.audit(&AuditFilter::default()).len(), 100);
            }

            #[test]
            fn clamps_to_1000() {
                let mut f = fixture();
                many(&mut f, 1100);
                let got = f.audit(&AuditFilter {
                    limit: Some(5000),
                    ..AuditFilter::default()
                });
                assert_eq!(got.len(), 1000);
            }
        }
    }

    mod record_kinds {
        use super::*;

        #[test]
        fn lists_an_inventory_change() {
            let mut f = fixture();
            f.seed_user("board");
            let got = f.audit_of("resource_created");
            assert_eq!(got.len(), 1);
            assert_eq!(got[0].actor, holder("admin"));
        }

        #[test]
        fn lists_a_grant() {
            let mut f = fixture();
            f.seed_user("board");
            f.grant(&caller("a"), req("board", Priority::Normal, false, "x"));
            let got = f.audit_of("granted");
            assert_eq!(got.len(), 1);
            assert_eq!(got[0].actor, holder("a"));
            assert_eq!(got[0].at, T0);
        }

        #[test]
        fn lists_a_handover_with_cleanup_confirmed() {
            let mut f = fixture();
            f.seed_user("board");
            let lease = f
                .grant(&caller("a"), req("board", Priority::Normal, true, "x"))
                .lease_id;
            f.acquire(&caller("b"), req("board", Priority::High, false, "y"));
            f.confirm(&caller("a"), &lease, Confirm::Yielded);
            let b_lease = f
                .take(&holder("b"), 10)
                .into_iter()
                .find_map(|n| n.lease_id)
                .unwrap();
            f.store
                .tx(|tx| f.sched.claim(tx, &caller("b"), &b_lease))
                .unwrap();
            f.store
                .tx(|tx| f.sched.release(tx, &caller("b"), &b_lease))
                .unwrap();
            f.confirm(&caller("b"), &b_lease, Confirm::Clean);
            let got = f.audit_of("unit_returned");
            assert_eq!(got.len(), 1);
            assert_eq!(got[0].detail["cleanup_confirmed"], true);
        }

        #[test]
        fn lists_a_handover_with_cleanup_not_confirmed() {
            let mut f = fixture();
            f.seed_user("board");
            let lease = f
                .grant(&caller("a"), req("board", Priority::Normal, true, "x"))
                .lease_id;
            f.acquire(&caller("b"), req("board", Priority::High, false, "y"));
            f.confirm(&caller("a"), &lease, Confirm::Yielded);
            let b_lease = f
                .take(&holder("b"), 10)
                .into_iter()
                .find_map(|n| n.lease_id)
                .unwrap();
            f.store
                .tx(|tx| f.sched.claim(tx, &caller("b"), &b_lease))
                .unwrap();
            f.store
                .tx(|tx| f.sched.release(tx, &caller("b"), &b_lease))
                .unwrap();
            f.clock.set(T0 + 3 * MIN_MS);
            f.tick();
            let got = f.audit_of("unit_returned");
            assert_eq!(got.len(), 1);
            assert_eq!(got[0].detail["cleanup_confirmed"], false);
        }

        #[test]
        fn lists_a_yield_ask_and_its_answer() {
            let mut f = fixture();
            f.seed_user("board");
            let lease = f
                .grant(&caller("a"), req("board", Priority::Normal, true, "x"))
                .lease_id;
            f.acquire(&caller("b"), req("board", Priority::High, false, "y"));
            assert_eq!(f.audit_of("yield_requested").len(), 1);
            f.confirm(&caller("a"), &lease, Confirm::Yielded);
            let answer = f.audit_of("yielded");
            assert_eq!(answer.len(), 1);
            assert_eq!(answer[0].actor, holder("a"));
        }

        #[test]
        fn lists_an_override_with_its_reason() {
            let mut f = fixture();
            f.seed_user("board");
            f.grant(&caller("a"), req("board", Priority::Normal, false, "x"));
            let mut over = req("board", Priority::Normal, false, "y");
            over.override_reason = Some("user said so".into());
            f.acquire(&caller("o"), over);
            let got = f.audit_of("override");
            assert_eq!(got.len(), 1);
            assert_eq!(got[0].actor, holder("o"));
            assert_eq!(got[0].detail["reason"], "user said so");
        }

        #[test]
        fn lists_a_promotion_with_the_user_request() {
            let mut f = fixture();
            let b = caller_in("b", Some("/b"));
            let a = caller_in("a", Some("/a"));
            f.seed(&b, "theirs", &Scope::Project(project("/b")), "k");
            f.seed(&a, "mine", &Scope::Project(project("/a")), "k");
            let id = f.store.tx(|tx| f.promos.list(tx)).unwrap().remove(0).id;
            let admin = Caller {
                override_reason: Some("share it".into()),
                ..caller("admin")
            };
            f.store
                .tx(|tx| f.promos.approve(tx, &admin, id, None))
                .unwrap();
            let got = f.audit_of("promotion_approved");
            assert_eq!(got.len(), 1);
            assert_eq!(got[0].override_reason.as_deref(), Some("share it"));
        }
    }
}

mod holder_ref_encoding {
    use super::*;

    fn json(h: &HolderRef) -> String {
        serde_json::to_string(h).unwrap()
    }

    #[test]
    fn agent_session_is_kind_then_session_id() {
        let h = HolderRef::AgentSession {
            session_id: "s1".into(),
        };
        assert_eq!(json(&h), r#"{"kind":"agent_session","session_id":"s1"}"#);
    }

    #[test]
    fn cli_process_is_kind_then_pid_then_start_time() {
        let h = HolderRef::CliProcess {
            pid: 7,
            start_time: 100,
            boot_id: None,
        };
        assert_eq!(
            json(&h),
            r#"{"kind":"cli_process","pid":7,"start_time":100}"#
        );
    }

    #[test]
    fn user_is_kind_then_tag() {
        assert_eq!(json(&holder("t")), r#"{"kind":"user","tag":"t"}"#);
    }
}

mod notice_delivery {
    use super::*;

    impl Fixture {
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

        fn extend(&mut self, who: &Caller, lease: &LeaseId, new_ends_at: i64) {
            self.store
                .tx(|tx| self.sched.extend(tx, who, lease, new_ends_at))
                .unwrap();
        }
    }

    mod on_cleanup_window_started_for_a_lease_already_confirmed {
        use super::*;

        /// `a` released its lease and confirmed the cleanup at once, so the
        /// cleanup notice it was sent is out of date.
        fn confirmed() -> (Fixture, LeaseId) {
            let mut f = fixture();
            f.seed_user("board");
            let a = caller("a");
            let lease = f
                .grant(&a, req("board", Priority::Normal, false, "mine"))
                .lease_id;
            f.release(&a, &lease);
            f.confirm(&a, &lease, Confirm::Clean);
            (f, lease)
        }

        #[test]
        fn is_not_returned() {
            let (mut f, _) = confirmed();
            let got = f.take(&holder("a"), 10);
            assert!(
                !got.iter()
                    .any(|n| n.kind == NoticeKind::CleanupWindowStarted),
                "{got:?}"
            );
        }

        #[test]
        fn is_not_returned_by_returned_either() {
            let (mut f, _) = confirmed();
            let h = holder("a");
            let got = f.with_tx(|tx| notices::returned(tx, &h, None, 10, DELIVERED_AT).unwrap());
            assert!(
                !got.iter()
                    .any(|n| n.kind == NoticeKind::CleanupWindowStarted),
                "{got:?}"
            );
        }

        #[test]
        fn is_audited_as_superseded() {
            let (mut f, lease) = confirmed();
            f.take(&holder("a"), 10);
            let got = f.audit_of("superseded");
            assert_eq!(got.len(), 1, "{got:?}");
            assert_eq!(got[0].detail["kind"], "cleanup_window_started");
            assert_eq!(got[0].detail["lease"], lease.as_str());
        }

        #[test]
        fn is_marked_delivered() {
            let (mut f, _) = confirmed();
            f.take(&holder("a"), 10);
            assert_eq!(
                f.with_tx(|tx| notices::pending(tx, &holder("a")).unwrap()),
                Vec::new()
            );
        }

        #[test]
        fn is_audited_once_however_often_taken() {
            let (mut f, _) = confirmed();
            f.take(&holder("a"), 10);
            f.take(&holder("a"), 10);
            assert_eq!(f.audit_of("superseded").len(), 1);
        }
    }

    mod on_cleanup_window_started_for_a_lease_still_cleaning {
        use super::*;

        #[test]
        fn is_returned() {
            let mut f = fixture();
            f.seed_user("board");
            let a = caller("a");
            let lease = f
                .grant(&a, req("board", Priority::Normal, false, "mine"))
                .lease_id;
            f.release(&a, &lease);
            let got = f.take(&holder("a"), 10);
            assert!(
                got.iter()
                    .any(|n| n.kind == NoticeKind::CleanupWindowStarted),
                "{got:?}"
            );
            assert_eq!(f.audit_of("superseded"), Vec::new());
        }
    }

    mod on_offered_for_a_lease_already_claimed {
        use super::*;

        #[test]
        fn is_not_returned() {
            let mut f = fixture();
            f.seed_user("board");
            let (a, b) = (caller("a"), caller("b"));
            let first = f
                .grant(&a, req("board", Priority::Normal, false, "mine"))
                .lease_id;
            f.acquire(&b, req("board", Priority::Normal, false, "theirs"));
            f.release(&a, &first);
            f.confirm(&a, &first, Confirm::Clean);
            let offer = f.with_tx(|tx| {
                notices::pending(tx, &holder("b"))
                    .unwrap()
                    .into_iter()
                    .find(|n| n.kind == NoticeKind::Offered)
                    .and_then(|n| n.lease_id)
                    .unwrap()
            });
            f.claim(&b, &offer);
            let got = f.take(&holder("b"), 10);
            assert!(
                !got.iter().any(|n| n.kind == NoticeKind::Offered),
                "{got:?}"
            );
            assert_eq!(f.audit_of("superseded").len(), 1);
        }
    }

    mod on_expiry_warning_for_an_extended_lease_whose_new_end_is_beyond_the_warning {
        use super::*;

        /// `a`'s lease (10 minutes) was warned of its end, then extended.
        fn warned() -> (Fixture, LeaseId) {
            let mut f = fixture();
            f.seed_user("board");
            let lease = f
                .grant(&caller("a"), req("board", Priority::Normal, false, "mine"))
                .lease_id;
            f.clock.set(T0 + 9 * MIN_MS);
            f.tick();
            (f, lease)
        }

        #[test]
        fn is_not_returned() {
            let (mut f, lease) = warned();
            f.extend(&caller("a"), &lease, T0 + 30 * MIN_MS);
            let got = f.take(&holder("a"), 10);
            assert!(
                !got.iter().any(|n| n.kind == NoticeKind::ExpiryWarning),
                "{got:?}"
            );
            assert_eq!(f.audit_of("superseded").len(), 1);
        }

        #[test]
        fn is_returned_while_the_lease_is_not_extended() {
            let (mut f, _) = warned();
            let got = f.take(&holder("a"), 10);
            assert!(
                got.iter().any(|n| n.kind == NoticeKind::ExpiryWarning),
                "{got:?}"
            );
        }
    }

    mod on_expiry_warning_for_a_lease_that_ended {
        use super::*;

        #[test]
        fn is_not_returned() {
            let mut f = fixture();
            f.seed_user("board");
            let a = caller("a");
            let lease = f
                .grant(&a, req("board", Priority::Normal, false, "mine"))
                .lease_id;
            f.clock.set(T0 + 9 * MIN_MS);
            f.tick();
            f.release(&a, &lease);
            f.confirm(&a, &lease, Confirm::Clean);
            let got = f.take(&holder("a"), 10);
            assert!(
                !got.iter().any(|n| n.kind == NoticeKind::ExpiryWarning),
                "{got:?}"
            );
        }
    }

    mod on_expiry_warning_for_a_lease_overdue_under_soft_expiry {
        use super::*;

        #[test]
        fn is_returned() {
            let mut f = fixture();
            f.seed_user("board");
            let a = HolderRef::AgentSession {
                session_id: "s1".into(),
            };
            let who = Caller {
                holder: a.clone(),
                project: None,
                override_reason: None,
            };
            f.grant(&who, req("board", Priority::Normal, false, "mine"));
            f.clock.set(T0 + 9 * MIN_MS);
            f.tick();
            f.clock.set(T0 + 11 * MIN_MS);
            f.tick();
            let got = f.take(&a, 10);
            assert!(
                got.iter().any(|n| n.kind == NoticeKind::ExpiryWarning),
                "{got:?}"
            );
        }
    }
}
