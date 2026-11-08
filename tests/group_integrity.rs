//! BDD: identity groups with a live lease are never split, by a settings
//! change or by deleting a unit that bridges two others. Driven through the
//! engine's RPC surface, so every path dispatches as the daemon does.

use std::sync::Arc;
use std::time::Duration;

use leclerk::clock::ManualClock;
use leclerk::config::Config;
use leclerk::engine::{ApiError, Command, Engine, EngineHandle, EngineOptions};
use leclerk::health::HealthResult;
use leclerk::model::{Caller, HealthSpec, HolderRef, ProjectKey, ResourceId, UnitId, UnitSettings};
use leclerk::scheduler::HealthProbe;
use leclerk::store::Store;
use leclerk::store::notices::NoticeKind;
use serde_json::{Value, json};

const T0: i64 = 1_000_000;
const REASON: &str = "the user asked for it";

/// Every unit is healthy.
struct Healthy;

impl HealthProbe for Healthy {
    fn check(&self, _: &HealthSpec, _: &ResourceId, _: &UnitId, _: &UnitSettings) -> HealthResult {
        HealthResult::Healthy
    }
}

fn spawn() -> EngineHandle {
    Engine::spawn_with(
        Store::open_in_memory().unwrap(),
        Arc::new(ManualClock::new(T0)),
        Config::default(),
        Arc::new(Healthy),
        EngineOptions {
            liveness_interval: Duration::from_secs(3600),
        },
    )
    .unwrap()
}

fn holder(tag: &str) -> HolderRef {
    HolderRef::User { tag: tag.into() }
}

fn user(tag: &str) -> Caller {
    Caller {
        holder: holder(tag),
        project: None,
        user_requested: None,
    }
}

fn in_project(tag: &str, project: &str) -> Caller {
    Caller {
        project: Some(ProjectKey(project.into())),
        ..user(tag)
    }
}

fn admin() -> Caller {
    Caller {
        user_requested: Some(REASON.into()),
        ..user("admin")
    }
}

async fn rpc(h: &EngineHandle, method: &str, who: &Caller, args: Value) -> Result<Value, ApiError> {
    h.call(Command::parse(method, who.clone(), args)?).await
}

async fn ok(h: &EngineHandle, method: &str, who: &Caller, args: Value) -> Value {
    rpc(h, method, who, args)
        .await
        .unwrap_or_else(|e| panic!("{method} failed: {e:?}"))
}

async fn err(h: &EngineHandle, method: &str, who: &Caller, args: Value) -> ApiError {
    match rpc(h, method, who, args).await {
        Ok(v) => panic!("{method} unexpectedly succeeded: {v}"),
        Err(e) => e,
    }
}

async fn resource(h: &EngineHandle, who: &Caller, id: &str, ty: &str, scope: Value) {
    ok(
        h,
        "create_resource",
        who,
        json!({"id": id, "type": ty, "scope": scope}),
    )
    .await;
}

async fn unit(h: &EngineHandle, who: &Caller, res: &str, id: &str, settings: Value) {
    ok(
        h,
        "add_unit",
        who,
        json!({"resource": res, "id": id, "settings": settings}),
    )
    .await;
}

fn generic(key: &str) -> Value {
    json!({"type": "generic", "identity_key": key})
}

fn service(host: &str, port: u16, id: Option<&str>) -> Value {
    json!({"type": "service", "mode": "connect", "scheme": null, "host": host, "port": port,
           "service_id": id})
}

async fn acquire(h: &EngineHandle, who: &Caller, res: &str) -> Value {
    ok(
        h,
        "acquire",
        who,
        json!({"target": {"resource": res}, "duration_ms": 600_000, "purpose": "test"}),
    )
    .await
}

async fn offers(h: &EngineHandle, who: &HolderRef) -> usize {
    h.wait_notices(who.clone(), Duration::ZERO)
        .await
        .unwrap()
        .iter()
        .filter(|n| n.kind == NoticeKind::Offered)
        .count()
}

async fn audit_of(h: &EngineHandle, action: &str) -> Vec<Value> {
    ok(h, "list_audit", &admin(), json!({"action": action}))
        .await
        .as_array()
        .unwrap()
        .clone()
}

fn agent1() -> Caller {
    in_project("agent1", "/p1")
}

fn agent2() -> Caller {
    in_project("agent2", "/p2")
}

/// The reviewer's bridge: `a` = db1:5432 id pg (user scope), `b` =
/// db1:5432 (/p1), `c` = db2:6543 id pg (/p2). `b` and `c` match only `a`,
/// so `a` is the one unit that makes the three one group.
async fn bridge(h: &EngineHandle) {
    resource(h, &user("owner"), "a", "service", json!("user")).await;
    unit(
        h,
        &user("owner"),
        "a",
        "u",
        service("db1", 5432, Some("pg")),
    )
    .await;
    resource(h, &agent1(), "b", "service", json!({"project": "/p1"})).await;
    unit(h, &agent1(), "b", "u", service("db1", 5432, None)).await;
    resource(h, &agent2(), "c", "service", json!({"project": "/p2"})).await;
    unit(h, &agent2(), "c", "u", service("db2", 6543, Some("pg"))).await;
}

/// The bridge with `b` held by agent1 and agent2 queued on `c`.
async fn bridge_held() -> EngineHandle {
    let h = spawn();
    bridge(&h).await;
    assert_eq!(acquire(&h, &agent1(), "b").await["outcome"], "granted");
    assert_eq!(acquire(&h, &agent2(), "c").await["outcome"], "queued");
    h
}

/// The group survives: agent2 got no offer, and once agent1 holds `b`
/// (again), acquiring `c` still queues.
async fn assert_one_group(h: &EngineHandle) {
    assert_eq!(offers(h, &agent2().holder).await, 0);
    let again = acquire(h, &in_project("agent3", "/p2"), "c").await;
    assert_eq!(again["outcome"], "queued", "{again}");
}

mod inventory {
    use super::*;

    mod update_unit {
        use super::*;

        /// g-a (user) and g-b (/p1) share an identity key, so they are linked;
        /// agent1 holds g-b.
        async fn leased_pair() -> EngineHandle {
            let h = spawn();
            resource(&h, &user("owner"), "g-a", "generic", json!("user")).await;
            unit(&h, &user("owner"), "g-a", "u", generic("same")).await;
            resource(&h, &agent1(), "g-b", "generic", json!({"project": "/p1"})).await;
            unit(&h, &agent1(), "g-b", "u", generic("same")).await;
            assert_eq!(acquire(&h, &agent1(), "g-b").await["outcome"], "granted");
            h
        }

        mod on_a_change_that_would_split_a_leased_group {
            use super::*;

            #[tokio::test]
            async fn returns_busy() {
                let h = leased_pair().await;
                let e = err(
                    &h,
                    "update_unit",
                    &agent1(),
                    json!({"resource": "g-b", "unit": "u", "settings": generic("other")}),
                )
                .await;
                assert_eq!(e.code, "busy");
                assert!(e.message.contains("release or retire"), "{}", e.message);
            }

            #[tokio::test]
            async fn refuses_a_change_to_the_free_partner_too() {
                let h = leased_pair().await;
                let e = err(
                    &h,
                    "update_unit",
                    &user("owner"),
                    json!({"resource": "g-a", "unit": "u", "settings": generic("other")}),
                )
                .await;
                assert_eq!(e.code, "busy");
            }

            #[tokio::test]
            async fn keeps_the_group_so_acquiring_the_partner_queues() {
                let h = leased_pair().await;
                err(
                    &h,
                    "update_unit",
                    &agent1(),
                    json!({"resource": "g-b", "unit": "u", "settings": generic("other")}),
                )
                .await;
                let out = acquire(&h, &user("someone"), "g-a").await;
                assert_eq!(out["outcome"], "queued", "{out}");
            }

            #[tokio::test]
            async fn persists_nothing_and_writes_no_audit() {
                let h = leased_pair().await;
                err(
                    &h,
                    "update_unit",
                    &agent1(),
                    json!({"resource": "g-b", "unit": "u", "settings": generic("other")}),
                )
                .await;
                let got = ok(&h, "get_resource", &agent1(), json!({"id": "g-b"})).await;
                assert_eq!(got["units"][0]["settings"], generic("same"));
                assert_eq!(audit_of(&h, "unit_updated").await, Vec::<Value>::new());
            }
        }

        mod on_a_change_kept_together_by_a_manual_link {
            use super::*;

            #[tokio::test]
            async fn succeeds() {
                let h = leased_pair().await;
                ok(
                    &h,
                    "admin_link",
                    &admin(),
                    json!({"a": {"resource": "g-a", "scope": "user", "unit": "u"},
                           "b": {"resource": "g-b", "scope": "project:/p1", "unit": "u"}}),
                )
                .await;
                ok(
                    &h,
                    "update_unit",
                    &agent1(),
                    json!({"resource": "g-b", "unit": "u", "settings": generic("other")}),
                )
                .await;
            }
        }

        mod on_a_change_splitting_a_free_group {
            use super::*;

            #[tokio::test]
            async fn succeeds_and_the_units_are_apart() {
                let h = spawn();
                resource(&h, &user("owner"), "g-a", "generic", json!("user")).await;
                unit(&h, &user("owner"), "g-a", "u", generic("same")).await;
                resource(&h, &agent1(), "g-b", "generic", json!({"project": "/p1"})).await;
                unit(&h, &agent1(), "g-b", "u", generic("same")).await;
                ok(
                    &h,
                    "update_unit",
                    &agent1(),
                    json!({"resource": "g-b", "unit": "u", "settings": generic("other")}),
                )
                .await;
                assert_eq!(acquire(&h, &agent1(), "g-b").await["outcome"], "granted");
                assert_eq!(acquire(&h, &user("x"), "g-a").await["outcome"], "granted");
            }
        }
    }

    mod remove_unit {
        use super::*;

        mod on_a_free_unit_bridging_a_leased_group {
            use super::*;

            #[tokio::test]
            async fn keeps_the_remaining_members_one_group() {
                let h = bridge_held().await;
                ok(
                    &h,
                    "remove_unit",
                    &user("owner"),
                    json!({"resource": "a", "unit": "u"}),
                )
                .await;
                assert_one_group(&h).await;
            }

            #[tokio::test]
            async fn audits_group_preserved_with_the_new_link() {
                let h = bridge_held().await;
                ok(
                    &h,
                    "remove_unit",
                    &user("owner"),
                    json!({"resource": "a", "unit": "u"}),
                )
                .await;
                let entries = audit_of(&h, "group_preserved").await;
                assert_eq!(entries.len(), 1, "{entries:?}");
                assert_eq!(entries[0]["detail"]["unit"], "a/u");
                assert_eq!(entries[0]["detail"]["links"], json!([["b/u", "c/u"]]));
            }
        }

        mod on_a_unit_with_one_neighbour {
            use super::*;

            #[tokio::test]
            async fn writes_no_group_preserved_audit() {
                let h = spawn();
                resource(&h, &user("owner"), "g-a", "generic", json!("user")).await;
                unit(&h, &user("owner"), "g-a", "u", generic("same")).await;
                resource(&h, &agent1(), "g-b", "generic", json!({"project": "/p1"})).await;
                unit(&h, &agent1(), "g-b", "u", generic("same")).await;
                ok(
                    &h,
                    "remove_unit",
                    &user("owner"),
                    json!({"resource": "g-a", "unit": "u"}),
                )
                .await;
                assert_eq!(audit_of(&h, "group_preserved").await, Vec::<Value>::new());
            }
        }
    }

    mod retire_unit {
        use super::*;

        mod on_a_free_bridge_removed_at_once {
            use super::*;

            #[tokio::test]
            async fn keeps_the_remaining_members_one_group() {
                let h = bridge_held().await;
                ok(
                    &h,
                    "retire_unit",
                    &user("owner"),
                    json!({"resource": "a", "unit": "u"}),
                )
                .await;
                assert_one_group(&h).await;
            }
        }
    }

    mod delete_resource {
        use super::*;

        mod on_a_free_resource_whose_unit_bridges_a_leased_group {
            use super::*;

            #[tokio::test]
            async fn keeps_the_remaining_members_one_group() {
                let h = bridge_held().await;
                ok(&h, "delete_resource", &user("owner"), json!({"id": "a"})).await;
                assert_one_group(&h).await;
            }
        }
    }
}

mod scheduler {
    use super::*;

    mod sweep_retiring {
        use super::*;

        mod on_a_retired_bridge_whose_lease_ends {
            use super::*;

            #[tokio::test]
            async fn keeps_the_remaining_members_one_group() {
                let h = spawn();
                bridge(&h).await;
                let lease = acquire(&h, &user("h"), "a").await;
                assert_eq!(lease["outcome"], "granted");
                let id = lease["lease_id"].clone();
                ok(
                    &h,
                    "retire_unit",
                    &user("owner"),
                    json!({"resource": "a", "unit": "u"}),
                )
                .await;
                ok(&h, "release", &user("h"), json!({"lease_id": id})).await;
                ok(
                    &h,
                    "confirm",
                    &user("h"),
                    json!({"lease_id": id, "how": "clean"}),
                )
                .await;
                let got = rpc(&h, "get_resource", &user("owner"), json!({"id": "a"})).await;
                assert_eq!(got.unwrap()["units"], json!([]), "the sweep removed it");
                assert_eq!(acquire(&h, &agent1(), "b").await["outcome"], "granted");
                assert_eq!(acquire(&h, &agent2(), "c").await["outcome"], "queued");
            }
        }
    }
}

mod admin {
    use super::*;

    mod remove_unit {
        use super::*;

        mod on_a_bridge_of_a_leased_group {
            use super::*;

            #[tokio::test]
            async fn keeps_the_remaining_members_one_group() {
                let h = bridge_held().await;
                ok(
                    &h,
                    "admin_remove_unit",
                    &admin(),
                    json!({"resource": "a", "scope": "user", "unit": "u"}),
                )
                .await;
                assert_one_group(&h).await;
            }
        }
    }

    mod delete_resource {
        use super::*;

        mod on_a_bridge_resource_of_a_leased_group {
            use super::*;

            #[tokio::test]
            async fn keeps_the_remaining_members_one_group() {
                let h = bridge_held().await;
                ok(
                    &h,
                    "admin_delete_resource",
                    &admin(),
                    json!({"resource": "a", "scope": "user"}),
                )
                .await;
                assert_one_group(&h).await;
            }
        }
    }

    mod import {
        use super::*;

        mod on_replacing_a_bridge_resource_with_a_non_matching_unit {
            use super::*;

            #[tokio::test]
            async fn keeps_the_remaining_members_one_group() {
                let h = bridge_held().await;
                let doc = json!({
                    "version": 1,
                    "resources": [{
                        "id": "a", "type": "service", "scope": "user",
                        "units": [{"id": "u", "settings": service("db3", 1111, None)}],
                    }],
                });
                ok(
                    &h,
                    "admin_import",
                    &admin(),
                    json!({"doc": doc, "replace": true}),
                )
                .await;
                assert_one_group(&h).await;
            }
        }
    }
}
