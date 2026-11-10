//! BDD `AdminOps` and `rehome_holder`, driven through the engine's RPC
//! surface.

use std::sync::Arc;
use std::time::Duration;

use leclerk::clock::ManualClock;
use leclerk::config::Config;
use leclerk::engine::{ApiError, Command, Engine, EngineHandle, EngineOptions};
use leclerk::health::HealthResult;
use leclerk::model::{
    Caller, HealthSpec, HolderRef, Host, ProjectKey, ResourceId, UnitId, UnitSettings,
};
use leclerk::scheduler::{HealthProbe, liveness};
use leclerk::store::Store;
use leclerk::store::notices::{Notice, NoticeKind};
use serde_json::{Value, json};
use tempfile::TempDir;

const T0: i64 = 1_000_000;
const PATIENCE: Duration = Duration::from_secs(10);
const REASON: &str = "the user asked for it";

/// Every unit is healthy.
struct Healthy;

impl HealthProbe for Healthy {
    fn check(&self, _: &HealthSpec, _: &ResourceId, _: &UnitId, _: &UnitSettings) -> HealthResult {
        HealthResult::Healthy
    }
}

/// Liveness probes never fire during a test, so CLI holders with made-up
/// start times keep their leases until an admin acts.
fn options() -> EngineOptions {
    EngineOptions {
        liveness_interval: Duration::from_secs(3600),
    }
}

fn spawn_on(store: Store) -> (EngineHandle, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::new(T0));
    let handle = Engine::spawn_with(
        store,
        clock.clone(),
        Config::default(),
        Arc::new(Healthy),
        options(),
    )
    .unwrap();
    (handle, clock)
}

fn spawn() -> (EngineHandle, Arc<ManualClock>) {
    spawn_on(Store::open_in_memory().unwrap())
}

async fn stop(h: &EngineHandle) {
    ok(h, "daemon_stop", &admin(), json!({})).await;
    tokio::time::timeout(PATIENCE, h.stopped()).await.unwrap();
}

fn holder(tag: &str) -> HolderRef {
    HolderRef::User { tag: tag.into() }
}

fn user(tag: &str) -> Caller {
    Caller {
        holder: holder(tag),
        project: None,
        override_reason: None,
    }
}

fn in_project(tag: &str, project: &str) -> Caller {
    Caller {
        project: Some(ProjectKey(project.into())),
        ..user(tag)
    }
}

fn session(id: &str) -> Caller {
    Caller {
        holder: HolderRef::AgentSession {
            session_id: id.into(),
        },
        project: None,
        override_reason: None,
    }
}

fn cli(pid: u32, start_time: u64) -> Caller {
    Caller {
        holder: HolderRef::CliProcess { pid, start_time },
        project: None,
        override_reason: None,
    }
}

/// The administrator, acting on a user request.
fn admin() -> Caller {
    Caller {
        override_reason: Some(REASON.into()),
        ..user("admin")
    }
}

/// This test process, standing for the Claude Code process of the agent
/// sessions a test calls as.
fn me() -> Host {
    let pid = std::process::id();
    let start_time = liveness::start_time(pid).expect("this process is running");
    Host { pid, start_time }
}

async fn rpc(
    h: &EngineHandle,
    method: &str,
    caller: &Caller,
    args: Value,
) -> Result<Value, ApiError> {
    h.call_from(Command::parse(method, caller.clone(), args)?, Some(me()))
        .await
}

async fn ok(h: &EngineHandle, method: &str, caller: &Caller, args: Value) -> Value {
    rpc(h, method, caller, args)
        .await
        .unwrap_or_else(|e| panic!("{method} failed: {e:?}"))
}

async fn err(h: &EngineHandle, method: &str, caller: &Caller, args: Value) -> ApiError {
    match rpc(h, method, caller, args).await {
        Ok(v) => panic!("{method} unexpectedly succeeded: {v}"),
        Err(e) => e,
    }
}

/// A generic resource in `scope` (as JSON) with one unit per `(id, key)`.
async fn seed(h: &EngineHandle, who: &Caller, id: &str, scope: Value, units: &[(&str, &str)]) {
    ok(
        h,
        "create_resource",
        who,
        json!({"id": id, "type": "generic", "scope": scope}),
    )
    .await;
    for (u, key) in units {
        ok(
            h,
            "add_unit",
            who,
            json!({
                "resource": id,
                "id": u,
                "settings": {"type": "generic", "identity_key": key},
            }),
        )
        .await;
    }
}

/// A user-scope generic resource `board` with one unit per id.
async fn seed_board(h: &EngineHandle, units: &[&str]) {
    let pairs: Vec<(&str, String)> = units.iter().map(|u| (*u, format!("key-{u}"))).collect();
    let refs: Vec<(&str, &str)> = pairs.iter().map(|(u, k)| (*u, k.as_str())).collect();
    seed(h, &user("owner"), "board", json!("user"), &refs).await;
}

async fn acquire_on(h: &EngineHandle, who: &Caller, target: Value, extra: Value) -> Value {
    let mut args = json!({"target": target, "duration_ms": 600_000, "purpose": "test"});
    for (k, v) in extra.as_object().unwrap() {
        args[k] = v.clone();
    }
    ok(h, "acquire", who, args).await
}

async fn acquire(h: &EngineHandle, who: &Caller) -> Value {
    acquire_on(h, who, json!({"resource": "board"}), json!({})).await
}

async fn granted(h: &EngineHandle, who: &Caller) -> String {
    let out = acquire(h, who).await;
    assert_eq!(out["outcome"], "granted", "{out}");
    out["lease_id"].as_str().unwrap().to_owned()
}

async fn queued(h: &EngineHandle, who: &Caller) -> String {
    let out = acquire(h, who).await;
    assert_eq!(out["outcome"], "queued", "{out}");
    out["request_id"].as_str().unwrap().to_owned()
}

async fn notices(h: &EngineHandle, who: &HolderRef) -> Vec<Notice> {
    h.wait_notices(who.clone(), Duration::ZERO).await.unwrap()
}

fn kinds(notices: &[Notice]) -> Vec<NoticeKind> {
    notices.iter().map(|n| n.kind).collect()
}

async fn audit_of(h: &EngineHandle, action: &str) -> Vec<Value> {
    ok(h, "list_audit", &admin(), json!({"action": action}))
        .await
        .as_array()
        .unwrap()
        .clone()
}

/// The audit entry an admin op writes names the actor and the user request.
async fn assert_admin_audit(h: &EngineHandle, action: &str) {
    let entries = audit_of(h, action).await;
    assert_eq!(entries.len(), 1, "{action}: {entries:?}");
    assert_eq!(entries[0]["actor"], json!({"kind": "user", "tag": "admin"}));
    assert_eq!(entries[0]["override_reason"], REASON);
}

/// The status entry of `resource`/`unit` in the all-scopes view.
async fn unit_status(h: &EngineHandle, resource: &str, unit: &str) -> Option<Value> {
    let status = ok(h, "status", &admin(), json!({"view": "all"})).await;
    status["resources"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["id"] == resource)
        .flat_map(|r| r["units"].as_array().unwrap().clone())
        .find(|u| u["id"] == unit)
}

async fn queue_of(h: &EngineHandle, resource: &str) -> Vec<Value> {
    let status = ok(h, "status", &admin(), json!({"view": "all"})).await;
    status["resources"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["id"] == resource)
        .flat_map(|r| r["queue"].as_array().unwrap().clone())
        .collect()
}

async fn export(h: &EngineHandle) -> Value {
    ok(h, "admin_export", &admin(), json!({})).await
}

fn uref(resource: &str, unit: &str) -> Value {
    json!({"resource": resource, "unit": unit})
}

/// `board`/u1 is manually linked to `other`/p1; `a` holds u1.
async fn held_with_partner() -> (EngineHandle, String) {
    let (h, _clock) = spawn();
    seed_board(&h, &["u1"]).await;
    seed(&h, &user("owner"), "other", json!("user"), &[("p1", "kp")]).await;
    ok(
        &h,
        "admin_link",
        &admin(),
        json!({"a": uref("board", "u1"), "b": uref("other", "p1")}),
    )
    .await;
    let out = acquire_on(
        &h,
        &user("a"),
        json!({"resource": "board", "unit": "u1"}),
        json!({}),
    )
    .await;
    assert_eq!(out["outcome"], "granted", "{out}");
    (h, out["lease_id"].as_str().unwrap().to_owned())
}

/// The partner of a force-ended unit is free but marked dirty.
async fn assert_partner_dirty(h: &EngineHandle) {
    let partner = unit_status(h, "other", "p1").await.unwrap();
    assert_eq!(partner["occupancy"]["kind"], "free", "{partner}");
    assert_eq!(partner["dirty"]["reason"], "admin_forced", "{partner}");
}

mod admin_ops {
    use super::*;

    mod admin_release {
        use super::*;

        mod on_lease_on_a_linked_unit {
            use super::*;

            #[tokio::test]
            async fn keeps_an_earlier_dirty_record_of_the_partner() {
                let (h, lease) = held_with_partner().await;
                ok(&h, "admin_release", &admin(), json!({"lease_id": lease})).await;
                // u1 and p1 are now dirty from a. Clean u1 only; b takes it
                // and is forced off too.
                ok(&h, "admin_clean", &admin(), uref("board", "u1")).await;
                let out = acquire_on(
                    &h,
                    &user("b"),
                    json!({"resource": "board", "unit": "u1"}),
                    json!({}),
                )
                .await;
                assert_eq!(out["outcome"], "granted", "{out}");
                ok(
                    &h,
                    "admin_release",
                    &admin(),
                    json!({"lease_id": out["lease_id"]}),
                )
                .await;
                let partner = unit_status(&h, "other", "p1").await.unwrap();
                assert_eq!(partner["dirty"]["reason"], "admin_forced");
                assert_eq!(partner["dirty"]["previous_holder"], json!(holder("a")));
                let unit = unit_status(&h, "board", "u1").await.unwrap();
                assert_eq!(unit["dirty"]["previous_holder"], json!(holder("b")));
            }

            #[tokio::test]
            async fn marks_the_linked_partner_dirty_admin_forced() {
                let (h, lease) = held_with_partner().await;
                ok(&h, "admin_release", &admin(), json!({"lease_id": lease})).await;
                assert_partner_dirty(&h).await;
            }
        }

        mod on_held_lease {
            use super::*;

            async fn released() -> EngineHandle {
                let (h, _clock) = spawn();
                seed_board(&h, &["u1"]).await;
                let lease = granted(&h, &user("a")).await;
                queued(&h, &user("b")).await;
                notices(&h, &holder("a")).await;
                notices(&h, &holder("b")).await;
                ok(&h, "admin_release", &admin(), json!({"lease_id": lease})).await;
                h
            }

            #[tokio::test]
            async fn ends_the_lease_now() {
                let h = released().await;
                let got = notices(&h, &holder("a")).await;
                assert_eq!(kinds(&got), vec![NoticeKind::LeaseEnded]);
                let ended = audit_of(&h, "lease_ended").await;
                assert_eq!(ended[0]["detail"]["cause"], "admin_forced");
            }

            #[tokio::test]
            async fn marks_the_unit_dirty_admin_forced() {
                let h = released().await;
                let got = notices(&h, &holder("b")).await;
                assert_eq!(kinds(&got), vec![NoticeKind::Offered]);
                let dirty = serde_json::to_value(got[0].dirty.as_ref().unwrap()).unwrap();
                assert_eq!(dirty["reason"], "admin_forced");
            }

            #[tokio::test]
            async fn dispatches_to_the_next_waiter() {
                let h = released().await;
                let unit = unit_status(&h, "board", "u1").await.unwrap();
                assert_eq!(unit["occupancy"]["holder"], json!(holder("b")));
                assert_eq!(unit["occupancy"]["state"], "offered");
            }

            #[tokio::test]
            async fn audits_admin_release() {
                let h = released().await;
                assert_admin_audit(&h, "admin_release").await;
            }
        }

        mod on_unknown_lease {
            use super::*;

            #[tokio::test]
            async fn returns_not_found() {
                let (h, _clock) = spawn();
                let e = err(
                    &h,
                    "admin_release",
                    &admin(),
                    json!({"lease_id": "lse_01ARZ3NDEKTSV4RRFFQ69G5FAV"}),
                )
                .await;
                assert_eq!(e.code, "not_found");
            }
        }
    }

    mod admin_evict {
        use super::*;

        mod on_holder_of_a_linked_unit {
            use super::*;

            #[tokio::test]
            async fn marks_the_linked_partner_dirty_admin_forced() {
                let (h, _) = held_with_partner().await;
                ok(&h, "admin_evict", &admin(), json!({"holder": "user:a"})).await;
                assert_partner_dirty(&h).await;
            }
        }

        /// `victim` holds u1 and waits for u2, which `a` holds
        /// interruptibly at low priority, so `a` is asked to yield.
        async fn evicted(spec: &str, victim: &Caller) -> EngineHandle {
            let (h, _clock) = spawn();
            seed_board(&h, &["u1", "u2"]).await;
            granted(&h, victim).await;
            acquire_on(
                &h,
                &user("a"),
                json!({"resource": "board", "unit": "u2"}),
                json!({"interruptible": true, "priority": "low"}),
            )
            .await;
            let out = acquire_on(
                &h,
                victim,
                json!({"resource": "board", "unit": "u2"}),
                json!({"priority": "high"}),
            )
            .await;
            assert_eq!(out["outcome"], "queued");
            assert_eq!(
                kinds(&notices(&h, &holder("a")).await),
                vec![NoticeKind::YieldRequested]
            );
            ok(&h, "admin_evict", &admin(), json!({"holder": spec})).await;
            h
        }

        mod on_holder_with_leases_and_requests {
            use super::*;

            #[tokio::test]
            async fn ends_its_leases_and_marks_units_dirty_admin_forced() {
                let h = evicted("session:s1", &session("s1")).await;
                let unit = unit_status(&h, "board", "u1").await.unwrap();
                assert_eq!(unit["occupancy"]["kind"], "free");
                assert_eq!(unit["dirty"]["reason"], "admin_forced");
            }

            #[tokio::test]
            async fn drops_its_requests() {
                let h = evicted("session:s1", &session("s1")).await;
                assert_eq!(queue_of(&h, "board").await, Vec::<Value>::new());
            }

            #[tokio::test]
            async fn withdraws_the_yield_ask_it_caused() {
                let h = evicted("session:s1", &session("s1")).await;
                assert_eq!(
                    kinds(&notices(&h, &holder("a")).await),
                    vec![NoticeKind::YieldWithdrawn]
                );
            }

            #[tokio::test]
            async fn audits_admin_evict() {
                let h = evicted("session:s1", &session("s1")).await;
                assert_admin_audit(&h, "admin_evict").await;
            }
        }

        mod on_pid_spec {
            use super::*;

            #[tokio::test]
            async fn matches_the_pid_whatever_its_start_time() {
                let h = evicted("pid:4242", &cli(4242, 77)).await;
                let unit = unit_status(&h, "board", "u1").await.unwrap();
                assert_eq!(unit["occupancy"]["kind"], "free");
                assert_eq!(queue_of(&h, "board").await, Vec::<Value>::new());
            }
        }

        mod on_user_spec {
            use super::*;

            #[tokio::test]
            async fn matches_the_user_tag() {
                let h = evicted("user:victim", &user("victim")).await;
                let unit = unit_status(&h, "board", "u1").await.unwrap();
                assert_eq!(unit["occupancy"]["kind"], "free");
            }
        }

        mod on_malformed_spec {
            use super::*;

            #[test]
            fn returns_invalid_request() {
                for spec in ["bogus", "pid:abc", "session:", "host:x"] {
                    let e = Command::parse("admin_evict", admin(), json!({"holder": spec}))
                        .unwrap_err();
                    assert_eq!(e.code, "invalid_request", "{spec}");
                }
            }
        }
    }

    mod admin_dequeue {
        use super::*;

        async fn dequeued() -> (EngineHandle, String) {
            let (h, _clock) = spawn();
            seed_board(&h, &["u1"]).await;
            granted(&h, &user("a")).await;
            let first = queued(&h, &user("b")).await;
            queued(&h, &user("c")).await;
            notices(&h, &holder("b")).await;
            notices(&h, &holder("c")).await;
            ok(&h, "admin_dequeue", &admin(), json!({"request_id": first})).await;
            (h, first)
        }

        mod on_queued_request {
            use super::*;

            #[tokio::test]
            async fn removes_the_request() {
                let (h, first) = dequeued().await;
                let queue = queue_of(&h, "board").await;
                assert_eq!(queue.len(), 1);
                assert_ne!(queue[0]["request_id"], first);
            }

            #[tokio::test]
            async fn tells_its_holder_queue_dropped() {
                let (h, first) = dequeued().await;
                let got = notices(&h, &holder("b")).await;
                assert_eq!(kinds(&got), vec![NoticeKind::QueueDropped]);
                assert_eq!(got[0].request_id.as_ref().unwrap().as_str(), first);
            }

            #[tokio::test]
            async fn tells_later_waiters_their_new_position() {
                let (h, _) = dequeued().await;
                let got = notices(&h, &holder("c")).await;
                assert_eq!(kinds(&got), vec![NoticeKind::PositionChanged]);
                assert_eq!(got[0].position, Some(1));
            }

            #[tokio::test]
            async fn audits_admin_dequeue() {
                let (h, _) = dequeued().await;
                assert_admin_audit(&h, "admin_dequeue").await;
            }
        }

        mod on_unknown_request {
            use super::*;

            #[tokio::test]
            async fn returns_not_found() {
                let (h, _clock) = spawn();
                let e = err(
                    &h,
                    "admin_dequeue",
                    &admin(),
                    json!({"request_id": "req_01ARZ3NDEKTSV4RRFFQ69G5FAV"}),
                )
                .await;
                assert_eq!(e.code, "not_found");
            }
        }
    }

    mod admin_clear_queue {
        use super::*;

        mod on_resource_with_waiters {
            use super::*;

            async fn cleared() -> EngineHandle {
                let (h, _clock) = spawn();
                seed_board(&h, &["u1"]).await;
                granted(&h, &user("a")).await;
                queued(&h, &user("b")).await;
                queued(&h, &user("c")).await;
                notices(&h, &holder("b")).await;
                notices(&h, &holder("c")).await;
                ok(
                    &h,
                    "admin_clear_queue",
                    &admin(),
                    json!({"resource": "board"}),
                )
                .await;
                h
            }

            #[tokio::test]
            async fn removes_every_request() {
                let h = cleared().await;
                assert_eq!(queue_of(&h, "board").await, Vec::<Value>::new());
            }

            #[tokio::test]
            async fn tells_each_holder_queue_dropped() {
                let h = cleared().await;
                for who in ["b", "c"] {
                    assert_eq!(
                        kinds(&notices(&h, &holder(who)).await),
                        vec![NoticeKind::QueueDropped],
                        "{who}"
                    );
                }
            }

            #[tokio::test]
            async fn keeps_the_holder_lease() {
                let h = cleared().await;
                let unit = unit_status(&h, "board", "u1").await.unwrap();
                assert_eq!(unit["occupancy"]["holder"], json!(holder("a")));
            }

            #[tokio::test]
            async fn audits_admin_clear_queue() {
                let h = cleared().await;
                assert_admin_audit(&h, "admin_clear_queue").await;
            }
        }
    }

    mod admin_remove_unit {
        use super::*;

        /// `a` holds u1 of board, which is manually linked to p1 of other;
        /// `b` waits for u1.
        async fn removed() -> EngineHandle {
            let (h, _clock) = spawn();
            seed_board(&h, &["u1", "u2"]).await;
            seed(&h, &user("owner"), "other", json!("user"), &[("p1", "kp")]).await;
            ok(
                &h,
                "admin_link",
                &admin(),
                json!({"a": uref("board", "u1"), "b": uref("other", "p1")}),
            )
            .await;
            acquire_on(
                &h,
                &user("a"),
                json!({"resource": "board", "unit": "u1"}),
                json!({}),
            )
            .await;
            acquire_on(
                &h,
                &user("b"),
                json!({"resource": "board", "unit": "u1"}),
                json!({}),
            )
            .await;
            notices(&h, &holder("a")).await;
            notices(&h, &holder("b")).await;
            ok(
                &h,
                "admin_remove_unit",
                &admin(),
                json!({"resource": "board", "unit": "u1"}),
            )
            .await;
            h
        }

        mod on_held_and_queued_unit {
            use super::*;

            #[tokio::test]
            async fn removes_the_unit() {
                let h = removed().await;
                assert_eq!(unit_status(&h, "board", "u1").await, None);
                assert!(unit_status(&h, "board", "u2").await.is_some());
            }

            #[tokio::test]
            async fn tells_the_holder_resource_removed_and_ends_its_lease() {
                let h = removed().await;
                let got = kinds(&notices(&h, &holder("a")).await);
                assert_eq!(got[0], NoticeKind::ResourceRemoved);
                assert!(got.contains(&NoticeKind::LeaseEnded), "{got:?}");
                let ended = audit_of(&h, "lease_ended").await;
                assert_eq!(ended[0]["detail"]["cause"], "admin_forced");
            }

            #[tokio::test]
            async fn tells_the_waiter_resource_removed() {
                let h = removed().await;
                assert_eq!(
                    kinds(&notices(&h, &holder("b")).await),
                    vec![NoticeKind::ResourceRemoved]
                );
                assert_eq!(queue_of(&h, "board").await, Vec::<Value>::new());
            }

            #[tokio::test]
            async fn keeps_the_linked_partner_without_the_link() {
                let h = removed().await;
                assert!(unit_status(&h, "other", "p1").await.is_some());
                assert_eq!(export(&h).await["links"], json!([]));
            }

            #[tokio::test]
            async fn marks_the_linked_partner_dirty_admin_forced() {
                let h = removed().await;
                assert_partner_dirty(&h).await;
            }

            #[tokio::test]
            async fn audits_admin_remove_unit() {
                let h = removed().await;
                assert_admin_audit(&h, "admin_remove_unit").await;
            }
        }

        mod on_unknown_unit {
            use super::*;

            #[tokio::test]
            async fn returns_not_found() {
                let (h, _clock) = spawn();
                seed_board(&h, &["u1"]).await;
                let e = err(
                    &h,
                    "admin_remove_unit",
                    &admin(),
                    json!({"resource": "board", "unit": "nope"}),
                )
                .await;
                assert_eq!(e.code, "not_found");
            }
        }
    }

    mod admin_delete_resource {
        use super::*;

        mod on_held_linked_unit {
            use super::*;

            #[tokio::test]
            async fn marks_the_linked_partner_dirty_admin_forced() {
                let (h, _) = held_with_partner().await;
                ok(
                    &h,
                    "admin_delete_resource",
                    &admin(),
                    json!({"resource": "board"}),
                )
                .await;
                assert_partner_dirty(&h).await;
            }
        }

        mod on_held_and_queued_resource {
            use super::*;

            async fn deleted() -> EngineHandle {
                let (h, _clock) = spawn();
                seed_board(&h, &["u1"]).await;
                granted(&h, &user("a")).await;
                queued(&h, &user("b")).await;
                notices(&h, &holder("a")).await;
                notices(&h, &holder("b")).await;
                ok(
                    &h,
                    "admin_delete_resource",
                    &admin(),
                    json!({"resource": "board"}),
                )
                .await;
                h
            }

            #[tokio::test]
            async fn removes_the_resource() {
                let h = deleted().await;
                let e = err(&h, "get_resource", &user("x"), json!({"id": "board"})).await;
                assert_eq!(e.code, "not_found");
            }

            #[tokio::test]
            async fn tells_holders_and_waiters_resource_removed() {
                let h = deleted().await;
                assert_eq!(
                    kinds(&notices(&h, &holder("a")).await)[0],
                    NoticeKind::ResourceRemoved
                );
                assert_eq!(
                    kinds(&notices(&h, &holder("b")).await),
                    vec![NoticeKind::ResourceRemoved]
                );
            }

            #[tokio::test]
            async fn audits_admin_delete_resource() {
                let h = deleted().await;
                assert_admin_audit(&h, "admin_delete_resource").await;
            }
        }
    }

    mod admin_clean {
        use super::*;

        mod on_dirty_unit {
            use super::*;

            async fn cleaned() -> EngineHandle {
                let (h, _clock) = spawn();
                seed_board(&h, &["u1"]).await;
                let lease = granted(&h, &user("a")).await;
                ok(&h, "admin_release", &admin(), json!({"lease_id": lease})).await;
                let unit = unit_status(&h, "board", "u1").await.unwrap();
                assert_eq!(unit["dirty"]["reason"], "admin_forced");
                ok(
                    &h,
                    "admin_clean",
                    &admin(),
                    json!({"resource": "board", "unit": "u1"}),
                )
                .await;
                h
            }

            #[tokio::test]
            async fn clears_the_dirty_flag() {
                let h = cleaned().await;
                let unit = unit_status(&h, "board", "u1").await.unwrap();
                assert_eq!(unit["dirty"], Value::Null);
                let out = acquire(&h, &user("b")).await;
                assert_eq!(out["dirty"], Value::Null);
            }

            #[tokio::test]
            async fn audits_admin_clean() {
                let h = cleaned().await;
                assert_admin_audit(&h, "admin_clean").await;
            }
        }
    }

    mod admin_link {
        use super::*;

        mod on_groups_each_with_a_live_lease {
            use super::*;

            #[tokio::test]
            async fn returns_link_conflict_and_writes_nothing() {
                let (h, _clock) = spawn();
                seed(&h, &user("o"), "board", json!("user"), &[("u1", "k1")]).await;
                seed(&h, &user("o"), "other", json!("user"), &[("p1", "k2")]).await;
                granted(&h, &user("a")).await;
                acquire_on(&h, &user("b"), json!({"resource": "other"}), json!({})).await;
                let e = err(
                    &h,
                    "admin_link",
                    &admin(),
                    json!({"a": uref("board", "u1"), "b": uref("other", "p1")}),
                )
                .await;
                assert_eq!(e.code, "link_conflict");
                assert_eq!(export(&h).await["links"], json!([]));
                assert_eq!(audit_of(&h, "admin_link").await, Vec::<Value>::new());
            }
        }

        mod on_auto_linked_pair {
            use super::*;

            #[tokio::test]
            async fn upgrades_the_link_to_manual() {
                let (h, _clock) = spawn();
                seed(&h, &user("o"), "board", json!("user"), &[("u1", "same")]).await;
                let p = in_project("o", "/p");
                seed(&h, &p, "mine", json!({"project": "/p"}), &[("m1", "same")]).await;
                ok(
                    &h,
                    "admin_link",
                    &admin(),
                    json!({"a": uref("board", "u1"),
                           "b": {"resource": "mine", "scope": "project:/p", "unit": "m1"}}),
                )
                .await;
                // An auto link would go when the settings stop matching.
                ok(
                    &h,
                    "update_unit",
                    &p,
                    json!({"resource": "mine", "unit": "m1",
                           "settings": {"type": "generic", "identity_key": "different"}}),
                )
                .await;
                assert_eq!(export(&h).await["links"].as_array().unwrap().len(), 1);
            }
        }

        async fn linked() -> EngineHandle {
            let (h, _clock) = spawn();
            seed(&h, &user("o"), "board", json!("user"), &[("u1", "k1")]).await;
            seed(&h, &user("o"), "other", json!("user"), &[("p1", "k2")]).await;
            ok(
                &h,
                "admin_link",
                &admin(),
                json!({"a": uref("board", "u1"), "b": uref("other", "p1")}),
            )
            .await;
            h
        }

        mod on_two_units {
            use super::*;

            #[tokio::test]
            async fn makes_them_one_identity_group() {
                let h = linked().await;
                granted(&h, &user("a")).await;
                let partner = unit_status(&h, "other", "p1").await.unwrap();
                assert_eq!(partner["occupancy"]["holder"], json!(holder("a")));
                assert_eq!(
                    export(&h).await["links"],
                    json!([[["user", "board", "u1"], ["user", "other", "p1"]]])
                );
            }

            #[tokio::test]
            async fn audits_admin_link() {
                let h = linked().await;
                assert_admin_audit(&h, "admin_link").await;
            }
        }

        mod on_unknown_unit {
            use super::*;

            #[tokio::test]
            async fn returns_not_found() {
                let h = linked().await;
                let e = err(
                    &h,
                    "admin_link",
                    &admin(),
                    json!({"a": uref("board", "u1"), "b": uref("other", "zz")}),
                )
                .await;
                assert_eq!(e.code, "not_found");
            }
        }
    }

    mod admin_unlink {
        use super::*;

        mod on_a_leased_group {
            use super::*;

            #[tokio::test]
            async fn returns_the_live_leases_of_both_resulting_groups() {
                let (h, _clock) = spawn();
                seed(&h, &user("o"), "ra", json!("user"), &[("x", "ka")]).await;
                seed(&h, &user("o"), "rb", json!("user"), &[("x", "kb")]).await;
                ok(
                    &h,
                    "admin_link",
                    &admin(),
                    json!({"a": uref("ra", "x"), "b": uref("rb", "x")}),
                )
                .await;
                let lease = acquire_on(&h, &user("a"), json!({"resource": "ra"}), json!({})).await;
                assert_eq!(lease["outcome"], "granted");
                let out = ok(
                    &h,
                    "admin_unlink",
                    &admin(),
                    json!({"a": uref("ra", "x"), "b": uref("rb", "x")}),
                )
                .await;
                assert_eq!(out, json!({"live_leases": [lease["lease_id"]]}));
            }
        }

        mod on_units_joined_only_through_another {
            use super::*;

            #[tokio::test]
            async fn returns_not_directly_linked_and_writes_nothing() {
                let (h, _clock) = spawn();
                for (r, k) in [("ra", "ka"), ("rb", "kb"), ("rc", "kc")] {
                    seed(&h, &user("o"), r, json!("user"), &[("x", k)]).await;
                }
                for (a, b) in [("ra", "rc"), ("rc", "rb")] {
                    ok(
                        &h,
                        "admin_link",
                        &admin(),
                        json!({"a": uref(a, "x"), "b": uref(b, "x")}),
                    )
                    .await;
                }
                let e = err(
                    &h,
                    "admin_unlink",
                    &admin(),
                    json!({"a": uref("ra", "x"), "b": uref("rb", "x")}),
                )
                .await;
                assert_eq!(e.code, "not_directly_linked");
                assert!(e.message.contains("rc"), "{}", e.message);
                assert_eq!(export(&h).await["links"].as_array().unwrap().len(), 2);
                assert_eq!(audit_of(&h, "admin_unlink").await, Vec::<Value>::new());
            }
        }

        mod on_new_unit_matching_both_sides_while_held {
            use super::*;

            /// The reviewer's scenario: A and B match, are unlinked, and are
            /// held by different holders; X matches both.
            async fn split_and_held() -> EngineHandle {
                let h = unlinked().await;
                granted(&h, &user("a")).await;
                let out = acquire_on(
                    &h,
                    &in_project("b", "/p"),
                    json!({"resource": "mine"}),
                    json!({}),
                )
                .await;
                assert_eq!(out["outcome"], "granted");
                h
            }

            #[tokio::test]
            async fn add_unit_returns_ambiguous_duplicate_and_persists_nothing() {
                let h = split_and_held().await;
                let q = in_project("o", "/q");
                seed(&h, &q, "third", json!({"project": "/q"}), &[]).await;
                let e = err(
                    &h,
                    "add_unit",
                    &q,
                    json!({"resource": "third", "id": "x1",
                           "settings": {"type": "generic", "identity_key": "same"}}),
                )
                .await;
                assert_eq!(e.code, "ambiguous_duplicate");
                let got = ok(&h, "get_resource", &q, json!({"id": "third"})).await;
                assert_eq!(got["units"], json!([]));
                assert_eq!(export(&h).await["links"], json!([]));
                assert_eq!(audit_of(&h, "unit_added").await.len(), 2);
            }

            #[tokio::test]
            async fn update_unit_returns_ambiguous_duplicate_and_persists_nothing() {
                let h = split_and_held().await;
                let q = in_project("o", "/q");
                seed(
                    &h,
                    &q,
                    "third",
                    json!({"project": "/q"}),
                    &[("x1", "elsewhere")],
                )
                .await;
                let e = err(
                    &h,
                    "update_unit",
                    &q,
                    json!({"resource": "third", "unit": "x1",
                           "settings": {"type": "generic", "identity_key": "same"}}),
                )
                .await;
                assert_eq!(e.code, "ambiguous_duplicate");
                let got = ok(&h, "get_resource", &q, json!({"id": "third"})).await;
                assert_eq!(got["units"][0]["settings"]["identity_key"], "elsewhere");
                assert_eq!(export(&h).await["links"], json!([]));
            }

            #[tokio::test]
            async fn import_fails_whole_and_changes_nothing() {
                let h = split_and_held().await;
                let before = export(&h).await;
                let doc = json!({
                    "version": 1,
                    "resources": [{
                        "id": "third", "type": "generic", "scope": {"project": "/q"},
                        "units": [{"id": "x1",
                                   "settings": {"type": "generic", "identity_key": "same"}}],
                    }],
                    "links": [],
                });
                let e = err(&h, "admin_import", &admin(), json!({"doc": doc})).await;
                assert_eq!(e.code, "ambiguous_duplicate");
                assert_eq!(export(&h).await, before);
            }
        }

        /// Two user resources whose units match by identity, so they are
        /// auto-linked; the pair is then unlinked.
        async fn unlinked() -> EngineHandle {
            let (h, _clock) = spawn();
            seed(&h, &user("o"), "board", json!("user"), &[("u1", "same")]).await;
            seed(
                &h,
                &in_project("o", "/p"),
                "mine",
                json!({"project": "/p"}),
                &[("m1", "same")],
            )
            .await;
            assert_eq!(export(&h).await["links"].as_array().unwrap().len(), 1);
            ok(
                &h,
                "admin_unlink",
                &admin(),
                json!({
                    "a": uref("board", "u1"),
                    "b": {"resource": "mine", "scope": "project:/p", "unit": "m1"},
                }),
            )
            .await;
            h
        }

        /// Re-save a unit's settings with a change and back, which makes
        /// `update_unit` recompute its auto links.
        async fn touch(h: &EngineHandle, who: &Caller, resource: &str, unit: &str) {
            for key in ["changed", "same"] {
                ok(
                    h,
                    "update_unit",
                    who,
                    json!({
                        "resource": resource,
                        "unit": unit,
                        "settings": {"type": "generic", "identity_key": key},
                    }),
                )
                .await;
            }
        }

        mod on_linked_pair {
            use super::*;

            #[tokio::test]
            async fn removes_the_link() {
                let h = unlinked().await;
                assert_eq!(export(&h).await["links"], json!([]));
            }

            #[tokio::test]
            async fn splits_the_identity_group() {
                let h = unlinked().await;
                granted(&h, &user("a")).await;
                let other = unit_status(&h, "mine", "m1").await.unwrap();
                assert_eq!(other["occupancy"]["kind"], "free");
            }

            #[tokio::test]
            async fn audits_admin_unlink() {
                let h = unlinked().await;
                assert_admin_audit(&h, "admin_unlink").await;
            }
        }

        mod on_later_update_of_either_unit {
            use super::*;

            #[tokio::test]
            async fn does_not_relink_when_the_first_unit_changes() {
                let h = unlinked().await;
                touch(&h, &user("o"), "board", "u1").await;
                assert_eq!(export(&h).await["links"], json!([]));
            }

            #[tokio::test]
            async fn does_not_relink_when_the_second_unit_changes() {
                let h = unlinked().await;
                touch(&h, &in_project("o", "/p"), "mine", "m1").await;
                assert_eq!(export(&h).await["links"], json!([]));
            }
        }

        mod on_admin_link_after_unlink {
            use super::*;

            #[tokio::test]
            async fn links_again_and_survives_updates() {
                let h = unlinked().await;
                ok(
                    &h,
                    "admin_link",
                    &admin(),
                    json!({
                        "a": {"resource": "mine", "scope": "project:/p", "unit": "m1"},
                        "b": uref("board", "u1"),
                    }),
                )
                .await;
                touch(&h, &user("o"), "board", "u1").await;
                touch(&h, &in_project("o", "/p"), "mine", "m1").await;
                assert_eq!(export(&h).await["links"].as_array().unwrap().len(), 1);
            }
        }
    }

    mod admin_move {
        use super::*;

        mod on_resource_with_lease_and_waiter {
            use super::*;

            #[tokio::test]
            async fn holder_and_waiter_keep_working_in_the_new_scope() {
                let (h, _clock) = spawn();
                let owner = in_project("o", "/p");
                seed(&h, &owner, "mine", json!({"project": "/p"}), &[("m1", "k")]).await;
                let a = in_project("a", "/p");
                let b = in_project("b", "/p");
                let lease = acquire_on(&h, &a, json!({"resource": "mine"}), json!({})).await;
                let lease = lease["lease_id"].clone();
                let queued = acquire_on(&h, &b, json!({"resource": "mine"}), json!({})).await;
                assert_eq!(queued["outcome"], "queued");
                ok(
                    &h,
                    "admin_move",
                    &admin(),
                    json!({"resource": "mine", "scope": "project:/p", "to": "project:/other"}),
                )
                .await;
                ok(&h, "release", &a, json!({"lease_id": lease})).await;
                ok(&h, "confirm", &a, json!({"lease_id": lease})).await;
                let got = notices(&h, &holder("b")).await;
                let offer = got.iter().find(|n| n.kind == NoticeKind::Offered).unwrap();
                let claimed = ok(&h, "claim", &b, json!({"lease_id": offer.lease_id})).await;
                assert_eq!(claimed["state"], "held");
                assert_eq!(claimed["resource"], "mine");
            }
        }

        mod on_free_target_id {
            use super::*;

            async fn moved() -> (EngineHandle, String) {
                let (h, _clock) = spawn();
                let b = in_project("b", "/p/b");
                let a = in_project("a", "/p/a");
                seed(
                    &h,
                    &b,
                    "theirs",
                    json!({"project": "/p/b"}),
                    &[("t1", "same")],
                )
                .await;
                seed(
                    &h,
                    &a,
                    "mine",
                    json!({"project": "/p/a"}),
                    &[("m1", "same")],
                )
                .await;
                let promos = ok(&h, "list_promotions", &admin(), json!({})).await;
                let promo = promos[0]["id"].as_str().unwrap().to_owned();
                ok(
                    &h,
                    "admin_move",
                    &admin(),
                    json!({"resource": "theirs", "scope": "project:/p/b", "to": "user"}),
                )
                .await;
                (h, promo)
            }

            #[tokio::test]
            async fn changes_the_scope() {
                let (h, _) = moved().await;
                let got = ok(&h, "get_resource", &user("x"), json!({"id": "theirs"})).await;
                assert_eq!(got["scope"], "user");
            }

            #[tokio::test]
            async fn deletes_pending_promotions_of_the_resource() {
                let (h, _) = moved().await;
                assert_eq!(
                    ok(&h, "list_promotions", &admin(), json!({})).await,
                    json!([])
                );
            }

            #[tokio::test]
            async fn approving_its_old_promotion_returns_not_found() {
                let (h, promo) = moved().await;
                let e = err(&h, "approve_promotion", &admin(), json!({"id": promo})).await;
                assert_eq!(e.code, "not_found");
            }

            #[tokio::test]
            async fn moves_back_into_a_project() {
                let (h, _) = moved().await;
                ok(
                    &h,
                    "admin_move",
                    &admin(),
                    json!({"resource": "theirs", "to": "project:/p/c"}),
                )
                .await;
                let got = ok(
                    &h,
                    "get_resource",
                    &in_project("c", "/p/c"),
                    json!({"id": "theirs"}),
                )
                .await;
                assert_eq!(got["scope"], json!({"project": "/p/c"}));
            }

            #[tokio::test]
            async fn audits_admin_move() {
                let (h, _) = moved().await;
                assert_admin_audit(&h, "admin_move").await;
            }
        }

        mod on_id_clash_in_target_scope {
            use super::*;

            #[tokio::test]
            async fn returns_id_clash_and_changes_nothing() {
                let (h, _clock) = spawn();
                seed(&h, &user("o"), "board", json!("user"), &[]).await;
                let p = in_project("o", "/p");
                seed(&h, &p, "board", json!({"project": "/p"}), &[]).await;
                let e = err(
                    &h,
                    "admin_move",
                    &admin(),
                    json!({"resource": "board", "scope": "project:/p", "to": "user"}),
                )
                .await;
                assert_eq!(e.code, "id_clash");
                let got = ok(&h, "get_resource", &p, json!({"id": "board"})).await;
                assert_eq!(got["scope"], json!({"project": "/p"}));
                assert_eq!(audit_of(&h, "admin_move").await, Vec::<Value>::new());
            }
        }

        mod on_promotion_of_user_scope_resource {
            use super::*;

            /// A promotion still pending although its resource is already in
            /// user scope (a database from before moves deleted them).
            #[tokio::test]
            async fn approve_returns_not_found() {
                let dir = TempDir::new().unwrap();
                let path = dir.path().join("clerk.db");
                let (h, _clock) = spawn_on(Store::open(&path).unwrap());
                let b = in_project("b", "/p/b");
                seed(
                    &h,
                    &b,
                    "theirs",
                    json!({"project": "/p/b"}),
                    &[("t1", "same")],
                )
                .await;
                let a = in_project("a", "/p/a");
                seed(
                    &h,
                    &a,
                    "mine",
                    json!({"project": "/p/a"}),
                    &[("m1", "same")],
                )
                .await;
                let promos = ok(&h, "list_promotions", &admin(), json!({})).await;
                stop(&h).await;
                let conn = rusqlite::Connection::open(&path).unwrap();
                conn.execute(
                    "UPDATE resources SET scope_kind = 'user', project_key = '' WHERE id = 'theirs'",
                    [],
                )
                .unwrap();
                drop(conn);
                let (h, _clock) = spawn_on(Store::open(&path).unwrap());
                let e = err(
                    &h,
                    "approve_promotion",
                    &admin(),
                    json!({"id": promos[0]["id"]}),
                )
                .await;
                assert_eq!(e.code, "not_found");
            }
        }
    }

    mod admin_rename {
        use super::*;

        mod on_resource {
            use super::*;

            #[tokio::test]
            async fn changes_the_id_and_existing_leases_keep_working() {
                let (h, _clock) = spawn();
                seed_board(&h, &["u1"]).await;
                let lease = granted(&h, &user("a")).await;
                ok(
                    &h,
                    "admin_rename",
                    &admin(),
                    json!({"resource": "board", "new_id": "plank"}),
                )
                .await;
                assert!(unit_status(&h, "plank", "u1").await.is_some());
                let view = ok(&h, "release", &user("a"), json!({"lease_id": lease})).await;
                assert_eq!(view["resource"], "plank");
                assert_eq!(view["state"], "cleaning");
                assert_admin_audit(&h, "admin_rename").await;
            }

            #[tokio::test]
            async fn existing_requests_keep_working() {
                let (h, _clock) = spawn();
                seed_board(&h, &["u1"]).await;
                let lease = granted(&h, &user("a")).await;
                queued(&h, &user("b")).await;
                ok(
                    &h,
                    "admin_rename",
                    &admin(),
                    json!({"resource": "board", "new_id": "plank"}),
                )
                .await;
                ok(&h, "release", &user("a"), json!({"lease_id": lease})).await;
                ok(&h, "confirm", &user("a"), json!({"lease_id": lease})).await;
                let got = notices(&h, &holder("b")).await;
                let offer = got.iter().find(|n| n.kind == NoticeKind::Offered).unwrap();
                assert_eq!(offer.resource.as_ref().unwrap().as_str(), "plank");
            }
        }

        mod on_unit {
            use super::*;

            #[tokio::test]
            async fn changes_the_id_and_existing_leases_keep_working() {
                let (h, _clock) = spawn();
                seed_board(&h, &["u1"]).await;
                let lease = granted(&h, &user("a")).await;
                ok(
                    &h,
                    "admin_rename",
                    &admin(),
                    json!({"resource": "board", "unit": "u1", "new_id": "first"}),
                )
                .await;
                let view = ok(&h, "release", &user("a"), json!({"lease_id": lease})).await;
                assert_eq!(view["unit"], "first");
            }
        }

        mod on_id_clash {
            use super::*;

            #[tokio::test]
            async fn returns_id_clash() {
                let (h, _clock) = spawn();
                seed_board(&h, &["u1", "u2"]).await;
                seed(&h, &user("o"), "plank", json!("user"), &[]).await;
                let e = err(
                    &h,
                    "admin_rename",
                    &admin(),
                    json!({"resource": "board", "new_id": "plank"}),
                )
                .await;
                assert_eq!(e.code, "id_clash");
                let e = err(
                    &h,
                    "admin_rename",
                    &admin(),
                    json!({"resource": "board", "unit": "u1", "new_id": "u2"}),
                )
                .await;
                assert_eq!(e.code, "id_clash");
            }
        }
    }

    mod admin_edit_lease {
        use super::*;

        mod on_new_ends_at {
            use super::*;

            #[tokio::test]
            async fn re_arms_the_expiry_warning() {
                let (h, clock) = spawn();
                seed_board(&h, &["u1"]).await;
                let lease = granted(&h, &user("a")).await;
                // 600 s lease: warning lead is 120 s.
                clock.set(T0 + 500_000);
                assert_eq!(
                    kinds(&notices(&h, &holder("a")).await),
                    vec![NoticeKind::ExpiryWarning]
                );
                let edited = ok(
                    &h,
                    "admin_edit_lease",
                    &admin(),
                    json!({"lease_id": lease, "ends_at": T0 + 1_200_000}),
                )
                .await;
                assert_eq!(edited["ends_at"], T0 + 1_200_000);
                clock.set(T0 + 1_150_000);
                assert_eq!(
                    kinds(&notices(&h, &holder("a")).await),
                    vec![NoticeKind::ExpiryWarning]
                );
                assert_admin_audit(&h, "admin_edit_lease").await;
            }

            #[tokio::test]
            async fn in_the_past_expires_at_the_next_tick() {
                let (h, _clock) = spawn();
                seed_board(&h, &["u1"]).await;
                let lease = granted(&h, &user("a")).await;
                ok(
                    &h,
                    "admin_edit_lease",
                    &admin(),
                    json!({"lease_id": lease, "ends_at": T0 - 1}),
                )
                .await;
                let unit = unit_status(&h, "board", "u1").await.unwrap();
                assert_eq!(unit["occupancy"]["state"], "cleaning");
                let got = kinds(&notices(&h, &holder("a")).await);
                assert!(got.contains(&NoticeKind::CleanupWindowStarted), "{got:?}");
            }
        }

        mod on_priority_and_interruptible {
            use super::*;

            #[tokio::test]
            async fn updates_the_lease() {
                let (h, _clock) = spawn();
                seed_board(&h, &["u1"]).await;
                let lease = granted(&h, &user("a")).await;
                let edited = ok(
                    &h,
                    "admin_edit_lease",
                    &admin(),
                    json!({"lease_id": lease, "priority": "high", "interruptible": true}),
                )
                .await;
                assert_eq!(edited["priority"], "high");
                assert_eq!(edited["interruptible"], true);
            }
        }

        mod on_ended_lease {
            use super::*;

            #[tokio::test]
            async fn returns_bad_state() {
                let (h, _clock) = spawn();
                seed_board(&h, &["u1"]).await;
                let lease = granted(&h, &user("a")).await;
                ok(&h, "admin_release", &admin(), json!({"lease_id": lease})).await;
                let e = err(
                    &h,
                    "admin_edit_lease",
                    &admin(),
                    json!({"lease_id": lease, "priority": "high"}),
                )
                .await;
                assert_eq!(e.code, "bad_state");
            }
        }
    }

    mod admin_doctor {
        use super::*;

        /// A database file holding one problem of each kind, plus a healthy
        /// lease; returns the dir (keep it alive) and the database path.
        async fn broken() -> (TempDir, std::path::PathBuf) {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("clerk.db");
            let (h, _clock) = spawn_on(Store::open(&path).unwrap());
            seed_board(&h, &["u1", "u2"]).await;
            ok(
                &h,
                "create_resource",
                &user("o"),
                json!({"id": "dev", "type": "device", "scope": "user"}),
            )
            .await;
            ok(
                &h,
                "add_unit",
                &user("o"),
                json!({"resource": "dev", "id": "ttyx",
                       "settings": {"type": "device", "path": "/nonexistent/clerk-test-tty"}}),
            )
            .await;
            // A dead CLI holder: this process's pid with a start time that
            // cannot be its own.
            acquire_on(
                &h,
                &cli(std::process::id(), 1),
                json!({"resource": "board", "unit": "u1"}),
                json!({}),
            )
            .await;
            granted(&h, &user("alive")).await;
            stop(&h).await;
            // Foreign keys are off on a plain connection, so dangling rows
            // can be written.
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "PRAGMA foreign_keys = OFF; \
                 INSERT INTO leases (lease_id, unit_row, resource_row, state, holder, priority, \
                     interruptible, purpose, duration_ms, created_at, ends_at) \
                 VALUES ('lse_01ARZ3NDEKTSV4RRFFQ69G5FAV', 9998, 9998, 'held', \
                     '{\"kind\":\"user\",\"tag\":\"ghost\"}', 'normal', 0, 'x', 600000, 0, \
                     999999999999); \
                 INSERT INTO requests (request_id, resource_row, state, holder, priority, \
                     interruptible, purpose, duration_ms, created_at) \
                 VALUES ('req_01ARZ3NDEKTSV4RRFFQ69G5FAV', 9997, 'queued', \
                     '{\"kind\":\"user\",\"tag\":\"lost\"}', 'normal', 0, 'x', 600000, 0); \
                 INSERT INTO unit_links (unit_a, unit_b, origin) VALUES (1, 9999, 'manual');",
            )
            .unwrap();
            drop(conn);
            (dir, path)
        }

        fn finding<'a>(report: &'a Value, kind: &str) -> Vec<&'a Value> {
            report["findings"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|f| f["kind"] == kind)
                .collect()
        }

        mod on_report_only {
            use super::*;

            #[tokio::test]
            async fn reports_every_kind_unfixed() {
                let (_dir, path) = broken().await;
                let (h, _clock) = spawn_on(Store::open(&path).unwrap());
                let report = ok(&h, "admin_doctor", &admin(), json!({"fix": false})).await;
                for kind in [
                    "lease_on_missing_unit",
                    "request_for_missing_resource",
                    "dead_holder",
                    "missing_device_path",
                    "link_to_missing_unit",
                ] {
                    let found = finding(&report, kind);
                    assert_eq!(found.len(), 1, "{kind}: {report}");
                    assert_eq!(found[0]["fixed"], false, "{kind}");
                }
                assert_eq!(report["findings"].as_array().unwrap().len(), 5, "{report}");
                assert_eq!(
                    finding(&report, "lease_on_missing_unit")[0]["subject"],
                    "lse_01ARZ3NDEKTSV4RRFFQ69G5FAV"
                );
            }

            #[tokio::test]
            async fn changes_nothing() {
                let (_dir, path) = broken().await;
                let (h, _clock) = spawn_on(Store::open(&path).unwrap());
                ok(&h, "admin_doctor", &admin(), json!({})).await;
                let again = ok(&h, "admin_doctor", &admin(), json!({})).await;
                assert_eq!(again["findings"].as_array().unwrap().len(), 5);
            }
        }

        mod on_fix {
            use super::*;

            #[tokio::test]
            async fn repairs_each_fixable_finding() {
                let (_dir, path) = broken().await;
                let (h, _clock) = spawn_on(Store::open(&path).unwrap());
                let report = ok(&h, "admin_doctor", &admin(), json!({"fix": true})).await;
                for f in report["findings"].as_array().unwrap() {
                    let fixable = f["kind"] != "missing_device_path";
                    assert_eq!(f["fixed"], fixable, "{f}");
                }
                let again = ok(&h, "admin_doctor", &admin(), json!({})).await;
                assert_eq!(
                    again["findings"],
                    json!([{"kind": "missing_device_path", "subject": "dev/ttyx", "fixed": false}])
                );
            }

            #[tokio::test]
            async fn ends_the_dead_holder_lease_and_keeps_live_ones() {
                let (_dir, path) = broken().await;
                let (h, _clock) = spawn_on(Store::open(&path).unwrap());
                ok(&h, "admin_doctor", &admin(), json!({"fix": true})).await;
                let u1 = unit_status(&h, "board", "u1").await.unwrap();
                assert_eq!(u1["occupancy"]["kind"], "free");
                assert_eq!(u1["dirty"]["reason"], "holder_gone");
                let u2 = unit_status(&h, "board", "u2").await.unwrap();
                assert_eq!(u2["occupancy"]["holder"], json!(holder("alive")));
            }

            #[tokio::test]
            async fn audits_each_repair() {
                let (_dir, path) = broken().await;
                let (h, _clock) = spawn_on(Store::open(&path).unwrap());
                ok(&h, "admin_doctor", &admin(), json!({"fix": true})).await;
                let entries = audit_of(&h, "admin_doctor_fix").await;
                assert_eq!(entries.len(), 4, "{entries:?}");
                assert!(entries.iter().all(|e| e["override_reason"] == REASON));
                assert_admin_audit(&h, "admin_doctor").await;
            }
        }

        /// A database file whose migrated text breaks the rules: resource
        /// `board` has an unknown placeholder in its usage, its unit `u1`
        /// a multiline description; `fine` is untouched.
        async fn legacy_text() -> (TempDir, std::path::PathBuf) {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("clerk.db");
            let (h, _clock) = spawn_on(Store::open(&path).unwrap());
            seed_board(&h, &["u1"]).await;
            seed(&h, &user("o"), "fine", json!("user"), &[("f1", "kf")]).await;
            stop(&h).await;
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "UPDATE resources SET usage = 'flash {unit.pth}' WHERE id = 'board'; \
                 UPDATE units SET description = 'left' || char(10) || 'port' \
                     WHERE id = 'u1';",
            )
            .unwrap();
            drop(conn);
            (dir, path)
        }

        mod on_legacy_text_that_breaks_the_rules {
            use super::*;

            #[tokio::test]
            async fn reports_the_resource_with_invalid_usage() {
                let (_dir, path) = legacy_text().await;
                let (h, _clock) = spawn_on(Store::open(&path).unwrap());
                let report = ok(&h, "admin_doctor", &admin(), json!({})).await;
                let found = finding(&report, "invalid_resource_text");
                assert_eq!(found.len(), 1, "{report}");
                assert_eq!(found[0]["subject"], "board (user)");
                assert_eq!(found[0]["fixed"], false);
            }

            #[tokio::test]
            async fn reports_the_unit_with_a_multiline_description() {
                let (_dir, path) = legacy_text().await;
                let (h, _clock) = spawn_on(Store::open(&path).unwrap());
                let report = ok(&h, "admin_doctor", &admin(), json!({})).await;
                let found = finding(&report, "invalid_unit_text");
                assert_eq!(found.len(), 1, "{report}");
                assert_eq!(found[0]["subject"], "board/u1 (user)");
                assert_eq!(found[0]["fixed"], false);
            }

            #[tokio::test]
            async fn reports_nothing_else() {
                let (_dir, path) = legacy_text().await;
                let (h, _clock) = spawn_on(Store::open(&path).unwrap());
                let report = ok(&h, "admin_doctor", &admin(), json!({})).await;
                assert_eq!(report["findings"].as_array().unwrap().len(), 2, "{report}");
            }

            #[tokio::test]
            async fn leaves_the_text_unchanged_and_unfixed_with_fix() {
                let (_dir, path) = legacy_text().await;
                let (h, _clock) = spawn_on(Store::open(&path).unwrap());
                let report = ok(&h, "admin_doctor", &admin(), json!({"fix": true})).await;
                for f in report["findings"].as_array().unwrap() {
                    assert_eq!(f["fixed"], false, "{f}");
                }
                let out = export(&h).await;
                let board = out["resources"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|r| r["id"] == "board")
                    .unwrap();
                assert_eq!(board["usage"], "flash {unit.pth}");
                assert_eq!(board["units"][0]["description"], "left\nport");
                let again = ok(&h, "admin_doctor", &admin(), json!({})).await;
                assert_eq!(again["findings"].as_array().unwrap().len(), 2, "{again}");
            }

            #[tokio::test]
            async fn export_then_import_into_a_fresh_database_imports_nothing() {
                let (_dir, path) = legacy_text().await;
                let (h, _clock) = spawn_on(Store::open(&path).unwrap());
                let doc = export(&h).await;
                let (fresh, _clock) = spawn();
                let e = err(&fresh, "admin_import", &admin(), json!({"doc": doc})).await;
                assert_eq!(e.code, "unknown_placeholder");
                let after = export(&fresh).await;
                assert_eq!(after["resources"], json!([]));
            }
        }

        mod read_only {
            use super::*;

            #[test]
            fn only_without_fix() {
                let parse = |args| Command::parse("admin_doctor", admin(), args).unwrap();
                assert!(parse(json!({})).read_only());
                assert!(parse(json!({"fix": false})).read_only());
                assert!(!parse(json!({"fix": true})).read_only());
                assert!(
                    Command::parse("admin_export", admin(), json!({}))
                        .unwrap()
                        .read_only()
                );
                assert!(
                    !Command::parse("admin_clean", admin(), uref("r", "u"))
                        .unwrap()
                        .read_only()
                );
            }
        }
    }

    mod admin_export {
        use super::*;

        mod on_inventory {
            use super::*;

            #[tokio::test]
            async fn returns_the_documented_format() {
                let (h, _clock) = spawn();
                ok(
                    &h,
                    "create_resource",
                    &user("o"),
                    json!({"id": "board", "type": "generic", "scope": "user",
                           "description": "boards", "usage": "n", "health": [{"builtin": "none"}]}),
                )
                .await;
                ok(
                    &h,
                    "add_unit",
                    &user("o"),
                    json!({"resource": "board", "id": "u1", "description": "blue",
                           "settings": {"type": "generic", "identity_key": "k1"}}),
                )
                .await;
                let p = in_project("o", "/p");
                seed(&h, &p, "mine", json!({"project": "/p"}), &[("m1", "k1")]).await;
                let got = export(&h).await;
                assert_eq!(
                    got,
                    json!({
                        "version": 1,
                        "resources": [
                            {
                                "id": "board",
                                "type": "generic",
                                "scope": "user",
                                "description": "boards",
                                "usage": "n",
                                "health": [{"builtin": "none"}],
                                "tags": [],
                                "units": [{
                                    "id": "u1",
                                    "settings": {"type": "generic", "identity_key": "k1"},
                                    "description": "blue",
                                }],
                            },
                            {
                                "id": "mine",
                                "type": "generic",
                                "scope": {"project": "/p"},
                                "description": "",
                                "usage": "",
                                "health": null,
                                "tags": [],
                                "units": [{
                                    "id": "m1",
                                    "settings": {"type": "generic", "identity_key": "k1"},
                                    "description": "",
                                }],
                            },
                        ],
                        "links": [[["project:/p", "mine", "m1"], ["user", "board", "u1"]]],
                    })
                );
                assert_admin_audit(&h, "admin_export").await;
            }
        }
    }

    mod admin_import {
        use super::*;

        mod on_doc_link_joining_two_held_groups {
            use super::*;

            #[tokio::test]
            async fn returns_link_conflict_listing_units_and_changes_nothing() {
                let (h, _clock) = spawn();
                seed(&h, &user("o"), "board", json!("user"), &[("u1", "k1")]).await;
                seed(&h, &user("o"), "other", json!("user"), &[("p1", "k2")]).await;
                granted(&h, &user("a")).await;
                acquire_on(&h, &user("b"), json!({"resource": "other"}), json!({})).await;
                let before = export(&h).await;
                let doc = json!({
                    "version": 1,
                    "resources": [{
                        "id": "third", "type": "generic", "scope": {"project": "/q"},
                        "units": [
                            {"id": "x1", "settings": {"type": "generic", "identity_key": "k1"}},
                            {"id": "y1", "settings": {"type": "generic", "identity_key": "k2"}},
                        ],
                    }],
                    "links": [[["project:/q", "third", "x1"], ["project:/q", "third", "y1"]]],
                });
                let e = err(&h, "admin_import", &admin(), json!({"doc": doc})).await;
                assert_eq!(e.code, "link_conflict");
                assert!(e.message.contains("x1"), "{}", e.message);
                assert_eq!(export(&h).await, before);
            }
        }

        fn doc(resource_usage: &str) -> Value {
            json!({
                "version": 1,
                "resources": [{
                    "id": "board",
                    "type": "generic",
                    "scope": "user",
                    "description": "",
                    "usage": resource_usage,
                    "health": null,
                    "units": [{
                        "id": "u1",
                        "settings": {"type": "generic", "identity_key": "k1"},
                        "description": "",
                    }],
                }],
                "links": [],
            })
        }

        mod on_id_clash_without_replace {
            use super::*;

            #[tokio::test]
            async fn returns_import_clash_listing_ids_and_changes_nothing() {
                let (h, _clock) = spawn();
                seed(&h, &user("o"), "board", json!("user"), &[("x1", "kx")]).await;
                let before = export(&h).await;
                let e = err(
                    &h,
                    "admin_import",
                    &admin(),
                    json!({"doc": doc("new"), "replace": false}),
                )
                .await;
                assert_eq!(e.code, "import_clash");
                assert!(e.message.contains("board"), "{}", e.message);
                assert_eq!(export(&h).await, before);
                assert_eq!(audit_of(&h, "admin_import").await, Vec::<Value>::new());
            }
        }

        mod on_replace {
            use super::*;

            #[tokio::test]
            async fn marks_the_linked_partner_of_a_held_unit_dirty_admin_forced() {
                let (h, _) = held_with_partner().await;
                ok(
                    &h,
                    "admin_import",
                    &admin(),
                    json!({"doc": doc("new"), "replace": true}),
                )
                .await;
                assert_partner_dirty(&h).await;
            }

            #[tokio::test]
            async fn overwrites_the_clashing_resource() {
                let (h, _clock) = spawn();
                seed(&h, &user("o"), "board", json!("user"), &[("x1", "kx")]).await;
                ok(
                    &h,
                    "admin_import",
                    &admin(),
                    json!({"doc": doc("new"), "replace": true}),
                )
                .await;
                let got = ok(&h, "get_resource", &user("x"), json!({"id": "board"})).await;
                assert_eq!(got["usage"], "new");
                let units: Vec<&Value> = got["units"].as_array().unwrap().iter().collect();
                assert_eq!(units.len(), 1);
                assert_eq!(units[0]["id"], "u1");
                assert_admin_audit(&h, "admin_import").await;
            }

            #[tokio::test]
            async fn ends_leases_on_the_replaced_resource() {
                let (h, _clock) = spawn();
                seed(&h, &user("o"), "board", json!("user"), &[("x1", "kx")]).await;
                granted(&h, &user("a")).await;
                notices(&h, &holder("a")).await;
                ok(
                    &h,
                    "admin_import",
                    &admin(),
                    json!({"doc": doc("new"), "replace": true}),
                )
                .await;
                assert_eq!(
                    kinds(&notices(&h, &holder("a")).await)[0],
                    NoticeKind::ResourceRemoved
                );
                assert_eq!(
                    unit_status(&h, "board", "u1").await.unwrap()["occupancy"]["kind"],
                    "free"
                );
            }
        }

        mod on_no_clash {
            use super::*;

            #[tokio::test]
            async fn runs_dedup_and_links() {
                let (h, _clock) = spawn();
                let p = in_project("o", "/p");
                seed(&h, &p, "mine", json!({"project": "/p"}), &[("m1", "k1")]).await;
                ok(&h, "admin_import", &admin(), json!({"doc": doc("")})).await;
                assert_eq!(
                    export(&h).await["links"],
                    json!([[["project:/p", "mine", "m1"], ["user", "board", "u1"]]])
                );
            }

            #[tokio::test]
            async fn rejects_a_duplicate_in_scope_and_changes_nothing() {
                let (h, _clock) = spawn();
                seed(&h, &user("o"), "other", json!("user"), &[("o1", "k1")]).await;
                let before = export(&h).await;
                let e = err(&h, "admin_import", &admin(), json!({"doc": doc("")})).await;
                assert_eq!(e.code, "duplicate_in_scope");
                assert_eq!(export(&h).await, before);
            }
        }

        mod on_invalid_doc {
            use super::*;

            #[tokio::test]
            async fn returns_invalid_argument_and_changes_nothing() {
                let (h, _clock) = spawn();
                let mut bad_version = doc("");
                bad_version["version"] = json!(2);
                let mut bad_link = doc("");
                bad_link["links"] = json!([[["user", "board", "u1"], ["user", "nope", "n1"]]]);
                let mut bad_settings = doc("");
                bad_settings["resources"][0]["units"][0]["settings"] =
                    json!({"type": "device", "path": "/dev/x"});
                for bad in [bad_version, bad_link, bad_settings] {
                    let e = err(&h, "admin_import", &admin(), json!({"doc": bad})).await;
                    assert_eq!(e.code, "invalid_argument", "{}", e.message);
                }
                assert_eq!(export(&h).await["resources"], json!([]));
            }
        }

        mod on_export_wipe_import {
            use super::*;

            #[tokio::test]
            async fn keeps_ids_scopes_settings_text_health_and_links() {
                let (h, _clock) = spawn();
                ok(
                    &h,
                    "create_resource",
                    &user("o"),
                    json!({"id": "board", "type": "generic", "scope": "user",
                           "description": "d", "usage": "n", "health": [{"builtin": "none"}]}),
                )
                .await;
                for (u, k) in [("u1", "k1"), ("u2", "k2")] {
                    ok(
                        &h,
                        "add_unit",
                        &user("o"),
                        json!({"resource": "board", "id": u, "description": format!("note-{u}"),
                               "settings": {"type": "generic", "identity_key": k}}),
                    )
                    .await;
                }
                let p = in_project("o", "/p");
                seed(
                    &h,
                    &p,
                    "mine",
                    json!({"project": "/p"}),
                    &[("m1", "k1"), ("m2", "km")],
                )
                .await;
                ok(
                    &h,
                    "create_resource",
                    &user("o"),
                    json!({"id": "svc", "type": "service", "scope": "user"}),
                )
                .await;
                ok(
                    &h,
                    "add_unit",
                    &user("o"),
                    json!({"resource": "svc", "id": "s1", "settings": {"type": "service",
                        "mode": "listen", "scheme": "http", "host": "127.0.0.1", "port": 8080,
                        "service_id": null}}),
                )
                .await;
                // A manual link between units that dedup would never match.
                ok(
                    &h,
                    "admin_link",
                    &admin(),
                    json!({"a": uref("board", "u2"),
                           "b": {"resource": "mine", "scope": "project:/p", "unit": "m2"}}),
                )
                .await;
                let first = export(&h).await;
                assert_eq!(first["links"].as_array().unwrap().len(), 2);

                let (fresh, _clock) = spawn();
                ok(
                    &fresh,
                    "admin_import",
                    &admin(),
                    json!({"doc": first.clone()}),
                )
                .await;
                assert_eq!(export(&fresh).await, first);
            }
        }
    }
}

mod rehome_holder {
    use super::*;

    mod on_caller_is_the_holder {
        use super::*;

        async fn rehomed() -> (EngineHandle, String) {
            let (h, clock) = spawn();
            seed_board(&h, &["u1"]).await;
            let lease = granted(&h, &user("a")).await;
            // Past the warning lead: an expiry warning waits for `a`.
            clock.set(T0 + 500_000);
            ok(&h, "status", &user("a"), json!({})).await;
            h.report_host("s9".into(), me()).await.unwrap();
            ok(
                &h,
                "rehome_holder",
                &user("a"),
                json!({"lease_id": lease, "new_holder": session("s9").holder}),
            )
            .await;
            (h, lease)
        }

        #[tokio::test]
        async fn the_lease_belongs_to_the_new_holder() {
            let (h, lease) = rehomed().await;
            let e = err(&h, "release", &user("a"), json!({"lease_id": lease})).await;
            assert_eq!(e.code, "not_holder");
            let view = ok(&h, "release", &session("s9"), json!({"lease_id": lease})).await;
            assert_eq!(view["holder"], json!(session("s9").holder));
        }

        #[tokio::test]
        async fn its_undelivered_notices_move_too() {
            let (h, lease) = rehomed().await;
            assert_eq!(kinds(&notices(&h, &holder("a")).await), vec![]);
            let got = notices(&h, &session("s9").holder).await;
            assert_eq!(kinds(&got), vec![NoticeKind::ExpiryWarning]);
            assert_eq!(got[0].holder, session("s9").holder);
            assert_eq!(got[0].lease_id.as_ref().unwrap().as_str(), lease);
        }

        #[tokio::test]
        async fn wakes_a_waiting_new_holder() {
            let (h, clock) = spawn();
            seed_board(&h, &["u1"]).await;
            let lease = granted(&h, &user("a")).await;
            clock.set(T0 + 500_000);
            ok(&h, "status", &user("a"), json!({})).await;
            let waiter = {
                let h = h.clone();
                tokio::spawn(async move {
                    h.wait_notices(session("s9").holder, Duration::from_secs(30))
                        .await
                })
            };
            tokio::time::timeout(PATIENCE, async {
                while !h.is_waiting(&session("s9").holder) {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
            h.report_host("s9".into(), me()).await.unwrap();
            ok(
                &h,
                "rehome_holder",
                &user("a"),
                json!({"lease_id": lease, "new_holder": session("s9").holder}),
            )
            .await;
            let got = tokio::time::timeout(PATIENCE, waiter)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(kinds(&got), vec![NoticeKind::ExpiryWarning]);
        }
    }

    mod on_caller_not_the_holder {
        use super::*;

        #[tokio::test]
        async fn returns_not_holder() {
            let (h, _clock) = spawn();
            seed_board(&h, &["u1"]).await;
            let lease = granted(&h, &user("a")).await;
            let e = err(
                &h,
                "rehome_holder",
                &user("b"),
                json!({"lease_id": lease, "new_holder": holder("b")}),
            )
            .await;
            assert_eq!(e.code, "not_holder");
        }
    }
}

mod export_text {
    use super::*;

    fn legacy_doc() -> Value {
        json!({
            "version": 1,
            "resources": [{
                "id": "board",
                "type": "device",
                "scope": "user",
                "description": "boards",
                "notes": "flash {unit.path}",
                "units": [{
                    "id": "u1",
                    "settings": {"type": "device", "path": "/dev/ttyUSB0"},
                    "notes": "left port",
                }],
            }],
        })
    }

    mod admin_import {
        use super::*;

        mod on_an_older_export_with_notes_fields {
            use super::*;

            #[tokio::test]
            async fn imports_resource_notes_as_usage() {
                let (h, _clock) = spawn();
                ok(&h, "admin_import", &admin(), json!({"doc": legacy_doc()})).await;
                let got = ok(&h, "get_resource", &user("x"), json!({"id": "board"})).await;
                assert_eq!(got["usage"], "flash {unit.path}");
            }

            #[tokio::test]
            async fn imports_unit_notes_as_description() {
                let (h, _clock) = spawn();
                ok(&h, "admin_import", &admin(), json!({"doc": legacy_doc()})).await;
                let got = ok(&h, "get_resource", &user("x"), json!({"id": "board"})).await;
                assert_eq!(got["units"][0]["description"], "left port");
            }

            #[tokio::test]
            async fn re_exports_under_the_new_names() {
                let (h, _clock) = spawn();
                ok(&h, "admin_import", &admin(), json!({"doc": legacy_doc()})).await;
                let out = export(&h).await;
                assert_eq!(out["resources"][0]["usage"], "flash {unit.path}");
                assert!(out["resources"][0].get("notes").is_none());
                assert_eq!(out["resources"][0]["units"][0]["description"], "left port");
            }
        }

        mod on_an_imported_usage_with_an_unknown_placeholder {
            use super::*;

            fn bad_doc() -> Value {
                let mut doc = legacy_doc();
                doc["resources"][0]["notes"] = json!("flash {unit.pth}");
                doc
            }

            #[tokio::test]
            async fn fails_with_unknown_placeholder() {
                let (h, _clock) = spawn();
                let e = err(&h, "admin_import", &admin(), json!({"doc": bad_doc()})).await;
                assert_eq!(e.code, "unknown_placeholder");
            }

            #[tokio::test]
            async fn imports_nothing() {
                let (h, _clock) = spawn();
                seed(&h, &user("o"), "other", json!("user"), &[("x1", "kx")]).await;
                let before = export(&h).await;
                // A valid resource comes first, so the failure arrives
                // after a write.
                let mut doc = legacy_doc();
                doc["resources"][0]["id"] = json!("first");
                doc["resources"][0]["notes"] = json!("flash {unit.path}");
                let mut second = bad_doc()["resources"][0].clone();
                second["id"] = json!("second");
                second["units"][0]["settings"]["path"] = json!("/dev/ttyUSB1");
                doc["resources"].as_array_mut().unwrap().push(second);
                let e = err(&h, "admin_import", &admin(), json!({"doc": doc})).await;
                assert_eq!(e.code, "unknown_placeholder");
                let missing = err(&h, "get_resource", &user("x"), json!({"id": "first"})).await;
                assert_eq!(missing.code, "not_found");
                assert_eq!(export(&h).await, before);
            }
        }

        mod on_an_imported_description_that_is_multiline {
            use super::*;

            #[tokio::test]
            async fn fails_with_description_multiline() {
                let (h, _clock) = spawn();
                let mut doc = legacy_doc();
                doc["resources"][0]["units"][0]["notes"] = json!("a\nb");
                let e = err(&h, "admin_import", &admin(), json!({"doc": doc})).await;
                assert_eq!(e.code, "description_multiline");
            }
        }
    }

    mod admin_export {
        use super::*;

        mod on_resources_with_text {
            use super::*;

            #[tokio::test]
            async fn writes_usage_for_resources_and_description_for_units() {
                let (h, _clock) = spawn();
                ok(&h, "admin_import", &admin(), json!({"doc": legacy_doc()})).await;
                let out = export(&h).await;
                let resource = out["resources"][0].as_object().unwrap();
                assert!(resource.contains_key("usage"));
                assert!(!resource.contains_key("notes"));
                let unit = resource["units"][0].as_object().unwrap();
                assert!(unit.contains_key("description"));
                assert!(!unit.contains_key("notes"));
            }
        }
    }
}

mod resource_tags {
    use super::*;

    async fn tagged(h: &EngineHandle, who: &Caller, id: &str, scope: Value, tags: &[&str]) {
        ok(
            h,
            "create_resource",
            who,
            json!({"id": id, "type": "generic", "scope": scope, "tags": tags}),
        )
        .await;
    }

    async fn tags_of(h: &EngineHandle, who: &Caller, id: &str) -> Value {
        ok(h, "get_resource", who, json!({"id": id})).await["tags"].clone()
    }

    mod admin_rename {
        use super::*;

        #[tokio::test]
        async fn the_resource_keeps_its_tags() {
            let (h, _clock) = spawn();
            tagged(&h, &user("o"), "board", json!("user"), &["zigbee", "esp32"]).await;
            ok(
                &h,
                "admin_rename",
                &admin(),
                json!({"resource": "board", "new_id": "plank"}),
            )
            .await;
            assert_eq!(
                tags_of(&h, &user("o"), "plank").await,
                json!(["esp32", "zigbee"])
            );
        }
    }

    mod admin_move {
        use super::*;

        #[tokio::test]
        async fn the_resource_keeps_its_tags() {
            let (h, _clock) = spawn();
            let owner = in_project("o", "/p");
            tagged(&h, &owner, "mine", json!({"project": "/p"}), &["thread"]).await;
            ok(
                &h,
                "admin_move",
                &admin(),
                json!({"resource": "mine", "scope": "project:/p", "to": "user"}),
            )
            .await;
            assert_eq!(tags_of(&h, &user("x"), "mine").await, json!(["thread"]));
        }
    }

    mod approve_promotion {
        use super::*;

        #[tokio::test]
        async fn the_promoted_resource_keeps_its_tags() {
            let (h, _clock) = spawn();
            let (a, b) = (in_project("a", "/p/a"), in_project("b", "/p/b"));
            tagged(&h, &b, "theirs", json!({"project": "/p/b"}), &["thread"]).await;
            ok(
                &h,
                "add_unit",
                &b,
                json!({"resource": "theirs", "id": "t1",
                       "settings": {"type": "generic", "identity_key": "same"}}),
            )
            .await;
            seed(
                &h,
                &a,
                "mine",
                json!({"project": "/p/a"}),
                &[("m1", "same")],
            )
            .await;
            let promos = ok(&h, "list_promotions", &admin(), json!({})).await;
            ok(
                &h,
                "approve_promotion",
                &admin(),
                json!({"id": promos[0]["id"]}),
            )
            .await;
            assert_eq!(tags_of(&h, &user("x"), "theirs").await, json!(["thread"]));
        }
    }

    mod admin_export {
        use super::*;

        #[tokio::test]
        async fn writes_tags_per_resource_sorted() {
            let (h, _clock) = spawn();
            tagged(&h, &user("o"), "board", json!("user"), &["zigbee", "esp32"]).await;
            tagged(&h, &user("o"), "bare", json!("user"), &[]).await;
            let got = export(&h).await;
            let tags: Vec<Value> = got["resources"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["tags"].clone())
                .collect();
            assert_eq!(tags, [json!([]), json!(["esp32", "zigbee"])]);
        }
    }

    mod admin_import {
        use super::*;

        fn doc(resource: &Value) -> Value {
            json!({"version": 1, "resources": [resource], "links": []})
        }

        mod on_a_current_export {
            use super::*;

            #[tokio::test]
            async fn round_trips_tags_identically() {
                let (h, _clock) = spawn();
                tagged(&h, &user("o"), "board", json!("user"), &["zigbee", "esp32"]).await;
                let p = in_project("o", "/p");
                tagged(&h, &p, "mine", json!({"project": "/p"}), &["thread"]).await;
                let before = export(&h).await;
                let (h2, _clock2) = spawn();
                ok(&h2, "admin_import", &admin(), json!({"doc": before})).await;
                assert_eq!(export(&h2).await, before);
                assert_eq!(
                    tags_of(&h2, &user("x"), "board").await,
                    json!(["esp32", "zigbee"])
                );
            }
        }

        mod on_an_export_without_tags_fields {
            use super::*;

            #[tokio::test]
            async fn imports_resources_with_no_tags() {
                let (h, _clock) = spawn();
                let old = doc(&json!({"id": "board", "type": "generic", "scope": "user"}));
                ok(&h, "admin_import", &admin(), json!({"doc": old})).await;
                assert_eq!(tags_of(&h, &user("x"), "board").await, json!([]));
            }
        }

        mod on_an_imported_malformed_tag {
            use super::*;

            #[tokio::test]
            async fn fails_with_invalid_tag_and_imports_nothing() {
                let (h, _clock) = spawn();
                let bad = json!({"version": 1, "links": [], "resources": [
                    {"id": "good", "type": "generic", "scope": "user", "tags": ["fine"]},
                    {"id": "board", "type": "generic", "scope": "user", "tags": ["Thread"]},
                ]});
                let before = export(&h).await;
                let e = err(&h, "admin_import", &admin(), json!({"doc": bad})).await;
                assert_eq!(e.code, "invalid_tag");
                assert_eq!(e.details, Some(json!({"tag": "Thread"})));
                assert_eq!(export(&h).await, before);
            }
        }
    }
}
