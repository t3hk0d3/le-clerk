mod common;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use leclerk::clock::ManualClock;
use leclerk::config::Config;
use leclerk::health::HealthResult;
use leclerk::inventory::{Inventory, NewResource, NewUnit, ResourcePatch, UnitPatch, View};
use leclerk::model::{
    Caller, DirtyInfo, DirtyReason, HealthSpec, HolderRef, LeaseId, LeaseState, Priority,
    ProjectKey, RequestId, ResourceId, ResourceType, Scope, Tag, TagSet, UnitId, UnitSettings,
};
use leclerk::scheduler::{
    AcquireOutcome, AcquireRequest, Confirm, Grant, HealthProbe, Occupancy, OtherParty, Scheduler,
    SchedulerError, Target, notify,
};
use leclerk::store::notices::{self, Notice, NoticeKind};
use leclerk::store::{Store, StoreError, Tx, audit};

const T0: i64 = 1_000_000;
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

fn req(target: Target, duration: Duration, priority: Priority, purpose: &str) -> AcquireRequest {
    AcquireRequest {
        target,
        duration,
        interruptible: false,
        priority,
        purpose: purpose.into(),
        override_reason: None,
        skip_checks: false,
    }
}

fn on(resource: &str) -> Target {
    Target::Resource(rid(resource))
}

fn on_unit(resource: &str, unit: &str) -> Target {
    Target::Unit(rid(resource), uid(unit))
}

/// A probe that reports the listed unit ids unhealthy and counts calls.
#[derive(Default)]
struct StubProbe {
    unhealthy: Mutex<HashSet<String>>,
    calls: Mutex<Vec<String>>,
}

impl StubProbe {
    fn fail(&self, unit: &str) {
        self.unhealthy.lock().unwrap().insert(unit.to_owned());
    }

    fn heal(&self, unit: &str) {
        self.unhealthy.lock().unwrap().remove(unit);
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl HealthProbe for StubProbe {
    fn check(
        &self,
        _spec: &HealthSpec,
        _resource: &ResourceId,
        unit: &UnitId,
        _settings: &UnitSettings,
    ) -> HealthResult {
        self.calls.lock().unwrap().push(unit.as_str().to_owned());
        if self.unhealthy.lock().unwrap().contains(unit.as_str()) {
            HealthResult::Unhealthy("stub says no".into())
        } else {
            HealthResult::Healthy
        }
    }
}

struct Fixture {
    store: Store,
    inv: Inventory,
    sched: Scheduler,
    clock: Arc<ManualClock>,
    probe: Arc<StubProbe>,
}

fn fixture() -> Fixture {
    let clock = Arc::new(ManualClock::new(T0));
    let probe = Arc::new(StubProbe::default());
    Fixture {
        store: Store::open_in_memory().unwrap(),
        inv: Inventory::new(clock.clone()),
        sched: Scheduler::new(clock.clone(), Config::default(), probe.clone()),
        clock,
        probe,
    }
}

fn generic_settings(key: &str) -> UnitSettings {
    UnitSettings::Generic {
        identity_key: key.into(),
    }
}

impl Fixture {
    /// A generic resource in `scope` with `units`; each unit's identity key is
    /// `key_prefix-unit`, so equal prefixes across projects link units.
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
                            settings: generic_settings(&format!("{key}-{u}")),
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

    fn acquire(
        &mut self,
        who: &Caller,
        r: AcquireRequest,
    ) -> Result<AcquireOutcome, SchedulerError> {
        self.store.tx(|tx| self.sched.acquire(tx, who, r))
    }

    fn grant(&mut self, who: &Caller, r: AcquireRequest) -> Grant {
        match self.acquire(who, r).unwrap() {
            AcquireOutcome::Granted(g) => g,
            other => panic!("expected grant, got {other:?}"),
        }
    }

    fn queue(&mut self, who: &Caller, r: AcquireRequest) -> (RequestId, u32) {
        match self.acquire(who, r).unwrap() {
            AcquireOutcome::Queued {
                request_id,
                position,
                ..
            } => (request_id, position),
            other => panic!("expected queued, got {other:?}"),
        }
    }

    fn dispatch(&mut self) {
        self.store.tx(|tx| self.sched.dispatch(tx)).unwrap();
    }

    fn with_tx<T>(&mut self, f: impl FnOnce(&Tx) -> T) -> T {
        self.store
            .tx(|tx| -> Result<T, StoreError> { Ok(f(tx)) })
            .unwrap()
    }

    /// End a lease directly in storage (release arrives in a later task).
    fn end(&mut self, lease: &LeaseId) {
        self.with_tx(|tx| {
            tx.conn()
                .execute(
                    "UPDATE leases SET state = 'ended' WHERE lease_id = ?1",
                    [lease.as_str()],
                )
                .unwrap();
        });
    }

    fn lease_state(&mut self, lease: &LeaseId) -> String {
        self.with_tx(|tx| {
            tx.conn()
                .query_row(
                    "SELECT state FROM leases WHERE lease_id = ?1",
                    [lease.as_str()],
                    |r| r.get(0),
                )
                .unwrap()
        })
    }

    fn lease_column<T: rusqlite::types::FromSql>(&mut self, lease: &LeaseId, col: &str) -> T {
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

    fn queued_requests(&mut self) -> Vec<String> {
        self.with_tx(|tx| {
            let mut stmt = tx
                .conn()
                .prepare("SELECT request_id FROM requests WHERE state = 'queued' ORDER BY row_id")
                .unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<Result<Vec<String>, _>>()
                .unwrap()
        })
    }

    fn notices(&mut self, h: &HolderRef) -> Vec<Notice> {
        self.with_tx(|tx| notices::pending(tx, h).unwrap())
    }

    fn notices_of(&mut self, h: &HolderRef, kind: NoticeKind) -> Vec<Notice> {
        self.notices(h)
            .into_iter()
            .filter(|n| n.kind == kind)
            .collect()
    }

    fn count(&mut self, h: &HolderRef, kind: NoticeKind) -> usize {
        self.notices_of(h, kind).len()
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

    fn unit_exists(&mut self, resource: &str, unit: &str) -> bool {
        let who = caller("admin");
        self.store
            .tx(|tx| self.inv.get_resource(tx, &who, &rid(resource)))
            .unwrap()
            .units
            .iter()
            .any(|u| u.id.as_str() == unit)
    }

    fn mark_dirty(&mut self, resource: &str, unit: &str, prev: &HolderRef) {
        let row = self.unit(resource, unit).row;
        let prev = serde_json::to_string(prev).unwrap();
        self.with_tx(|tx| {
            tx.conn()
                .execute(
                    "UPDATE units SET dirty_reason = 'expired', dirty_at = 5, \
                     dirty_prev_holder = ?2 WHERE row_id = ?1",
                    rusqlite::params![row, prev],
                )
                .unwrap();
        });
    }

    fn retire(&mut self, resource: &str, unit: &str) {
        let who = caller("admin");
        self.store
            .tx(|tx| self.inv.retire_unit(tx, &who, &rid(resource), &uid(unit)))
            .unwrap();
    }

    fn link(&mut self, a: (&str, &str), b: (&str, &str)) {
        let a = self.unit(a.0, a.1).row;
        let b = self.unit(b.0, b.1).row;
        self.store.tx(|tx| Inventory::link(tx, a, b)).unwrap();
    }

    /// The offer notice sent to `h`, which must exist.
    fn offered_lease(&mut self, h: &HolderRef) -> LeaseId {
        let n = self.notices_of(h, NoticeKind::Offered);
        n.last()
            .and_then(|n| n.lease_id.clone())
            .expect("an Offered notice with a lease id")
    }
}

const FLASH_USAGE: &str =
    "flash: idf.py -p {unit.path} flash; env ${PORT}; {resource.description} / {unit.description}";

impl Fixture {
    /// A user-scope device resource `rig` with unit `u1` (`/dev/ttyU1`), both
    /// described, and a usage template over the unit's path.
    fn seed_rig(&mut self) {
        let who = caller("admin");
        self.store
            .tx(|tx| -> Result<(), leclerk::inventory::InventoryError> {
                self.inv.create_resource(
                    tx,
                    &who,
                    &NewResource {
                        id: rid("rig"),
                        ty: ResourceType::Device,
                        scope: Scope::User,
                        description: "ESP32 rig".into(),
                        usage: FLASH_USAGE.into(),
                        health: None,
                        tags: Vec::new(),
                    },
                )?;
                self.inv.add_unit(
                    tx,
                    &who,
                    &rid("rig"),
                    &NewUnit {
                        id: uid("u1"),
                        settings: UnitSettings::Device {
                            path: PathBuf::from("/dev/ttyU1"),
                        },
                        description: "left board".into(),
                    },
                )?;
                Ok(())
            })
            .unwrap();
    }
}

impl Fixture {
    /// Replace the tags of resource `id` (as an admin).
    fn set_tags(&mut self, id: &str, tags: &[&str]) {
        self.set_tags_as(&caller("admin"), id, tags);
    }

    /// Replace the tags of resource `id` as `who`.
    fn set_tags_as(&mut self, who: &Caller, id: &str, tags: &[&str]) {
        self.store
            .tx(|tx| {
                self.inv.update_resource(
                    tx,
                    who,
                    &rid(id),
                    ResourcePatch {
                        tags: Some(tags.iter().map(|t| (*t).to_owned()).collect()),
                        ..ResourcePatch::default()
                    },
                )
            })
            .unwrap();
    }
}

fn tag_names(tags: &TagSet) -> Vec<&str> {
    tags.iter().map(Tag::as_str).collect()
}

const RENDERED: &str = "flash: idf.py -p /dev/ttyU1 flash; env ${PORT}; ESP32 rig / left board";

mod scheduler {
    use super::*;

    mod acquire {
        use super::*;

        mod on_immediate_grant {
            use super::*;

            #[test]
            fn grant_carries_resource_description_unit_description_and_rendered_usage() {
                let mut f = fixture();
                f.seed_rig();
                let g = f.grant(&caller("a"), req(on("rig"), MIN, Priority::Normal, "x"));
                assert_eq!(g.resource_description, "ESP32 rig");
                assert_eq!(g.unit_description, "left board");
                assert_eq!(g.usage, RENDERED);
            }
        }

        mod on_immediate_grant_with_tags {
            use super::*;

            #[test]
            fn grant_carries_resource_tags_sorted() {
                let mut f = fixture();
                f.seed_rig();
                f.set_tags("rig", &["zigbee", "esp32"]);
                let g = f.grant(&caller("a"), req(on("rig"), MIN, Priority::Normal, "x"));
                assert_eq!(tag_names(&g.resource_tags), ["esp32", "zigbee"]);
            }
        }

        mod on_free_healthy_unit_in_target_resource {
            use super::*;

            #[test]
            fn returns_granted_with_unit_settings_and_ends_at() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(
                    &caller("a"),
                    req(on("board"), 10 * MIN, Priority::Normal, "x"),
                );
                assert_eq!(g.resource, rid("board"));
                assert_eq!(g.unit, uid("u1"));
                assert_eq!(g.settings, generic_settings("board-u1"));
                assert_eq!(g.ends_at, T0 + 600_000);
            }

            #[test]
            fn lease_state_is_held() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req(on("board"), MIN, Priority::Normal, "x"));
                assert_eq!(f.lease_state(&g.lease_id), "held");
            }

            #[test]
            fn writes_granted_audit_entry() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                f.grant(&caller("a"), req(on("board"), MIN, Priority::Normal, "x"));
                let entries = f.store.tx(audit::all).unwrap();
                let granted = entries.iter().find(|e| e.action == "granted").unwrap();
                assert_eq!(granted.actor, holder("a"));
            }
        }

        mod on_specific_unit_target {
            use super::*;

            #[test]
            fn grants_that_unit() {
                let mut f = fixture();
                f.seed("board", &["u1", "u2"]);
                let g = f.grant(
                    &caller("a"),
                    req(on_unit("board", "u2"), MIN, Priority::Normal, "x"),
                );
                assert_eq!(g.unit, uid("u2"));
            }

            #[test]
            fn queues_when_that_unit_is_held_although_others_are_free() {
                let mut f = fixture();
                f.seed("board", &["u1", "u2"]);
                f.grant(
                    &caller("a"),
                    req(on_unit("board", "u2"), MIN, Priority::Normal, "x"),
                );
                let (_, position) = f.queue(
                    &caller("b"),
                    req(on_unit("board", "u2"), MIN, Priority::Normal, "y"),
                );
                assert_eq!(position, 1);
            }

            #[test]
            fn queues_when_a_linked_member_of_its_group_is_held() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                f.seed("alias", &["v1"]);
                f.link(("board", "u1"), ("alias", "v1"));
                f.grant(&caller("a"), req(on("alias"), MIN, Priority::Normal, "x"));
                let out = f
                    .acquire(
                        &caller("b"),
                        req(on_unit("board", "u1"), MIN, Priority::Normal, "y"),
                    )
                    .unwrap();
                assert!(matches!(out, AcquireOutcome::Queued { .. }));
            }

            #[test]
            fn on_unknown_unit_returns_not_found() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let out = f.acquire(
                    &caller("a"),
                    req(on_unit("board", "nope"), MIN, Priority::Normal, "x"),
                );
                assert!(matches!(out, Err(SchedulerError::NotFound)));
            }
        }

        mod on_unknown_or_invisible_resource {
            use super::*;

            #[test]
            fn returns_not_found_for_unknown_id() {
                let mut f = fixture();
                let out = f.acquire(&caller("a"), req(on("ghost"), MIN, Priority::Normal, "x"));
                assert!(matches!(out, Err(SchedulerError::NotFound)));
            }

            #[test]
            fn returns_not_found_for_another_projects_resource() {
                let mut f = fixture();
                let b = caller_in("b", Some("/p/b"));
                f.seed_keyed(&b, "board", &Scope::Project(project("/p/b")), &["u1"], "k");
                let a = caller_in("a", Some("/p/a"));
                let out = f.acquire(&a, req(on("board"), MIN, Priority::Normal, "x"));
                assert!(matches!(out, Err(SchedulerError::NotFound)));
            }
        }

        mod on_several_free_units {
            use super::*;

            #[test]
            fn grants_one_and_a_second_acquire_gets_a_different_unit() {
                let mut f = fixture();
                f.seed("board", &["u1", "u2"]);
                let g1 = f.grant(&caller("a"), req(on("board"), MIN, Priority::Normal, "x"));
                let g2 = f.grant(&caller("b"), req(on("board"), MIN, Priority::Normal, "y"));
                assert_ne!(g1.unit, g2.unit);
            }
        }

        mod on_free_unit_failing_health_check {
            use super::*;

            #[test]
            fn skips_it_and_grants_another_healthy_unit() {
                let mut f = fixture();
                f.seed("board", &["u1", "u2"]);
                f.probe.fail("u1");
                let g = f.grant(&caller("a"), req(on("board"), MIN, Priority::Normal, "x"));
                assert_eq!(g.unit, uid("u2"));
            }

            #[test]
            fn records_last_health_on_the_checked_unit() {
                let mut f = fixture();
                f.seed("board", &["u1", "u2"]);
                f.probe.fail("u1");
                f.grant(&caller("a"), req(on("board"), MIN, Priority::Normal, "x"));
                let u1 = f.unit("board", "u1");
                assert_eq!(u1.last_health.as_deref(), Some("unhealthy: stub says no"));
                assert_eq!(u1.last_health_at, Some(T0));
                assert_eq!(
                    f.unit("board", "u2").last_health.as_deref(),
                    Some("healthy")
                );
            }
        }

        mod on_no_healthy_free_unit {
            use super::*;

            #[test]
            fn returns_queued_with_request_id_position_and_estimated_wait() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                f.grant(
                    &caller("a"),
                    req(on("board"), 10 * MIN, Priority::Normal, "x"),
                );
                f.clock.advance(4 * MIN);
                let out = f
                    .acquire(&caller("b"), req(on("board"), MIN, Priority::Normal, "y"))
                    .unwrap();
                let AcquireOutcome::Queued {
                    request_id,
                    position,
                    estimated_wait,
                    ..
                } = out
                else {
                    panic!("expected queued, got {out:?}");
                };
                assert_eq!(f.queued_requests(), vec![request_id.to_string()]);
                assert_eq!(position, 1);
                assert_eq!(estimated_wait, Some(6 * MIN));
            }

            #[test]
            fn later_waiters_get_later_positions_and_longer_estimates() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                f.grant(
                    &caller("a"),
                    req(on("board"), 10 * MIN, Priority::Normal, "x"),
                );
                f.queue(
                    &caller("b"),
                    req(on("board"), 3 * MIN, Priority::Normal, "y"),
                );
                let out = f
                    .acquire(&caller("c"), req(on("board"), MIN, Priority::Normal, "z"))
                    .unwrap();
                let AcquireOutcome::Queued {
                    position,
                    estimated_wait,
                    ..
                } = out
                else {
                    panic!("expected queued, got {out:?}");
                };
                assert_eq!(position, 2);
                assert_eq!(estimated_wait, Some(13 * MIN));
            }

            #[test]
            fn queues_when_every_free_unit_is_unhealthy() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                f.probe.fail("u1");
                let out = f
                    .acquire(&caller("a"), req(on("board"), MIN, Priority::Normal, "x"))
                    .unwrap();
                assert!(matches!(
                    out,
                    AcquireOutcome::Queued {
                        position: 1,
                        estimated_wait: None,
                        ..
                    }
                ));
            }

            #[test]
            fn estimate_is_none_when_a_free_unit_is_blocked_only_by_health() {
                let mut f = fixture();
                f.seed("board", &["u1", "u2"]);
                f.grant(
                    &caller("a"),
                    req(on_unit("board", "u1"), 10 * MIN, Priority::Normal, "x"),
                );
                f.probe.fail("u2");
                let out = f
                    .acquire(&caller("b"), req(on("board"), MIN, Priority::Normal, "y"))
                    .unwrap();
                assert!(matches!(
                    out,
                    AcquireOutcome::Queued {
                        estimated_wait: None,
                        ..
                    }
                ));
            }

            #[test]
            fn estimate_is_none_when_a_target_lease_is_paused() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                // a yields to c and is paused while c borrows the unit.
                let g = f.grant(
                    &caller("a"),
                    AcquireRequest {
                        interruptible: true,
                        ..req(on("board"), 10 * MIN, Priority::Normal, "x")
                    },
                );
                f.acquire(&caller("c"), req(on("board"), MIN, Priority::High, "z"))
                    .unwrap();
                f.store
                    .tx(|tx| {
                        f.sched
                            .confirm(tx, &caller("a"), &g.lease_id, Confirm::Yielded)
                    })
                    .unwrap();
                let out = f
                    .acquire(&caller("b"), req(on("board"), MIN, Priority::Normal, "y"))
                    .unwrap();
                assert!(matches!(
                    out,
                    AcquireOutcome::Queued {
                        estimated_wait: None,
                        ..
                    }
                ));
            }

            #[test]
            fn estimate_ignores_leases_on_retiring_units() {
                let mut f = fixture();
                f.seed("board", &["u1", "u2"]);
                f.grant(
                    &caller("a"),
                    req(on_unit("board", "u1"), 60 * MIN, Priority::Normal, "x"),
                );
                f.grant(
                    &caller("b"),
                    req(on_unit("board", "u2"), 10 * MIN, Priority::Normal, "y"),
                );
                f.retire("board", "u1");
                let out = f
                    .acquire(&caller("c"), req(on("board"), MIN, Priority::Normal, "z"))
                    .unwrap();
                assert!(matches!(
                    out,
                    AcquireOutcome::Queued {
                        estimated_wait: Some(w),
                        ..
                    } if w == 10 * MIN
                ));
            }
        }

        mod on_unit_dirty {
            use super::*;

            #[test]
            fn grant_carries_dirty_reason_time_and_previous_holder() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                f.mark_dirty("board", "u1", &holder("old"));
                let g = f.grant(&caller("a"), req(on("board"), MIN, Priority::Normal, "x"));
                assert_eq!(
                    g.dirty,
                    Some(DirtyInfo {
                        reason: DirtyReason::Expired,
                        at: 5,
                        previous_holder: holder("old"),
                    })
                );
            }

            #[test]
            fn clears_the_dirty_flag() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                f.mark_dirty("board", "u1", &holder("old"));
                f.grant(&caller("a"), req(on("board"), MIN, Priority::Normal, "x"));
                assert_eq!(f.unit("board", "u1").dirty, None);
            }

            #[test]
            fn grant_on_a_clean_unit_carries_no_dirty_info() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req(on("board"), MIN, Priority::Normal, "x"));
                assert_eq!(g.dirty, None);
            }
        }

        mod on_linked_unit_held_through_another_resource {
            use super::*;

            #[test]
            fn treats_the_group_as_held() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                f.seed("alias", &["v1"]);
                f.link(("board", "u1"), ("alias", "v1"));
                f.grant(&caller("a"), req(on("alias"), MIN, Priority::Normal, "x"));
                let out = f
                    .acquire(&caller("b"), req(on("board"), MIN, Priority::Normal, "y"))
                    .unwrap();
                assert!(matches!(out, AcquireOutcome::Queued { .. }));
            }

            #[test]
            fn treats_a_cross_project_auto_link_as_held() {
                let mut f = fixture();
                let a = caller_in("a", Some("/p/a"));
                let b = caller_in("b", Some("/p/b"));
                f.seed_keyed(
                    &a,
                    "board",
                    &Scope::Project(project("/p/a")),
                    &["u1"],
                    "same",
                );
                f.seed_keyed(
                    &b,
                    "mine",
                    &Scope::Project(project("/p/b")),
                    &["u1"],
                    "same",
                );
                f.grant(&b, req(on("mine"), MIN, Priority::Normal, "secret"));
                let out = f
                    .acquire(&a, req(on("board"), MIN, Priority::Normal, "y"))
                    .unwrap();
                assert!(matches!(out, AcquireOutcome::Queued { .. }));
            }
        }

        mod on_retiring_unit {
            use super::*;

            #[test]
            fn never_grants_it() {
                let mut f = fixture();
                f.seed("board", &["u1", "u2"]);
                // u1 is pinned by a held lease, so retiring only flags it.
                let g = f.grant(
                    &caller("a"),
                    req(on_unit("board", "u1"), MIN, Priority::Normal, "x"),
                );
                f.retire("board", "u1");
                f.with_tx(|tx| {
                    tx.conn()
                        .execute(
                            "UPDATE leases SET state = 'ended' WHERE lease_id = ?1",
                            [g.lease_id.as_str()],
                        )
                        .unwrap();
                });
                // u1 is now free but retiring: the grant must pick u2.
                let g2 = f.grant(&caller("b"), req(on("board"), MIN, Priority::Normal, "y"));
                assert_eq!(g2.unit, uid("u2"));
                let g3 = f.acquire(&caller("c"), req(on("board"), MIN, Priority::Normal, "z"));
                assert!(matches!(g3, Ok(AcquireOutcome::Queued { .. })));
            }
        }

        mod on_target_unit_retiring {
            use super::*;

            #[test]
            fn returns_retiring_instead_of_queueing() {
                let mut f = fixture();
                f.seed("board", &["u1", "u2"]);
                f.grant(
                    &caller("a"),
                    req(on_unit("board", "u1"), MIN, Priority::Normal, "x"),
                );
                f.retire("board", "u1");
                let out = f.acquire(
                    &caller("b"),
                    req(on_unit("board", "u1"), MIN, Priority::Normal, "y"),
                );
                assert!(matches!(out, Err(SchedulerError::Retiring)));
            }

            #[test]
            fn returns_retiring_for_a_resource_whose_units_all_retire() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                f.grant(&caller("a"), req(on("board"), MIN, Priority::Normal, "x"));
                f.retire("board", "u1");
                let out = f.acquire(&caller("b"), req(on("board"), MIN, Priority::Normal, "y"));
                assert!(matches!(out, Err(SchedulerError::Retiring)));
            }
        }

        mod on_caller_already_holding_this_exact_unit {
            use super::*;

            #[test]
            fn returns_already_held() {
                let mut f = fixture();
                f.seed("board", &["u1", "u2"]);
                let a = caller("a");
                f.grant(&a, req(on_unit("board", "u1"), MIN, Priority::Normal, "x"));
                let out = f.acquire(&a, req(on_unit("board", "u1"), MIN, Priority::Normal, "x"));
                assert!(matches!(out, Err(SchedulerError::AlreadyHeld)));
            }
        }

        mod on_override_reason {
            use super::*;

            #[test]
            fn is_stored_on_the_lease() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let mut r = req(on("board"), MIN, Priority::Normal, "x");
                r.override_reason = Some("user said so".into());
                let g = f.grant(&caller("a"), r);
                let stored: Option<String> = f.lease_column(&g.lease_id, "override_reason");
                assert_eq!(stored.as_deref(), Some("user said so"));
            }

            #[test]
            fn is_stored_on_the_queued_request() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                f.grant(&caller("a"), req(on("board"), MIN, Priority::Normal, "x"));
                let mut r = req(on("board"), MIN, Priority::Normal, "y");
                r.override_reason = Some("user said so".into());
                let (id, _) = f.queue(&caller("b"), r);
                let stored: Option<String> = f.with_tx(|tx| {
                    tx.conn()
                        .query_row(
                            "SELECT override_reason FROM requests WHERE request_id = ?1",
                            [id.as_str()],
                            |r| r.get(0),
                        )
                        .unwrap()
                });
                assert_eq!(stored.as_deref(), Some("user said so"));
            }
        }
        mod on_a_resource_with_no_units {
            use super::*;

            #[test]
            fn returns_no_units_and_queues_nothing() {
                let mut f = fixture();
                f.seed("empty", &[]);
                let e = f
                    .acquire(&caller("a"), req(on("empty"), MIN, Priority::Normal, "x"))
                    .unwrap_err();
                assert!(matches!(e, SchedulerError::NoUnits), "{e:?}");
                assert_eq!(leclerk::error::ErrorCode::code(&e), "no_units");
                assert_eq!(f.queued_requests(), Vec::<String>::new());
            }

            #[test]
            fn maps_to_a_conflict() {
                let api = leclerk::engine::ApiError::from(SchedulerError::NoUnits);
                assert_eq!(api.kind, leclerk::engine::ErrorKind::Conflict);
                assert_eq!(api.code, "no_units");
            }
        }
    }

    mod queue_ordering_and_dispatch {
        use super::*;
        mod on_the_last_usable_unit_removed_under_a_waiter {
            use super::*;

            fn stranded() -> Fixture {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                f.probe.fail("u1");
                f.queue(&caller("w"), req(on("board"), MIN, Priority::Normal, "w"));
                let who = caller("admin");
                f.store
                    .tx(|tx| f.inv.remove_unit(tx, &who, &rid("board"), &uid("u1")))
                    .unwrap();
                f.dispatch();
                f
            }

            #[test]
            fn drops_the_resource_level_request() {
                let mut f = stranded();
                assert_eq!(f.queued_requests(), Vec::<String>::new());
            }

            #[test]
            fn tells_the_waiter_with_queue_dropped() {
                let mut f = stranded();
                assert_eq!(f.count(&holder("w"), NoticeKind::QueueDropped), 1);
            }
        }

        mod on_unit_freed_with_waiters {
            use super::*;

            #[test]
            fn offers_to_highest_priority_then_earliest_request() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req(on("board"), MIN, Priority::Normal, "x"));
                f.queue(&caller("low"), req(on("board"), MIN, Priority::Low, "l"));
                f.clock.advance(Duration::from_secs(1));
                f.queue(&caller("early"), req(on("board"), MIN, Priority::High, "e"));
                f.clock.advance(Duration::from_secs(1));
                f.queue(&caller("late"), req(on("board"), MIN, Priority::High, "l"));
                f.end(&g.lease_id);
                f.dispatch();
                assert_eq!(f.notices_of(&holder("early"), NoticeKind::Offered).len(), 1);
                assert_eq!(f.count(&holder("late"), NoticeKind::Offered), 0);
                assert_eq!(f.count(&holder("low"), NoticeKind::Offered), 0);
            }

            #[test]
            fn orders_by_clock_time_not_by_id() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req(on("board"), MIN, Priority::Normal, "x"));
                f.queue(
                    &caller("first"),
                    req(on("board"), MIN, Priority::Normal, "1"),
                );
                f.queue(
                    &caller("second"),
                    req(on("board"), MIN, Priority::Normal, "2"),
                );
                // Make the second request older by the clock.
                f.with_tx(|tx| {
                    tx.conn()
                        .execute(
                            "UPDATE requests SET created_at = 0 WHERE holder LIKE '%second%'",
                            [],
                        )
                        .unwrap();
                });
                f.end(&g.lease_id);
                f.dispatch();
                assert_eq!(
                    f.notices_of(&holder("second"), NoticeKind::Offered).len(),
                    1
                );
            }

            #[test]
            fn waiters_of_all_resources_containing_a_group_member_compete() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                f.seed("alias", &["v1"]);
                f.link(("board", "u1"), ("alias", "v1"));
                let g = f.grant(&caller("a"), req(on("board"), MIN, Priority::Normal, "x"));
                f.queue(
                    &caller("via-board"),
                    req(on("board"), MIN, Priority::Normal, "b"),
                );
                f.queue(
                    &caller("via-alias"),
                    req(on("alias"), MIN, Priority::High, "h"),
                );
                f.end(&g.lease_id);
                f.dispatch();
                let lease = f.offered_lease(&holder("via-alias"));
                let unit_row: i64 = f.lease_column(&lease, "unit_row");
                assert_eq!(unit_row, f.unit("alias", "v1").row);
                assert_eq!(f.count(&holder("via-board"), NoticeKind::Offered), 0);
            }

            #[test]
            fn offers_one_unit_of_a_group_only_once() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                f.seed("alias", &["v1"]);
                f.link(("board", "u1"), ("alias", "v1"));
                let g = f.grant(&caller("a"), req(on("board"), MIN, Priority::Normal, "x"));
                f.queue(&caller("b"), req(on("board"), MIN, Priority::Normal, "b"));
                f.queue(&caller("c"), req(on("alias"), MIN, Priority::Normal, "c"));
                f.end(&g.lease_id);
                f.dispatch();
                f.dispatch();
                let offers = f.notices_of(&holder("b"), NoticeKind::Offered).len()
                    + f.notices_of(&holder("c"), NoticeKind::Offered).len();
                assert_eq!(offers, 1);
            }

            #[test]
            fn skips_an_unhealthy_unit_and_offers_a_healthy_one() {
                let mut f = fixture();
                f.seed("board", &["u1", "u2"]);
                let g1 = f.grant(&caller("a"), req(on("board"), MIN, Priority::Normal, "x"));
                let g2 = f.grant(&caller("b"), req(on("board"), MIN, Priority::Normal, "y"));
                f.queue(&caller("w"), req(on("board"), MIN, Priority::Normal, "w"));
                f.probe.fail(g1.unit.as_str());
                f.end(&g1.lease_id);
                f.dispatch();
                assert_eq!(f.count(&holder("w"), NoticeKind::Offered), 0);
                f.end(&g2.lease_id);
                f.dispatch();
                let lease = f.offered_lease(&holder("w"));
                let unit_row: i64 = f.lease_column(&lease, "unit_row");
                assert_eq!(unit_row, f.unit("board", g2.unit.as_str()).row);
            }
        }

        mod on_offer {
            use super::*;

            fn offered() -> (Fixture, LeaseId) {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req(on("board"), MIN, Priority::Normal, "x"));
                f.queue(
                    &caller("w"),
                    req(on("board"), 5 * MIN, Priority::Normal, "w"),
                );
                f.end(&g.lease_id);
                f.clock.advance(Duration::from_secs(7));
                f.dispatch();
                let lease = f.offered_lease(&holder("w"));
                (f, lease)
            }

            #[test]
            fn lease_state_is_offered() {
                let (mut f, lease) = offered();
                assert_eq!(f.lease_state(&lease), "offered");
            }

            #[test]
            fn publishes_offered_notice_with_claim_deadline() {
                let (mut f, lease) = offered();
                let n = f.notices_of(&holder("w"), NoticeKind::Offered);
                assert_eq!(n.len(), 1);
                assert_eq!(n[0].lease_id.as_ref(), Some(&lease));
                assert_eq!(n[0].deadline, Some(T0 + 7_000 + 300_000));
                assert_eq!(n[0].resource, Some(rid("board")));
                assert_eq!(n[0].unit, Some(uid("u1")));
                assert_ne!(n[0].action, "");
            }

            #[test]
            fn removes_the_request_from_the_queue() {
                let (mut f, _) = offered();
                assert_eq!(f.queued_requests(), Vec::<String>::new());
            }

            #[test]
            fn publishes_position_changed_to_waiters_behind() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req(on("board"), MIN, Priority::Normal, "x"));
                f.queue(&caller("w1"), req(on("board"), MIN, Priority::Normal, "1"));
                f.queue(&caller("w2"), req(on("board"), MIN, Priority::Normal, "2"));
                f.end(&g.lease_id);
                f.dispatch();
                let n = f.notices_of(&holder("w2"), NoticeKind::PositionChanged);
                assert_eq!(n.len(), 1);
                assert_eq!(n[0].position, Some(1));
            }

            #[test]
            fn hands_over_dirty_info_and_clears_the_unit() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req(on("board"), MIN, Priority::Normal, "x"));
                f.queue(&caller("w"), req(on("board"), MIN, Priority::Normal, "w"));
                f.end(&g.lease_id);
                f.mark_dirty("board", "u1", &holder("a"));
                f.dispatch();
                let n = f.notices_of(&holder("w"), NoticeKind::Offered);
                assert_eq!(
                    n[0].dirty.as_ref().map(|d| d.reason),
                    Some(DirtyReason::Expired)
                );
                assert_eq!(f.unit("board", "u1").dirty, None);
            }
        }

        mod on_claim_within_claim_window {
            use super::*;

            #[test]
            fn state_held_and_ends_at_is_claim_time_plus_duration() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req(on("board"), MIN, Priority::Normal, "x"));
                let w = caller("w");
                f.queue(&w, req(on("board"), 5 * MIN, Priority::Normal, "w"));
                f.end(&g.lease_id);
                f.dispatch();
                let lease = f.offered_lease(&w.holder);
                f.clock.advance(2 * MIN);
                let claimed = f.store.tx(|tx| f.sched.claim(tx, &w, &lease)).unwrap();
                assert_eq!(claimed.state, LeaseState::Held);
                assert_eq!(claimed.ends_at, Some(T0 + 120_000 + 300_000));
                assert_eq!(claimed.settings, generic_settings("board-u1"));
                assert_eq!(f.lease_state(&lease), "held");
            }

            #[test]
            fn by_another_holder_returns_not_holder() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req(on("board"), MIN, Priority::Normal, "x"));
                f.queue(&caller("w"), req(on("board"), MIN, Priority::Normal, "w"));
                f.end(&g.lease_id);
                f.dispatch();
                let lease = f.offered_lease(&holder("w"));
                let out = f.store.tx(|tx| f.sched.claim(tx, &caller("thief"), &lease));
                assert!(matches!(out, Err(SchedulerError::NotHolder)));
            }

            #[test]
            fn of_a_held_lease_returns_bad_state() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let a = caller("a");
                let g = f.grant(&a, req(on("board"), MIN, Priority::Normal, "x"));
                let out = f.store.tx(|tx| f.sched.claim(tx, &a, &g.lease_id));
                assert!(matches!(
                    out,
                    Err(SchedulerError::BadState {
                        state: LeaseState::Held
                    })
                ));
            }

            #[test]
            fn of_an_unknown_lease_returns_not_found() {
                let mut f = fixture();
                let out = f
                    .store
                    .tx(|tx| f.sched.claim(tx, &caller("a"), &LeaseId::new()));
                assert!(matches!(out, Err(SchedulerError::NotFound)));
            }
        }

        mod on_claim_window_passing {
            use super::*;

            /// `w1` is offered the unit and lets the claim window pass;
            /// `w2` waits behind.
            fn lapsed() -> (Fixture, LeaseId) {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req(on("board"), MIN, Priority::Normal, "x"));
                f.queue(&caller("w1"), req(on("board"), MIN, Priority::Normal, "1"));
                f.queue(&caller("w2"), req(on("board"), MIN, Priority::Normal, "2"));
                f.end(&g.lease_id);
                f.dispatch();
                let lease = f.offered_lease(&holder("w1"));
                f.clock.advance(Duration::from_secs(301));
                f.dispatch();
                (f, lease)
            }

            #[test]
            fn offer_is_withdrawn() {
                let (mut f, lease) = lapsed();
                assert_eq!(f.lease_state(&lease), "ended");
            }

            #[test]
            fn request_is_dropped() {
                let (mut f, _) = lapsed();
                // w2 was offered next, so nothing of w1's is left queued.
                assert_eq!(f.queued_requests(), Vec::<String>::new());
                let out = f
                    .store
                    .tx(|tx| f.sched.status(tx, &caller("admin"), View::All))
                    .unwrap();
                assert!(out.resources.iter().all(|r| r.queue.is_empty()));
            }

            #[test]
            fn publishes_queue_dropped_notice() {
                let (mut f, lease) = lapsed();
                let n = f.notices_of(&holder("w1"), NoticeKind::QueueDropped);
                assert_eq!(n.len(), 1);
                assert_eq!(n[0].lease_id.as_ref(), Some(&lease));
            }

            #[test]
            fn queue_dropped_notice_names_the_request() {
                let (mut f, _) = lapsed();
                let offered = f.notices_of(&holder("w1"), NoticeKind::Offered);
                let dropped = f.notices_of(&holder("w1"), NoticeKind::QueueDropped);
                assert!(offered[0].request_id.is_some());
                assert_eq!(dropped[0].request_id, offered[0].request_id);
            }

            #[test]
            fn offers_the_unit_to_the_next_waiter() {
                let (mut f, _) = lapsed();
                assert_eq!(f.notices_of(&holder("w2"), NoticeKind::Offered).len(), 1);
            }

            #[test]
            fn a_late_claim_returns_bad_state() {
                let (mut f, lease) = lapsed();
                let out = f.store.tx(|tx| f.sched.claim(tx, &caller("w1"), &lease));
                assert!(matches!(out, Err(SchedulerError::BadState { .. })));
            }

            #[test]
            fn a_claim_after_the_deadline_but_before_dispatch_is_refused() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req(on("board"), MIN, Priority::Normal, "x"));
                f.queue(&caller("w"), req(on("board"), MIN, Priority::Normal, "w"));
                f.end(&g.lease_id);
                f.dispatch();
                let lease = f.offered_lease(&holder("w"));
                f.clock.advance(Duration::from_secs(301));
                let out = f.store.tx(|tx| f.sched.claim(tx, &caller("w"), &lease));
                assert!(matches!(out, Err(SchedulerError::BadState { .. })));
            }

            #[test]
            fn restores_the_dirty_flag_the_offer_carried() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req(on("board"), MIN, Priority::Normal, "x"));
                f.queue(&caller("w"), req(on("board"), MIN, Priority::Normal, "w"));
                f.end(&g.lease_id);
                f.mark_dirty("board", "u1", &holder("a"));
                f.dispatch();
                f.clock.advance(Duration::from_secs(301));
                f.dispatch();
                assert_eq!(
                    f.unit("board", "u1").dirty.map(|d| d.previous_holder),
                    Some(holder("a"))
                );
            }
        }

        mod on_cancel_of_queued_request {
            use super::*;

            fn three_waiters() -> (Fixture, RequestId) {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                f.grant(&caller("a"), req(on("board"), MIN, Priority::Normal, "x"));
                let (first, _) =
                    f.queue(&caller("w1"), req(on("board"), MIN, Priority::Normal, "1"));
                f.queue(&caller("w2"), req(on("board"), MIN, Priority::Normal, "2"));
                f.queue(&caller("w3"), req(on("board"), MIN, Priority::Normal, "3"));
                f.store
                    .tx(|tx| f.sched.cancel(tx, &caller("w1"), &first))
                    .unwrap();
                (f, first)
            }

            #[test]
            fn removes_the_request() {
                let (mut f, first) = three_waiters();
                assert!(!f.queued_requests().contains(&first.to_string()));
            }

            #[test]
            fn later_positions_decrease() {
                let (mut f, _) = three_waiters();
                let status = f
                    .store
                    .tx(|tx| f.sched.status(tx, &caller("admin"), View::All))
                    .unwrap();
                let queue = &status.resources[0].queue;
                let positions: Vec<(HolderRef, u32)> = queue
                    .iter()
                    .map(|q| (q.holder.clone(), q.position))
                    .collect();
                assert_eq!(positions, vec![(holder("w2"), 1), (holder("w3"), 2)]);
            }

            #[test]
            fn publishes_position_changed_to_each_moved_waiter() {
                let (mut f, _) = three_waiters();
                let w2 = f.notices_of(&holder("w2"), NoticeKind::PositionChanged);
                let w3 = f.notices_of(&holder("w3"), NoticeKind::PositionChanged);
                assert_eq!(w2.iter().map(|n| n.position).collect::<Vec<_>>(), [Some(1)]);
                assert_eq!(w3.iter().map(|n| n.position).collect::<Vec<_>>(), [Some(2)]);
            }

            #[test]
            fn by_another_holder_returns_not_holder() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                f.grant(&caller("a"), req(on("board"), MIN, Priority::Normal, "x"));
                let (id, _) = f.queue(&caller("w1"), req(on("board"), MIN, Priority::Normal, "1"));
                let out = f.store.tx(|tx| f.sched.cancel(tx, &caller("x"), &id));
                assert!(matches!(out, Err(SchedulerError::NotHolder)));
            }

            #[test]
            fn of_an_unknown_request_returns_not_found() {
                let mut f = fixture();
                let out = f
                    .store
                    .tx(|tx| f.sched.cancel(tx, &caller("x"), &RequestId::new()));
                assert!(matches!(out, Err(SchedulerError::NotFound)));
            }
        }

        mod on_waiter_blocked_by_invisible_holder {
            use super::*;

            #[test]
            fn status_shows_held_by_an_agent_in_another_project_only() {
                let mut f = fixture();
                let a = caller_in("a", Some("/p/a"));
                let b = caller_in("b", Some("/p/b"));
                f.seed_keyed(
                    &a,
                    "board",
                    &Scope::Project(project("/p/a")),
                    &["u1"],
                    "same",
                );
                f.seed_keyed(
                    &b,
                    "mine",
                    &Scope::Project(project("/p/b")),
                    &["u1"],
                    "same",
                );
                f.grant(&b, req(on("mine"), MIN, Priority::Normal, "secret purpose"));
                f.queue(&a, req(on("board"), MIN, Priority::Normal, "y"));
                let status = f
                    .store
                    .tx(|tx| f.sched.status(tx, &a, View::Caller))
                    .unwrap();
                assert_eq!(status.resources.len(), 1);
                let unit = &status.resources[0].units[0];
                assert_eq!(unit.occupancy, Occupancy::HeldInAnotherProject);
                assert_eq!(
                    unit.occupancy.to_string(),
                    "held by an agent in another project"
                );
                let json = serde_json::to_string(&status).unwrap();
                assert!(!json.contains("secret purpose"));
                assert!(!json.contains("mine"));
                assert!(!json.contains("/p/b"));
            }

            #[test]
            fn status_shows_visible_holder_details() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(
                    &caller("a"),
                    req(on("board"), MIN, Priority::Normal, "flash"),
                );
                let status = f
                    .store
                    .tx(|tx| f.sched.status(tx, &caller("b"), View::Caller))
                    .unwrap();
                let Occupancy::Held(h) = &status.resources[0].units[0].occupancy else {
                    panic!("expected visible holder");
                };
                assert_eq!(h.holder, holder("a"));
                assert_eq!(h.lease_id, g.lease_id);
                assert_eq!(h.purpose, "flash");
            }

            #[test]
            fn admin_view_shows_the_other_projects_holder() {
                let mut f = fixture();
                let a = caller_in("a", Some("/p/a"));
                let b = caller_in("b", Some("/p/b"));
                f.seed_keyed(
                    &a,
                    "board",
                    &Scope::Project(project("/p/a")),
                    &["u1"],
                    "same",
                );
                f.seed_keyed(
                    &b,
                    "mine",
                    &Scope::Project(project("/p/b")),
                    &["u1"],
                    "same",
                );
                f.grant(&b, req(on("mine"), MIN, Priority::Normal, "secret purpose"));
                let status = f.store.tx(|tx| f.sched.status(tx, &a, View::All)).unwrap();
                let board = status
                    .resources
                    .iter()
                    .find(|r| r.id == rid("board"))
                    .unwrap();
                assert!(matches!(board.units[0].occupancy, Occupancy::Held(_)));
            }
        }

        mod on_retiring_unit_freed {
            use super::*;

            /// u1 is held, retired, has a waiter pinned to it and one on the
            /// resource; then its lease ends.
            fn freed() -> Fixture {
                let mut f = fixture();
                f.seed("board", &["u1", "u2"]);
                let g = f.grant(
                    &caller("a"),
                    req(on_unit("board", "u1"), MIN, Priority::Normal, "x"),
                );
                f.grant(
                    &caller("b"),
                    req(on_unit("board", "u2"), MIN, Priority::Normal, "y"),
                );
                f.queue(
                    &caller("pinned"),
                    req(on_unit("board", "u1"), MIN, Priority::High, "p"),
                );
                f.queue(&caller("any"), req(on("board"), MIN, Priority::Normal, "q"));
                f.retire("board", "u1");
                f.end(&g.lease_id);
                f.dispatch();
                f
            }

            #[test]
            fn removes_the_unit() {
                let mut f = freed();
                assert!(!f.unit_exists("board", "u1"));
            }

            #[test]
            fn drops_requests_pinned_to_it_with_queue_dropped() {
                let mut f = freed();
                assert_eq!(
                    f.notices_of(&holder("pinned"), NoticeKind::QueueDropped)
                        .len(),
                    1
                );
                assert_eq!(f.queued_requests().len(), 1);
            }

            #[test]
            fn never_offers_it_to_resource_waiters() {
                let mut f = freed();
                assert_eq!(f.count(&holder("any"), NoticeKind::Offered), 0);
            }

            #[test]
            fn drops_resource_requests_stranded_without_units() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req(on("board"), MIN, Priority::Normal, "x"));
                f.queue(&caller("w"), req(on("board"), MIN, Priority::Normal, "w"));
                f.retire("board", "u1");
                f.end(&g.lease_id);
                f.dispatch();
                assert_eq!(f.count(&holder("w"), NoticeKind::QueueDropped), 1);
                assert_eq!(f.queued_requests(), Vec::<String>::new());
            }

            #[test]
            fn keeps_a_retiring_unit_with_a_live_lease() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                f.grant(&caller("a"), req(on("board"), MIN, Priority::Normal, "x"));
                f.retire("board", "u1");
                f.dispatch();
                assert!(f.unit_exists("board", "u1"));
            }
        }

        mod dispatch_idempotence {
            use super::*;

            #[test]
            fn a_second_run_changes_nothing() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                let g = f.grant(&caller("a"), req(on("board"), MIN, Priority::Normal, "x"));
                f.queue(&caller("w1"), req(on("board"), MIN, Priority::Normal, "1"));
                f.queue(&caller("w2"), req(on("board"), MIN, Priority::Normal, "2"));
                f.end(&g.lease_id);
                f.dispatch();
                let before = (f.notices(&holder("w1")), f.notices(&holder("w2")));
                f.dispatch();
                let after = (f.notices(&holder("w1")), f.notices(&holder("w2")));
                assert_eq!(before, after);
            }

            #[test]
            fn does_not_recheck_health_of_a_held_group() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                f.grant(&caller("a"), req(on("board"), MIN, Priority::Normal, "x"));
                f.queue(&caller("w"), req(on("board"), MIN, Priority::Normal, "w"));
                let calls = f.probe.calls().len();
                f.dispatch();
                assert_eq!(f.probe.calls().len(), calls);
            }
        }

        mod on_unit_healing_while_waiters_queue {
            use super::*;

            #[test]
            fn acquire_does_not_jump_an_existing_waiter() {
                let mut f = fixture();
                f.seed("board", &["u1"]);
                f.probe.fail("u1");
                f.queue(&caller("w"), req(on("board"), MIN, Priority::Normal, "w"));
                f.probe.heal("u1");
                // An unhealthy result stands until the backoff passes.
                f.clock.advance(leclerk::scheduler::HEALTH_RECHECK_BACKOFF);
                let out = f
                    .acquire(
                        &caller("late"),
                        req(on("board"), MIN, Priority::Normal, "l"),
                    )
                    .unwrap();
                assert!(matches!(out, AcquireOutcome::Queued { .. }));
                assert_eq!(f.notices_of(&holder("w"), NoticeKind::Offered).len(), 1);
            }
        }
    }
}

mod status {
    use super::*;

    fn status_of(f: &mut Fixture, who: &Caller, view: View) -> leclerk::scheduler::StatusView {
        f.store.tx(|tx| f.sched.status(tx, who, view)).unwrap()
    }

    mod on_a_described_and_tagged_resource {
        use super::*;

        #[test]
        fn resource_status_carries_description_and_sorted_tags() {
            let mut f = fixture();
            f.seed_rig();
            f.set_tags("rig", &["zigbee", "esp32"]);
            let status = status_of(&mut f, &caller("a"), View::Caller);
            assert_eq!(status.resources[0].description, "ESP32 rig");
            assert_eq!(tag_names(&status.resources[0].tags), ["esp32", "zigbee"]);
        }

        #[test]
        fn unit_status_carries_description() {
            let mut f = fixture();
            f.seed_rig();
            let status = status_of(&mut f, &caller("a"), View::Caller);
            assert_eq!(status.resources[0].units[0].description, "left board");
        }
    }

    mod on_a_holder_in_another_project {
        use super::*;

        #[test]
        fn stays_held_by_an_agent_in_another_project_without_leaking_its_resource() {
            let mut f = fixture();
            let a = caller_in("a", Some("/p/a"));
            let b = caller_in("b", Some("/p/b"));
            f.seed_keyed(
                &a,
                "board",
                &Scope::Project(project("/p/a")),
                &["u1"],
                "same",
            );
            f.seed_keyed(
                &b,
                "hidden",
                &Scope::Project(project("/p/b")),
                &["u1"],
                "same",
            );
            f.set_tags_as(&b, "hidden", &["secret-tag"]);
            f.grant(&b, req(on("hidden"), MIN, Priority::Normal, "x"));
            let status = status_of(&mut f, &a, View::Caller);
            assert_eq!(status.resources.len(), 1);
            assert_eq!(
                status.resources[0].units[0].occupancy,
                Occupancy::HeldInAnotherProject
            );
            let json = serde_json::to_string(&status).unwrap();
            assert!(!json.contains("secret-tag"));
            assert!(!json.contains("hidden"));
        }
    }

    mod on_a_project_resource_shadowing_a_user_one {
        use super::*;

        #[test]
        fn marks_the_project_resource_only() {
            let mut f = fixture();
            f.seed("board", &["u1"]);
            let p = caller_in("p", Some("/p"));
            f.seed_keyed(&p, "board", &Scope::Project(project("/p")), &["u1"], "pb");
            let status = f
                .store
                .tx(|tx| f.sched.status(tx, &caller("admin"), View::All))
                .unwrap();
            let marks: Vec<(Scope, bool)> = status
                .resources
                .into_iter()
                .map(|r| (r.scope, r.shadows_user))
                .collect();
            assert!(marks.contains(&(Scope::User, false)), "{marks:?}");
            assert!(
                marks.contains(&(Scope::Project(project("/p")), true)),
                "{marks:?}"
            );
        }
    }
}

mod health_backoff {
    use super::*;
    use leclerk::scheduler::HEALTH_RECHECK_BACKOFF;

    /// `board/u1` failing its check, with `w` waiting for it; the acquire
    /// probed it once.
    fn failing_with_waiter() -> Fixture {
        let mut f = fixture();
        f.seed("board", &["u1"]);
        f.probe.fail("u1");
        f.queue(&caller("w"), req(on("board"), MIN, Priority::Normal, "w"));
        assert_eq!(f.probe.calls().len(), 1);
        f
    }

    /// One engine maintenance pass: tick, dispatch, tick.
    fn maintain(f: &mut Fixture) {
        f.store
            .tx(|tx| {
                f.sched.tick(tx)?;
                f.sched.dispatch(tx)?;
                f.sched.tick(tx)
            })
            .unwrap();
    }

    mod on_an_unhealthy_unit_with_a_waiter {
        use super::*;

        #[test]
        fn is_probed_at_most_once_per_backoff_across_dispatches() {
            let mut f = failing_with_waiter();
            for _ in 0..5 {
                f.clock.advance(Duration::from_secs(5));
                f.dispatch();
                maintain(&mut f);
            }
            assert!(Duration::from_secs(25) < HEALTH_RECHECK_BACKOFF);
            assert_eq!(f.probe.calls().len(), 1);
        }

        #[test]
        fn is_probed_again_once_the_backoff_passed() {
            let mut f = failing_with_waiter();
            f.clock.advance(HEALTH_RECHECK_BACKOFF);
            f.dispatch();
            assert_eq!(f.probe.calls().len(), 2);
        }

        #[test]
        fn is_offered_once_it_heals_and_the_backoff_passed() {
            let mut f = failing_with_waiter();
            f.probe.heal("u1");
            f.dispatch();
            assert_eq!(f.count(&holder("w"), NoticeKind::Offered), 0);
            f.clock.advance(HEALTH_RECHECK_BACKOFF);
            f.dispatch();
            assert_eq!(f.count(&holder("w"), NoticeKind::Offered), 1);
        }
    }

    mod on_a_unit_update {
        use super::*;

        #[test]
        fn probes_again_at_once() {
            let mut f = failing_with_waiter();
            let who = caller("admin");
            f.store
                .tx(|tx| {
                    f.inv.update_unit(
                        tx,
                        &who,
                        &rid("board"),
                        &uid("u1"),
                        UnitPatch {
                            settings: Some(generic_settings("board-u1-new")),
                            description: None,
                        },
                    )
                })
                .unwrap();
            f.dispatch();
            assert_eq!(f.probe.calls().len(), 2);
        }
    }

    mod on_a_resource_health_change {
        use super::*;

        #[test]
        fn probes_again_at_once() {
            let mut f = failing_with_waiter();
            let who = caller("admin");
            f.store
                .tx(|tx| {
                    f.inv.update_resource(
                        tx,
                        &who,
                        &rid("board"),
                        ResourcePatch {
                            health: Some(Some(HealthSpec {
                                checks: vec![leclerk::model::Check::Custom {
                                    command: "true".into(),
                                }],
                            })),
                            ..ResourcePatch::default()
                        },
                    )
                })
                .unwrap();
            f.dispatch();
            assert_eq!(f.probe.calls().len(), 2);
        }
    }

    mod on_forget_health {
        use super::*;

        #[test]
        fn probes_again_at_once() {
            let mut f = failing_with_waiter();
            let row = f.unit("board", "u1").row;
            f.sched.forget_health(&[row]);
            f.dispatch();
            assert_eq!(f.probe.calls().len(), 2);
        }
    }

    mod on_a_healthy_unit {
        use super::*;

        #[test]
        fn is_probed_at_every_grant() {
            let mut f = fixture();
            f.seed("board", &["u1"]);
            let g = f.grant(&caller("a"), req(on("board"), MIN, Priority::Normal, "x"));
            f.end(&g.lease_id);
            f.grant(&caller("b"), req(on("board"), MIN, Priority::Normal, "y"));
            assert_eq!(f.probe.calls().len(), 2);
        }
    }
}

mod notify_helper {
    use super::*;

    fn published(recipient: Option<&str>, scope: &Scope) -> Option<String> {
        let mut f = fixture();
        let to = holder("to");
        let recipient = recipient.map(project);
        f.with_tx(|tx| {
            let notice = Notice::new(to.clone(), NoticeKind::YieldRequested, "act".into(), T0);
            notify(
                tx,
                notice,
                recipient.as_ref(),
                Some(OtherParty {
                    scope,
                    purpose: "secret purpose",
                }),
            )
            .unwrap();
        });
        f.notices(&to).remove(0).other_purpose
    }

    #[test]
    fn omits_other_purpose_for_a_party_in_an_invisible_project() {
        let scope = Scope::Project(project("/p/b"));
        assert_eq!(published(Some("/p/a"), &scope), None);
        assert_eq!(published(None, &scope), None);
    }

    #[test]
    fn includes_other_purpose_for_a_visible_party() {
        let own = Scope::Project(project("/p/a"));
        assert_eq!(
            published(Some("/p/a"), &own).as_deref(),
            Some("secret purpose")
        );
        assert_eq!(
            published(Some("/p/a"), &Scope::User).as_deref(),
            Some("secret purpose")
        );
    }
}

mod scheduler_leases {
    use super::*;

    fn held(f: &mut Fixture, who: &Caller) -> LeaseId {
        f.seed_rig();
        f.grant(who, req(on("rig"), MIN, Priority::Normal, "x"))
            .lease_id
    }

    mod claim {
        use super::*;

        mod on_any_lease {
            use super::*;

            #[test]
            fn lease_carries_resource_description_unit_description_and_rendered_usage() {
                let mut f = fixture();
                f.seed_rig();
                let a = caller("a");
                let b = caller("b");
                let first = f.grant(&a, req(on("rig"), MIN, Priority::Normal, "x"));
                f.queue(&b, req(on("rig"), MIN, Priority::Normal, "y"));
                f.end(&first.lease_id);
                f.dispatch();
                let offer = f.offered_lease(&b.holder);
                let lease = f.store.tx(|tx| f.sched.claim(tx, &b, &offer)).unwrap();
                assert_eq!(lease.resource_description, "ESP32 rig");
                assert_eq!(lease.unit_description, "left board");
                assert_eq!(lease.usage, RENDERED);
            }

            #[test]
            fn lease_carries_resource_tags() {
                let mut f = fixture();
                f.seed_rig();
                f.set_tags("rig", &["esp32"]);
                let a = caller("a");
                let b = caller("b");
                let first = f.grant(&a, req(on("rig"), MIN, Priority::Normal, "x"));
                f.queue(&b, req(on("rig"), MIN, Priority::Normal, "y"));
                f.end(&first.lease_id);
                f.dispatch();
                let offer = f.offered_lease(&b.holder);
                let lease = f.store.tx(|tx| f.sched.claim(tx, &b, &offer)).unwrap();
                assert_eq!(tag_names(&lease.resource_tags), ["esp32"]);
            }
        }
    }

    mod get_lease {
        use super::*;

        mod on_any_lease {
            use super::*;

            #[test]
            fn lease_carries_resource_description_unit_description_and_rendered_usage() {
                let mut f = fixture();
                let a = caller("a");
                let id = held(&mut f, &a);
                let lease = f.store.tx(|tx| f.sched.get_lease(tx, &a, &id)).unwrap();
                assert_eq!(lease.resource_description, "ESP32 rig");
                assert_eq!(lease.unit_description, "left board");
                assert_eq!(lease.usage, RENDERED);
            }
        }

        mod on_any_lease_with_tags {
            use super::*;

            #[test]
            fn lease_carries_resource_tags() {
                let mut f = fixture();
                let a = caller("a");
                let id = held(&mut f, &a);
                f.set_tags("rig", &["zigbee", "esp32"]);
                let lease = f.store.tx(|tx| f.sched.get_lease(tx, &a, &id)).unwrap();
                assert_eq!(tag_names(&lease.resource_tags), ["esp32", "zigbee"]);
            }
        }

        mod after_a_tag_change {
            use super::*;

            #[test]
            fn shows_the_new_tags() {
                let mut f = fixture();
                let a = caller("a");
                f.seed_rig();
                f.set_tags("rig", &["old"]);
                let id = f
                    .grant(&a, req(on("rig"), MIN, Priority::Normal, "x"))
                    .lease_id;
                f.set_tags("rig", &["new", "fresh"]);
                let lease = f.store.tx(|tx| f.sched.get_lease(tx, &a, &id)).unwrap();
                assert_eq!(tag_names(&lease.resource_tags), ["fresh", "new"]);
            }
        }

        mod after_description_change {
            use super::*;

            #[test]
            fn shows_the_new_descriptions_and_re_rendered_usage() {
                let mut f = fixture();
                let a = caller("a");
                let id = held(&mut f, &a);
                let admin = caller("admin");
                f.store
                    .tx(|tx| -> Result<(), leclerk::inventory::InventoryError> {
                        f.inv.update_resource(
                            tx,
                            &admin,
                            &rid("rig"),
                            ResourcePatch {
                                description: Some("Rig two".into()),
                                ..ResourcePatch::default()
                            },
                        )?;
                        f.inv.update_unit(
                            tx,
                            &admin,
                            &rid("rig"),
                            &uid("u1"),
                            UnitPatch {
                                settings: None,
                                description: Some("right board".into()),
                            },
                        )?;
                        Ok(())
                    })
                    .unwrap();
                let lease = f.store.tx(|tx| f.sched.get_lease(tx, &a, &id)).unwrap();
                assert_eq!(lease.resource_description, "Rig two");
                assert_eq!(lease.unit_description, "right board");
                assert_eq!(
                    lease.usage,
                    "flash: idf.py -p /dev/ttyU1 flash; env ${PORT}; Rig two / right board"
                );
            }
        }
    }

    mod my_leases {
        use super::*;

        mod on_any_lease {
            use super::*;

            #[test]
            fn lease_carries_resource_description_unit_description_and_rendered_usage() {
                let mut f = fixture();
                let a = caller("a");
                held(&mut f, &a);
                let mine = f.store.tx(|tx| f.sched.my_leases(tx, &a)).unwrap();
                assert_eq!(mine.leases.len(), 1);
                assert_eq!(mine.leases[0].resource_description, "ESP32 rig");
                assert_eq!(mine.leases[0].unit_description, "left board");
                assert_eq!(mine.leases[0].usage, RENDERED);
            }

            #[test]
            fn lease_carries_resource_tags() {
                let mut f = fixture();
                let a = caller("a");
                held(&mut f, &a);
                f.set_tags("rig", &["esp32"]);
                let mine = f.store.tx(|tx| f.sched.my_leases(tx, &a)).unwrap();
                assert_eq!(tag_names(&mine.leases[0].resource_tags), ["esp32"]);
            }
        }
    }
}
