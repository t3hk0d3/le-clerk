use std::sync::Arc;
use std::time::Duration;

use axum::body::{self, Body};
use axum::http::{Request, StatusCode};
use leclerk::clock::ManualClock;
use leclerk::config::Config;
use leclerk::daemon::server as http;
use leclerk::engine::{ApiError, Command, Engine, EngineHandle, EngineOptions, MAX_WAIT};
use leclerk::health::HealthResult;
use leclerk::model::{Caller, HealthSpec, HolderRef, ProjectKey, ResourceId, UnitId, UnitSettings};
use leclerk::scheduler::HealthProbe;
use leclerk::store::Store;
use leclerk::store::notices::{Notice, NoticeKind};
use serde_json::{Value, json};
use tower::ServiceExt;

const T0: i64 = 1_000_000;
/// Upper bound for anything event-driven; generous so slow CI never flakes.
const PATIENCE: Duration = Duration::from_secs(10);

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

fn spawn_with(options: EngineOptions) -> (EngineHandle, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::new(T0));
    let handle = Engine::spawn_with(
        Store::open_in_memory().unwrap(),
        clock.clone(),
        Config::default(),
        Arc::new(Healthy),
        options,
    )
    .unwrap();
    (handle, clock)
}

fn spawn() -> (EngineHandle, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::new(T0));
    let handle = Engine::spawn(
        Store::open_in_memory().unwrap(),
        clock.clone(),
        Config::default(),
        Arc::new(Healthy),
    )
    .unwrap();
    (handle, clock)
}

/// Options whose liveness probe never fires during a test, so the only
/// thing that can wake an idle engine is a deadline.
fn no_liveness() -> EngineOptions {
    EngineOptions {
        liveness_interval: Duration::from_secs(3600),
    }
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

async fn rpc(
    h: &EngineHandle,
    method: &str,
    caller: &Caller,
    args: Value,
) -> Result<Value, ApiError> {
    h.call(Command::parse(method, caller.clone(), args)?).await
}

async fn ok(h: &EngineHandle, method: &str, caller: &Caller, args: Value) -> Value {
    rpc(h, method, caller, args)
        .await
        .unwrap_or_else(|e| panic!("{method} failed: {e:?}"))
}

/// A user-scope generic resource `board` with one unit per id.
async fn seed_board(h: &EngineHandle, units: &[&str]) {
    let admin = user("admin");
    ok(
        h,
        "create_resource",
        &admin,
        json!({"id": "board", "type": "generic", "scope": "user"}),
    )
    .await;
    for u in units {
        ok(
            h,
            "add_unit",
            &admin,
            json!({
                "resource": "board",
                "id": u,
                "settings": {"type": "generic", "identity_key": format!("key-{u}")},
            }),
        )
        .await;
    }
}

async fn acquire(h: &EngineHandle, who: &Caller, duration_ms: u64) -> Value {
    ok(
        h,
        "acquire",
        who,
        json!({"target": {"resource": "board"}, "duration_ms": duration_ms, "purpose": "test"}),
    )
    .await
}

/// Wait (bounded) until a long-poll for `who` is parked in the engine.
async fn until_parked(h: &EngineHandle, who: &HolderRef) {
    tokio::time::timeout(PATIENCE, async {
        while !h.is_waiting(who) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("waiter never parked");
}

fn kinds(notices: &[Notice]) -> Vec<NoticeKind> {
    notices.iter().map(|n| n.kind).collect()
}

mod engine {
    use super::*;

    mod call {
        use super::*;

        mod on_concurrent_acquires {
            use super::*;

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn grants_each_unit_once() {
                let (h, _clock) = spawn();
                seed_board(&h, &["u1", "u2"]).await;
                let tasks: Vec<_> = (0..20)
                    .map(|i| {
                        let h = h.clone();
                        tokio::spawn(async move {
                            acquire(&h, &user(&format!("agent-{i}")), 60_000).await
                        })
                    })
                    .collect();
                let mut granted_units = Vec::new();
                let mut queued = 0;
                for t in tasks {
                    let out = t.await.unwrap();
                    match out["outcome"].as_str().unwrap() {
                        "granted" => granted_units.push(out["unit"].as_str().unwrap().to_owned()),
                        "queued" => queued += 1,
                        other => panic!("unexpected outcome {other}"),
                    }
                }
                granted_units.sort();
                assert_eq!(granted_units, vec!["u1", "u2"]);
                assert_eq!(queued, 18);
            }
        }

        mod on_engine_stopped {
            use super::*;

            #[tokio::test]
            async fn returns_engine_unavailable() {
                let (h, _clock) = spawn();
                ok(&h, "daemon_stop", &user("admin"), json!({})).await;
                tokio::time::timeout(PATIENCE, h.stopped()).await.unwrap();
                let err = rpc(&h, "list_resources", &user("a"), json!({}))
                    .await
                    .unwrap_err();
                assert_eq!(err.code, "engine_unavailable");
            }
        }
    }

    mod parse {
        use super::*;

        mod on_unknown_method {
            use super::*;

            #[test]
            fn returns_unknown_method() {
                let err = Command::parse("frobnicate", user("a"), json!({})).unwrap_err();
                assert_eq!(err.code, "unknown_method");
            }
        }

        mod on_unknown_field {
            use super::*;

            #[test]
            fn returns_invalid_request() {
                let err = Command::parse("get_resource", user("a"), json!({"id": "x", "zap": 1}))
                    .unwrap_err();
                assert_eq!(err.code, "invalid_request");
            }
        }

        mod on_zero_duration {
            use super::*;

            #[test]
            fn returns_invalid_argument() {
                let err = Command::parse(
                    "acquire",
                    user("a"),
                    json!({"target": {"resource": "board"}, "duration_ms": 0, "purpose": "p"}),
                )
                .unwrap_err();
                assert_eq!(err.code, "invalid_argument");
            }
        }
    }

    mod timer {
        use super::*;

        mod on_deadline_without_commands {
            use super::*;

            /// Deterministic: the liveness probe is pushed an hour out and
            /// no command is sent after the clock moves, so the only thing
            /// that can wake the engine is its own deadline timer. The
            /// engine sleeps (real time) for the gap between the manual
            /// clock and the next deadline (400 ms here), then re-reads the
            /// clock, which by then is past the deadline.
            #[tokio::test]
            async fn wakes_and_publishes_the_due_notices() {
                let (h, clock) = spawn_with(no_liveness());
                seed_board(&h, &["u1"]).await;
                let a = user("a");
                // 500 ms: warning at 400 ms (20%), expiry at 500 ms; the clock
                // is moved between the two.
                acquire(&h, &a, 500).await;
                let drained = h
                    .wait_notices(a.holder.clone(), Duration::ZERO)
                    .await
                    .unwrap();
                assert_eq!(kinds(&drained), vec![]);
                let waiter = {
                    let h = h.clone();
                    let who = a.holder.clone();
                    tokio::spawn(async move { h.wait_notices(who, Duration::from_secs(30)).await })
                };
                until_parked(&h, &a.holder).await;
                clock.set(T0 + 450);
                let got = tokio::time::timeout(PATIENCE, waiter)
                    .await
                    .expect("deadline never woke the engine")
                    .unwrap()
                    .unwrap();
                assert_eq!(got[0].kind, NoticeKind::ExpiryWarning);
            }
        }
    }

    mod liveness {
        use super::*;

        mod on_dead_cli_holder {
            use super::*;

            #[tokio::test]
            async fn offers_its_unit_to_the_next_waiter() {
                let (h, _clock) = spawn_with(EngineOptions {
                    liveness_interval: Duration::from_millis(20),
                });
                seed_board(&h, &["u1"]).await;
                let mut child = std::process::Command::new("sleep")
                    .arg("30")
                    .spawn()
                    .unwrap();
                let pid = child.id();
                let start_time = leclerk::scheduler::liveness::start_time(pid).unwrap();
                let gone = Caller {
                    holder: HolderRef::CliProcess { pid, start_time },
                    project: None,
                    override_reason: None,
                };
                assert_eq!(acquire(&h, &gone, 60_000).await["outcome"], "granted");
                let b = user("b");
                assert_eq!(acquire(&h, &b, 60_000).await["outcome"], "queued");
                child.kill().unwrap();
                child.wait().unwrap();
                let got = h.wait_notices(b.holder.clone(), PATIENCE).await.unwrap();
                assert_eq!(kinds(&got), vec![NoticeKind::Offered]);
            }
        }
    }

    mod wait_notices {
        use super::*;

        /// `a` holds the only unit; `b` waits in the queue.
        async fn holder_and_waiter() -> (EngineHandle, Value) {
            let (h, _clock) = spawn();
            seed_board(&h, &["u1"]).await;
            let lease = acquire(&h, &user("a"), 60_000).await;
            assert_eq!(acquire(&h, &user("b"), 60_000).await["outcome"], "queued");
            // Drain whatever queueing told b.
            h.wait_notices(holder("b"), Duration::ZERO).await.unwrap();
            (h, lease)
        }

        mod on_offer_while_waiting {
            use super::*;

            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            async fn returns_within_100ms() {
                let (h, lease) = holder_and_waiter().await;
                let waiter = {
                    let h = h.clone();
                    tokio::spawn(async move {
                        let got = h.wait_notices(holder("b"), Duration::from_secs(30)).await;
                        (got, std::time::Instant::now())
                    })
                };
                until_parked(&h, &holder("b")).await;
                let id = lease["lease_id"].clone();
                ok(&h, "release", &user("a"), json!({"lease_id": id})).await;
                ok(&h, "confirm", &user("a"), json!({"lease_id": id})).await;
                let offered_at = std::time::Instant::now();
                let (got, returned_at) = tokio::time::timeout(PATIENCE, waiter)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(kinds(&got.unwrap()), vec![NoticeKind::Offered]);
                let latency = returned_at.saturating_duration_since(offered_at);
                assert!(latency < Duration::from_millis(100), "took {latency:?}");
            }
        }

        mod on_pending_notices {
            use super::*;

            #[tokio::test]
            async fn returns_them_at_once_and_marks_them_delivered() {
                let (h, lease) = holder_and_waiter().await;
                let id = lease["lease_id"].clone();
                ok(&h, "release", &user("a"), json!({"lease_id": id})).await;
                ok(&h, "confirm", &user("a"), json!({"lease_id": id})).await;
                let got = h
                    .wait_notices(holder("b"), Duration::from_secs(30))
                    .await
                    .unwrap();
                assert_eq!(kinds(&got), vec![NoticeKind::Offered]);
                let again = h.wait_notices(holder("b"), Duration::ZERO).await.unwrap();
                assert_eq!(kinds(&again), vec![]);
            }
        }

        mod on_nothing_to_deliver {
            use super::*;

            #[tokio::test]
            async fn returns_empty_after_the_timeout() {
                let (h, _lease) = holder_and_waiter().await;
                let started = std::time::Instant::now();
                let got = h
                    .wait_notices(holder("b"), Duration::from_millis(50))
                    .await
                    .unwrap();
                assert_eq!(kinds(&got), vec![]);
                assert!(started.elapsed() >= Duration::from_millis(50));
                assert!(!h.is_waiting(&holder("b")));
            }
        }

        mod on_notice_for_another_holder {
            use super::*;

            #[tokio::test]
            async fn keeps_waiting() {
                let (h, lease) = holder_and_waiter().await;
                let c = user("c");
                let waiter = {
                    let h = h.clone();
                    let who = c.holder.clone();
                    tokio::spawn(
                        async move { h.wait_notices(who, Duration::from_millis(300)).await },
                    )
                };
                until_parked(&h, &c.holder).await;
                let id = lease["lease_id"].clone();
                ok(&h, "release", &user("a"), json!({"lease_id": id})).await;
                ok(&h, "confirm", &user("a"), json!({"lease_id": id})).await;
                let got = waiter.await.unwrap().unwrap();
                assert_eq!(kinds(&got), vec![]);
            }
        }
    }

    mod daemon_stop {
        use super::*;

        mod on_parked_long_poll {
            use super::*;

            #[tokio::test]
            async fn returns_engine_unavailable_at_once() {
                let (h, _clock) = spawn();
                let waiter = {
                    let h = h.clone();
                    tokio::spawn(async move { h.wait_notices(holder("b"), MAX_WAIT).await })
                };
                until_parked(&h, &holder("b")).await;
                ok(&h, "daemon_stop", &user("admin"), json!({})).await;
                let got = tokio::time::timeout(PATIENCE, waiter)
                    .await
                    .expect("parked long-poll outlived the engine")
                    .unwrap();
                assert_eq!(got.unwrap_err().code, "engine_unavailable");
            }
        }

        mod on_call {
            use super::*;

            #[tokio::test]
            async fn replies_then_stops_the_engine() {
                let (h, _clock) = spawn();
                let out = ok(&h, "daemon_stop", &user("admin"), json!({})).await;
                assert_eq!(out, json!({"stopping": true}));
                tokio::time::timeout(PATIENCE, h.stopped()).await.unwrap();
            }
        }
    }
}

mod server {
    use super::*;

    async fn send(h: &EngineHandle, req: Request<Body>) -> (StatusCode, Value) {
        let resp = http::router(h.clone()).oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn post(h: &EngineHandle, method: &str, body: String) -> (StatusCode, Value) {
        let req = Request::post(format!("/v1/rpc/{method}"))
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        send(h, req).await
    }

    async fn call(
        h: &EngineHandle,
        method: &str,
        caller: &Caller,
        args: Value,
    ) -> (StatusCode, Value) {
        post(
            h,
            method,
            json!({"caller": caller, "args": args}).to_string(),
        )
        .await
    }

    /// The `result` of a call that must succeed.
    async fn result(h: &EngineHandle, method: &str, caller: &Caller, args: Value) -> Value {
        let (status, body) = call(h, method, caller, args).await;
        assert_eq!(status, StatusCode::OK, "{method}: {body}");
        body["result"].clone()
    }

    fn error_code(body: &Value) -> &str {
        body["error"]["code"].as_str().unwrap()
    }

    mod version {
        use super::*;

        #[tokio::test]
        async fn returns_the_crate_version() {
            let (h, _clock) = spawn();
            let req = Request::get("/v1/version").body(Body::empty()).unwrap();
            let (status, body) = send(&h, req).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(
                body,
                json!({"version": leclerk::VERSION, "pid": std::process::id()})
            );
        }
    }

    mod rpc {
        use super::*;

        mod on_unknown_method {
            use super::*;

            #[tokio::test]
            async fn returns_404_error_json() {
                let (h, _clock) = spawn();
                let (status, body) = call(&h, "frobnicate", &user("a"), json!({})).await;
                assert_eq!(status, StatusCode::NOT_FOUND);
                assert_eq!(error_code(&body), "unknown_method");
                assert!(
                    body["error"]["message"]
                        .as_str()
                        .unwrap()
                        .contains("frobnicate")
                );
            }
        }

        mod on_domain_error {
            use super::*;

            #[tokio::test]
            async fn maps_code_and_status() {
                let (h, _clock) = spawn();
                let (status, body) =
                    call(&h, "get_resource", &user("a"), json!({"id": "nope"})).await;
                assert_eq!(status, StatusCode::NOT_FOUND);
                assert_eq!(error_code(&body), "not_found");
                assert_ne!(body["error"]["message"].as_str().unwrap(), "");
            }

            #[tokio::test]
            async fn maps_a_conflict_to_409() {
                let (h, _clock) = spawn();
                seed_board(&h, &[]).await;
                let (status, body) = call(
                    &h,
                    "create_resource",
                    &user("a"),
                    json!({"id": "board", "type": "generic", "scope": "user"}),
                )
                .await;
                assert_eq!(status, StatusCode::CONFLICT);
                assert_eq!(error_code(&body), "already_exists");
            }

            #[tokio::test]
            async fn maps_invalid_settings_to_422() {
                let (h, _clock) = spawn();
                seed_board(&h, &[]).await;
                let (status, body) = call(
                    &h,
                    "add_unit",
                    &user("a"),
                    json!({"resource": "board", "id": "u1",
                           "settings": {"type": "generic", "identity_key": ""}}),
                )
                .await;
                assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
                assert_eq!(error_code(&body), "invalid_settings");
            }
        }

        mod on_malformed_json {
            use super::*;

            #[tokio::test]
            async fn returns_422_invalid_request() {
                let (h, _clock) = spawn();
                let (status, body) = post(&h, "list_resources", "{not json".into()).await;
                assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
                assert_eq!(error_code(&body), "invalid_request");
            }
        }

        mod on_invalid_args {
            use super::*;

            #[tokio::test]
            async fn returns_422_invalid_request() {
                let (h, _clock) = spawn();
                let (status, body) =
                    call(&h, "get_resource", &user("a"), json!({"id": "Not An Id"})).await;
                assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
                assert_eq!(error_code(&body), "invalid_request");
            }
        }

        mod inventory_round_trip {
            use super::*;

            #[tokio::test]
            async fn creates_reads_changes_and_deletes() {
                let (h, _clock) = spawn();
                let a = user("a");
                let created = result(
                    &h,
                    "create_resource",
                    &a,
                    json!({"id": "board", "type": "generic", "scope": "user", "description": "esp"}),
                )
                .await;
                assert_eq!(created["resource"]["id"], "board");
                assert_eq!(created["resource"]["type"], "generic");
                assert_eq!(created["warnings"], json!([]));
                let unit = result(
                    &h,
                    "add_unit",
                    &a,
                    json!({"resource": "board", "id": "u1",
                           "settings": {"type": "generic", "identity_key": "k1"}}),
                )
                .await;
                assert_eq!(unit["unit"]["id"], "u1");
                let got = result(&h, "get_resource", &a, json!({"id": "board"})).await;
                assert_eq!(got["description"], "esp");
                assert_eq!(got["units"][0]["settings"]["identity_key"], "k1");
                let list = result(&h, "list_resources", &a, json!({})).await;
                assert_eq!(list.as_array().unwrap().len(), 1);
                let all = result(&h, "list_resources", &a, json!({"view": "all"})).await;
                assert_eq!(all.as_array().unwrap().len(), 1);
                let updated = result(
                    &h,
                    "update_resource",
                    &a,
                    json!({"id": "board", "usage": "on desk", "health": null}),
                )
                .await;
                assert_eq!(updated["usage"], "on desk");
                assert_eq!(updated["health"], Value::Null);
                let unit = result(
                    &h,
                    "update_unit",
                    &a,
                    json!({"resource": "board", "unit": "u1", "description": "blue"}),
                )
                .await;
                assert_eq!(unit["unit"]["description"], "blue");
                result(
                    &h,
                    "add_unit",
                    &a,
                    json!({"resource": "board", "id": "u2",
                    "settings": {"type": "generic", "identity_key": "k2"}}),
                )
                .await;
                assert_eq!(
                    result(
                        &h,
                        "retire_unit",
                        &a,
                        json!({"resource": "board", "unit": "u2"})
                    )
                    .await,
                    Value::Null
                );
                result(
                    &h,
                    "remove_unit",
                    &a,
                    json!({"resource": "board", "unit": "u1"}),
                )
                .await;
                assert_eq!(
                    result(&h, "get_resource", &a, json!({"id": "board"})).await["units"],
                    json!([])
                );
                result(&h, "delete_resource", &a, json!({"id": "board"})).await;
                let (status, _) = call(&h, "get_resource", &a, json!({"id": "board"})).await;
                assert_eq!(status, StatusCode::NOT_FOUND);
            }
        }

        mod promotions_round_trip {
            use super::*;

            #[tokio::test]
            async fn lists_and_approves_a_pending_promotion() {
                let (h, _clock) = spawn();
                let b = in_project("b", "/p/b");
                let a = in_project("a", "/p/a");
                for (who, project, id) in [(&b, "/p/b", "theirs"), (&a, "/p/a", "mine")] {
                    result(
                        &h,
                        "create_resource",
                        who,
                        json!({"id": id, "type": "generic", "scope": {"project": project}}),
                    )
                    .await;
                }
                result(
                    &h,
                    "add_unit",
                    &b,
                    json!({"resource": "theirs", "id": "t1",
                    "settings": {"type": "generic", "identity_key": "same"}}),
                )
                .await;
                let added = result(
                    &h,
                    "add_unit",
                    &a,
                    json!({"resource": "mine", "id": "m1",
                    "settings": {"type": "generic", "identity_key": "same"}}),
                )
                .await;
                assert_eq!(added["warnings"][0]["kind"], "similar_in_other_project");
                let promos = result(&h, "list_promotions", &user("u"), json!({})).await;
                assert_eq!(promos[0]["resource"], "theirs");
                let promoted = result(
                    &h,
                    "approve_promotion",
                    &user("u"),
                    json!({"id": promos[0]["id"], "new_id": "shared"}),
                )
                .await;
                assert_eq!(promoted["id"], "shared");
                assert_eq!(promoted["scope"], "user");
                assert_eq!(
                    result(&h, "list_promotions", &user("u"), json!({})).await,
                    json!([])
                );
            }
        }

        mod lease_round_trip {
            use super::*;

            #[tokio::test]
            async fn runs_a_lease_from_grant_to_handover() {
                let (h, _clock) = spawn();
                seed_board(&h, &["u1"]).await;
                let (a, b, c) = (user("a"), user("b"), user("c"));
                let acquire = |who: &Caller| {
                    let who = who.clone();
                    let h = h.clone();
                    async move {
                        result(
                            &h,
                            "acquire",
                            &who,
                            json!({
                                "target": {"resource": "board", "unit": "u1"},
                                "duration_ms": 60_000, "priority": "normal", "purpose": "flash",
                            }),
                        )
                        .await
                    }
                };
                let grant = acquire(&a).await;
                assert_eq!(grant["outcome"], "granted");
                let lease = json!({"lease_id": grant["lease_id"]});
                let queued = acquire(&b).await;
                assert_eq!(queued["outcome"], "queued");
                assert_eq!(queued["position"], 1);
                assert!(queued.get("estimated_wait_ms").is_some());
                let cq = acquire(&c).await;
                result(&h, "cancel", &c, json!({"request_id": cq["request_id"]})).await;

                let view = result(
                    &h,
                    "set_interruptible",
                    &a,
                    json!({"lease_id": grant["lease_id"], "interruptible": true}),
                )
                .await;
                assert_eq!(view["interruptible"], true);
                assert_eq!(view["duration_ms"], 60_000);
                let ext = result(
                    &h,
                    "extend",
                    &a,
                    json!({"lease_id": grant["lease_id"], "ends_at": T0 + 120_000}),
                )
                .await;
                assert_eq!(ext["ends_at"], T0 + 120_000);
                let (status, body) = call(&h, "refuse_yield", &a, lease.clone()).await;
                assert_eq!(
                    (status, error_code(&body)),
                    (StatusCode::CONFLICT, "bad_state")
                );
                let (status, body) = call(&h, "reclaim", &a, lease.clone()).await;
                assert_eq!(
                    (status, error_code(&body)),
                    (StatusCode::CONFLICT, "bad_state")
                );

                let released = result(&h, "release", &a, lease.clone()).await;
                assert_eq!(released["state"], "cleaning");
                let more = result(
                    &h,
                    "request_cleanup_extension",
                    &a,
                    json!({"lease_id": grant["lease_id"], "reason": "flashing"}),
                )
                .await;
                assert_eq!(more["state"], "cleaning");
                result(
                    &h,
                    "confirm",
                    &a,
                    json!({"lease_id": grant["lease_id"], "how": "clean"}),
                )
                .await;

                let offer = h.wait_notices(b.holder.clone(), PATIENCE).await.unwrap();
                let offered = offer
                    .iter()
                    .find(|n| n.kind == NoticeKind::Offered)
                    .unwrap();
                let lease_b = offered.lease_id.clone().unwrap();
                let claimed = result(&h, "claim", &b, json!({"lease_id": lease_b})).await;
                assert_eq!(claimed["state"], "held");
                let (status, body) = call(&h, "claim", &a, json!({"lease_id": lease_b})).await;
                assert_eq!(
                    (status, error_code(&body)),
                    (StatusCode::CONFLICT, "not_holder")
                );
            }
        }

        mod status_round_trip {
            use super::*;

            #[tokio::test]
            async fn reports_units_and_holders() {
                let (h, _clock) = spawn();
                seed_board(&h, &["u1"]).await;
                acquire(&h, &user("a"), 60_000).await;
                let status = result(&h, "status", &user("b"), json!({"view": "caller"})).await;
                let unit = &status["resources"][0]["units"][0];
                assert_eq!(unit["id"], "u1");
                assert_eq!(unit["occupancy"]["kind"], "held");
                let resource = &status["resources"][0];
                assert_eq!(resource["type"], "generic");
                assert_eq!(resource.get("ty"), None);
                let all = result(&h, "status", &user("b"), json!({"view": "all"})).await;
                assert_eq!(all["resources"].as_array().unwrap().len(), 1);
            }
        }

        mod audit_round_trip {
            use super::*;

            #[tokio::test]
            async fn lists_entries_newest_first_with_filters() {
                let (h, _clock) = spawn();
                seed_board(&h, &["u1"]).await;
                let all = result(&h, "list_audit", &user("a"), json!({})).await;
                assert_eq!(all[0]["action"], "unit_added");
                assert_eq!(all[1]["action"], "resource_created");
                assert_eq!(all[1]["actor"], json!({"kind": "user", "tag": "admin"}));
                let only = result(
                    &h,
                    "list_audit",
                    &user("a"),
                    json!({"action": "resource_created", "limit": 5, "since": T0}),
                )
                .await;
                assert_eq!(only.as_array().unwrap().len(), 1);
            }
        }

        mod daemon_stop_round_trip {
            use super::*;

            #[tokio::test]
            async fn answers_and_stops() {
                let (h, _clock) = spawn();
                let out = result(&h, "daemon_stop", &user("admin"), json!({})).await;
                assert_eq!(out, json!({"stopping": true}));
                tokio::time::timeout(PATIENCE, h.stopped()).await.unwrap();
            }
        }
    }

    mod notices {
        use super::*;

        fn url(holder: &HolderRef, wait: u64) -> String {
            let json = serde_json::to_string(holder).unwrap();
            let encoded: String = json
                .bytes()
                .map(|b| match b {
                    b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                        (b as char).to_string()
                    }
                    _ => format!("%{b:02X}"),
                })
                .collect();
            format!("/v1/notices?holder={encoded}&wait={wait}")
        }

        mod on_pending_notices {
            use super::*;

            #[tokio::test]
            async fn returns_them_as_result() {
                let (h, _clock) = spawn();
                seed_board(&h, &["u1"]).await;
                let grant = acquire(&h, &user("a"), 60_000).await;
                acquire(&h, &user("b"), 60_000).await;
                h.wait_notices(holder("b"), Duration::ZERO).await.unwrap();
                let id = grant["lease_id"].clone();
                ok(&h, "release", &user("a"), json!({"lease_id": id})).await;
                ok(&h, "confirm", &user("a"), json!({"lease_id": id})).await;
                let req = Request::get(url(&holder("b"), 5))
                    .body(Body::empty())
                    .unwrap();
                let (status, body) = send(&h, req).await;
                assert_eq!(status, StatusCode::OK);
                assert_eq!(body["result"][0]["kind"], "offered");
            }
        }

        mod on_nothing_pending {
            use super::*;

            #[tokio::test]
            async fn returns_an_empty_list() {
                let (h, _clock) = spawn();
                let req = Request::get(url(&holder("b"), 0))
                    .body(Body::empty())
                    .unwrap();
                let (status, body) = send(&h, req).await;
                assert_eq!(status, StatusCode::OK);
                assert_eq!(body, json!({"result": []}));
            }
        }

        mod on_bad_holder {
            use super::*;

            #[tokio::test]
            async fn returns_422_invalid_request() {
                let (h, _clock) = spawn();
                let req = Request::get("/v1/notices?holder=nope&wait=0")
                    .body(Body::empty())
                    .unwrap();
                let (status, body) = send(&h, req).await;
                assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
                assert_eq!(error_code(&body), "invalid_request");
            }
        }
    }
}

mod api_error {
    use super::*;
    use leclerk::admin::AdminError;
    use leclerk::inventory::InventoryError;
    use leclerk::model::LeaseId;
    use leclerk::scheduler::SchedulerError;

    fn details(e: impl Into<ApiError>) -> Option<Value> {
        e.into().details
    }

    mod details {
        use super::*;

        #[test]
        fn extend_refused_carries_waiting_purpose() {
            let e = SchedulerError::ExtendRefused {
                waiting_purpose: Some("flash".into()),
            };
            assert_eq!(details(e), Some(json!({"waiting_purpose": "flash"})));
        }

        #[test]
        fn borrower_not_overrun_carries_due_at() {
            let e = SchedulerError::BorrowerNotOverrun { due_at: Some(42) };
            assert_eq!(details(e), Some(json!({"due_at": 42})));
        }

        #[test]
        fn link_conflict_carries_leases() {
            let (a, b) = (LeaseId::new(), LeaseId::new());
            let e = InventoryError::LinkConflict {
                units: vec!["r/a".into(), "r/b".into()],
                leases: vec![a.clone(), b.clone()],
            };
            assert_eq!(details(e), Some(json!({"leases": [a, b]})));
            let e = AdminError::LinkConflict {
                units: vec!["r/a".into(), "r/b".into()],
                leases: vec![a.clone()],
            };
            assert_eq!(details(e), Some(json!({"leases": [a]})));
        }

        #[test]
        fn ambiguous_duplicate_carries_units() {
            let e = InventoryError::AmbiguousDuplicate {
                units: vec!["r/a".into(), "s/b".into()],
            };
            assert_eq!(details(e), Some(json!({"units": ["r/a", "s/b"]})));
        }

        #[test]
        fn import_clash_carries_ids() {
            let e = AdminError::ImportClash {
                ids: vec!["board (user)".into()],
            };
            assert_eq!(details(e), Some(json!({"ids": ["board (user)"]})));
        }

        #[test]
        fn duplicate_in_scope_carries_resource_unit_and_descriptions() {
            let e = InventoryError::DuplicateInScope {
                resource: ResourceId::parse("board").unwrap(),
                unit: UnitId::parse("u1").unwrap(),
                resource_description: "boards".into(),
                unit_description: "left port".into(),
            };
            assert_eq!(
                details(e),
                Some(json!({"resource": "board", "unit": "u1",
                            "resource_description": "boards", "unit_description": "left port"}))
            );
        }

        #[test]
        fn not_directly_linked_carries_via() {
            let e = AdminError::NotDirectlyLinked {
                via: vec!["r/c".into()],
            };
            assert_eq!(details(e), Some(json!({"via": ["r/c"]})));
        }

        #[test]
        fn an_error_without_details_omits_the_field_on_the_wire() {
            let e = ApiError::from(SchedulerError::NotFound);
            assert_eq!(e.details, None);
            let wire = serde_json::to_value(&e).unwrap();
            assert_eq!(wire.get("details"), None, "{wire}");
        }

        #[tokio::test]
        async fn travels_inside_the_error_object_over_http() {
            let (h, _clock) = spawn();
            seed_board(&h, &["u1"]).await;
            let resp = http::router(h.clone())
                .oneshot(
                    Request::post("/v1/rpc/add_unit")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            json!({"caller": user("admin"), "args": {
                                "resource": "board", "id": "u2",
                                "settings": {"type": "generic", "identity_key": "key-u1"}}})
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::CONFLICT);
            let bytes = body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["error"]["code"], "duplicate_in_scope");
            assert_eq!(
                body["error"]["details"],
                json!({"resource": "board", "unit": "u1",
                       "resource_description": "", "unit_description": ""})
            );
        }
    }
}

mod lease_rpc {
    use super::*;

    async fn err(h: &EngineHandle, method: &str, who: &Caller, args: Value) -> ApiError {
        match rpc(h, method, who, args).await {
            Ok(v) => panic!("{method} unexpectedly succeeded: {v}"),
            Err(e) => e,
        }
    }

    async fn granted(h: &EngineHandle, who: &Caller) -> Value {
        let out = acquire(h, who, 600_000).await;
        assert_eq!(out["outcome"], "granted", "{out}");
        out["lease_id"].clone()
    }

    /// `a` held `board/u1` and handed it over; `b` has an open offer and
    /// `c` waits behind it. Returns b's offered lease and request ids.
    async fn offered_to_b(h: &EngineHandle) -> (Value, Value) {
        seed_board(h, &["u1"]).await;
        let lease = granted(h, &user("a")).await;
        let queued = acquire(h, &user("b"), 60_000).await;
        acquire(h, &user("c"), 60_000).await;
        ok(h, "release", &user("a"), json!({"lease_id": lease})).await;
        ok(h, "confirm", &user("a"), json!({"lease_id": lease})).await;
        let mine = ok(h, "list_my_leases", &user("b"), json!({})).await;
        assert_eq!(mine["leases"][0]["state"], "offered", "{mine}");
        (
            mine["leases"][0]["lease_id"].clone(),
            queued["request_id"].clone(),
        )
    }

    async fn offers(h: &EngineHandle, who: &Caller) -> usize {
        h.wait_notices(who.holder.clone(), Duration::ZERO)
            .await
            .unwrap()
            .iter()
            .filter(|n| n.kind == NoticeKind::Offered)
            .count()
    }

    mod release {
        use super::*;

        mod on_failed {
            use super::*;

            #[tokio::test]
            async fn ends_the_lease_at_once_with_command_failed() {
                let (h, _clock) = spawn();
                seed_board(&h, &["u1"]).await;
                let lease = granted(&h, &user("a")).await;
                let view = ok(
                    &h,
                    "release",
                    &user("a"),
                    json!({"lease_id": lease, "failed": true}),
                )
                .await;
                assert_eq!(view["state"], "ended");
                assert_eq!(view["end_cause"], "command_failed");
            }

            #[tokio::test]
            async fn marks_the_unit_dirty_command_failed() {
                let (h, _clock) = spawn();
                seed_board(&h, &["u1"]).await;
                let lease = granted(&h, &user("a")).await;
                ok(
                    &h,
                    "release",
                    &user("a"),
                    json!({"lease_id": lease, "failed": true}),
                )
                .await;
                let board = ok(&h, "get_resource", &user("a"), json!({"id": "board"})).await;
                assert_eq!(board["units"][0]["dirty"]["reason"], "command_failed");
            }
        }

        mod on_not_failed {
            use super::*;

            #[tokio::test]
            async fn starts_cleaning_as_before() {
                let (h, _clock) = spawn();
                seed_board(&h, &["u1"]).await;
                let lease = granted(&h, &user("a")).await;
                let view = ok(&h, "release", &user("a"), json!({"lease_id": lease})).await;
                assert_eq!(view["state"], "cleaning");
            }
        }

        mod on_an_offered_lease {
            use super::*;

            #[tokio::test]
            async fn ends_it_unclaimed() {
                let (h, _clock) = spawn();
                let (lease, _) = offered_to_b(&h).await;
                let view = ok(&h, "release", &user("b"), json!({"lease_id": lease})).await;
                assert_eq!(view["state"], "ended");
                assert_eq!(view["end_cause"], "unclaimed");
            }

            #[tokio::test]
            async fn offers_the_unit_to_the_next_waiter() {
                let (h, _clock) = spawn();
                let (lease, _) = offered_to_b(&h).await;
                ok(&h, "release", &user("b"), json!({"lease_id": lease})).await;
                assert_eq!(offers(&h, &user("c")).await, 1);
            }
        }
    }

    mod cancel {
        use super::*;

        mod on_a_request_already_offered {
            use super::*;

            #[tokio::test]
            async fn ends_the_offer_unclaimed_and_offers_the_next_waiter() {
                let (h, _clock) = spawn();
                let (lease, request) = offered_to_b(&h).await;
                ok(&h, "cancel", &user("b"), json!({"request_id": request})).await;
                let view = ok(&h, "get_lease", &user("b"), json!({"lease_id": lease})).await;
                assert_eq!(view["state"], "ended");
                assert_eq!(view["end_cause"], "unclaimed");
                assert_eq!(offers(&h, &user("c")).await, 1);
            }

            #[tokio::test]
            async fn refuses_someone_else() {
                let (h, _clock) = spawn();
                let (_, request) = offered_to_b(&h).await;
                let e = err(&h, "cancel", &user("c"), json!({"request_id": request})).await;
                assert_eq!(e.code, "not_holder");
            }
        }
    }

    mod get_lease {
        use super::*;

        mod on_the_holders_own_lease {
            use super::*;

            #[tokio::test]
            async fn returns_the_full_lease_view() {
                let (h, _clock) = spawn();
                seed_board(&h, &["u1"]).await;
                let lease = granted(&h, &user("a")).await;
                let view = ok(&h, "get_lease", &user("a"), json!({"lease_id": lease})).await;
                assert_eq!(view["lease_id"], lease);
                assert_eq!(view["state"], "held");
                assert_eq!(view["resource"], "board");
                assert_eq!(view["unit"], "u1");
                assert_eq!(view["settings"]["identity_key"], "key-u1");
                assert_eq!(view["ends_at"], T0 + 600_000);
                assert_eq!(view["priority"], "normal");
                assert_eq!(view["interruptible"], false);
                assert_eq!(view["purpose"], "test");
                for key in ["claim_deadline", "cleanup_deadline", "end_cause"] {
                    assert_eq!(view[key], Value::Null, "{key}");
                    assert!(view.get(key).is_some(), "{key} missing");
                }
            }
        }

        mod on_someone_elses_lease {
            use super::*;

            #[tokio::test]
            async fn returns_not_found() {
                let (h, _clock) = spawn();
                seed_board(&h, &["u1"]).await;
                let lease = granted(&h, &user("a")).await;
                let e = err(&h, "get_lease", &user("b"), json!({"lease_id": lease})).await;
                assert_eq!(e.code, "not_found");
            }
        }
    }

    mod list_my_leases {
        use super::*;

        #[tokio::test]
        async fn returns_live_leases_and_queued_requests_with_positions() {
            let (h, _clock) = spawn();
            seed_board(&h, &["u1"]).await;
            let lease = granted(&h, &user("a")).await;
            acquire(&h, &user("b"), 60_000).await;
            let queued = acquire(&h, &user("a"), 60_000).await;
            let mine = ok(&h, "list_my_leases", &user("a"), json!({})).await;
            let leases = mine["leases"].as_array().unwrap();
            assert_eq!(leases.len(), 1);
            assert_eq!(leases[0]["lease_id"], lease);
            let requests = mine["requests"].as_array().unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0]["request_id"], queued["request_id"]);
            assert_eq!(requests[0]["resource"], "board");
            assert_eq!(requests[0]["position"], 2);
        }

        #[tokio::test]
        async fn leaves_out_ended_leases_and_other_holders() {
            let (h, _clock) = spawn();
            seed_board(&h, &["u1", "u2"]).await;
            let lease = granted(&h, &user("a")).await;
            granted(&h, &user("b")).await;
            ok(
                &h,
                "release",
                &user("a"),
                json!({"lease_id": lease, "failed": true}),
            )
            .await;
            let mine = ok(&h, "list_my_leases", &user("a"), json!({})).await;
            assert_eq!(mine, json!({"leases": [], "requests": []}));
        }
    }
}

mod wire_text {
    use super::*;

    async fn board(h: &EngineHandle) -> Value {
        ok(
            h,
            "create_resource",
            &user("a"),
            json!({"id": "board", "type": "device", "scope": "user",
                   "description": "dev boards", "usage": "flash {unit.path}"}),
        )
        .await;
        ok(
            h,
            "add_unit",
            &user("a"),
            json!({"resource": "board", "id": "u1",
                   "settings": {"type": "device", "path": "/dev/ttyUSB0"},
                   "description": "left port"}),
        )
        .await;
        ok(h, "get_resource", &user("a"), json!({"id": "board"})).await
    }

    mod create_resource {
        use super::*;

        mod on_args_with_description_and_usage {
            use super::*;

            #[tokio::test]
            async fn round_trips_into_get_resource() {
                let (h, _clock) = spawn();
                let got = board(&h).await;
                assert_eq!(got["description"], "dev boards");
                assert_eq!(got["usage"], "flash {unit.path}");
            }
        }

        mod on_args_with_legacy_notes {
            use super::*;

            #[test]
            fn is_rejected_as_an_unknown_field() {
                let e = Command::parse(
                    "create_resource",
                    user("a"),
                    json!({"id": "b", "type": "generic", "scope": "user", "notes": "x"}),
                )
                .unwrap_err();
                assert_eq!(e.code, "invalid_request");
            }
        }

        mod on_an_unknown_placeholder {
            use super::*;

            #[tokio::test]
            async fn fails_with_unknown_placeholder_and_details() {
                let (h, _clock) = spawn();
                let e = rpc(
                    &h,
                    "create_resource",
                    &user("a"),
                    json!({"id": "b", "type": "device", "scope": "user",
                           "usage": "{unit.pth}"}),
                )
                .await
                .unwrap_err();
                assert_eq!(e.code, "unknown_placeholder");
                assert_eq!(e.details.unwrap()["placeholder"], "unit.pth");
            }
        }
    }

    mod update_resource {
        use super::*;

        mod on_args_with_description_and_usage {
            use super::*;

            #[tokio::test]
            async fn round_trips_into_get_resource() {
                let (h, _clock) = spawn();
                board(&h).await;
                ok(
                    &h,
                    "update_resource",
                    &user("a"),
                    json!({"id": "board", "description": "new", "usage": "use {unit.id}"}),
                )
                .await;
                let got = ok(&h, "get_resource", &user("a"), json!({"id": "board"})).await;
                assert_eq!(got["description"], "new");
                assert_eq!(got["usage"], "use {unit.id}");
            }
        }

        mod on_args_with_legacy_notes {
            use super::*;

            #[test]
            fn is_rejected_as_an_unknown_field() {
                let e = Command::parse(
                    "update_resource",
                    user("a"),
                    json!({"id": "board", "notes": "x"}),
                )
                .unwrap_err();
                assert_eq!(e.code, "invalid_request");
            }
        }
    }

    mod add_unit {
        use super::*;

        mod on_args_with_description {
            use super::*;

            #[tokio::test]
            async fn round_trips_into_get_resource() {
                let (h, _clock) = spawn();
                let got = board(&h).await;
                assert_eq!(got["units"][0]["description"], "left port");
            }
        }

        mod on_args_with_legacy_notes {
            use super::*;

            #[test]
            fn is_rejected_as_an_unknown_field() {
                let e = Command::parse(
                    "add_unit",
                    user("a"),
                    json!({"resource": "board", "id": "u1", "notes": "x",
                           "settings": {"type": "generic", "identity_key": "k"}}),
                )
                .unwrap_err();
                assert_eq!(e.code, "invalid_request");
            }
        }
    }

    mod update_unit {
        use super::*;

        mod on_args_with_description {
            use super::*;

            #[tokio::test]
            async fn round_trips_into_get_resource() {
                let (h, _clock) = spawn();
                board(&h).await;
                ok(
                    &h,
                    "update_unit",
                    &user("a"),
                    json!({"resource": "board", "unit": "u1", "description": "right port"}),
                )
                .await;
                let got = ok(&h, "get_resource", &user("a"), json!({"id": "board"})).await;
                assert_eq!(got["units"][0]["description"], "right port");
            }
        }

        mod on_args_with_legacy_notes {
            use super::*;

            #[test]
            fn is_rejected_as_an_unknown_field() {
                let e = Command::parse(
                    "update_unit",
                    user("a"),
                    json!({"resource": "board", "unit": "u1", "notes": "x"}),
                )
                .unwrap_err();
                assert_eq!(e.code, "invalid_request");
            }
        }
    }

    mod resource_and_unit_views {
        use super::*;

        mod on_json {
            use super::*;

            #[tokio::test]
            async fn expose_description_and_usage_and_no_notes() {
                let (h, _clock) = spawn();
                let got = board(&h).await;
                assert!(got.get("notes").is_none(), "{got}");
                assert!(got["units"][0].get("notes").is_none(), "{got}");
                assert!(got.get("usage").is_some());
                assert!(got["units"][0].get("description").is_some());
            }
        }
    }

    mod list_resources {
        use super::*;

        mod on_a_resource_with_text {
            use super::*;

            #[tokio::test]
            async fn includes_descriptions_and_usage() {
                let (h, _clock) = spawn();
                board(&h).await;
                let list = ok(&h, "list_resources", &user("a"), json!({})).await;
                assert_eq!(list[0]["description"], "dev boards");
                assert_eq!(list[0]["usage"], "flash {unit.path}");
                assert_eq!(list[0]["units"][0]["description"], "left port");
            }
        }
    }

    mod list_promotions {
        use super::*;

        mod on_a_pending_promotion {
            use super::*;

            #[tokio::test]
            async fn exposes_the_resources_description() {
                let (h, _clock) = spawn();
                let b = in_project("b", "/p/b");
                let a = in_project("a", "/p/a");
                for (who, project, id, text) in [
                    (&b, "/p/b", "theirs", "their board"),
                    (&a, "/p/a", "mine", "my board"),
                ] {
                    ok(
                        &h,
                        "create_resource",
                        who,
                        json!({"id": id, "type": "generic", "scope": {"project": project},
                               "description": text}),
                    )
                    .await;
                    ok(
                        &h,
                        "add_unit",
                        who,
                        json!({"resource": id, "id": "u1",
                               "settings": {"type": "generic", "identity_key": "same"}}),
                    )
                    .await;
                }
                let promos = ok(&h, "list_promotions", &user("u"), json!({})).await;
                assert_eq!(promos[0]["description"], "their board");
            }
        }
    }
}

mod brief {
    use super::*;

    mod acquire {
        use super::*;

        mod on_a_device_resource_with_a_usage_template {
            use super::*;

            #[tokio::test]
            async fn returns_the_rendered_usage() {
                let (h, _clock) = spawn();
                let admin = user("admin");
                ok(
                    &h,
                    "create_resource",
                    &admin,
                    json!({
                        "id": "rig", "type": "device", "scope": "user",
                        "description": "ESP32 rig",
                        "usage": "flash: idf.py -p {unit.path} flash; env ${PORT}",
                    }),
                )
                .await;
                ok(
                    &h,
                    "add_unit",
                    &admin,
                    json!({
                        "resource": "rig", "id": "u1", "description": "left board",
                        "settings": {"type": "device", "path": "/dev/ttyU1"},
                    }),
                )
                .await;
                let out = ok(
                    &h,
                    "acquire",
                    &user("a"),
                    json!({"target": {"resource": "rig"}, "duration_ms": 60_000, "purpose": "t"}),
                )
                .await;
                assert_eq!(
                    out["usage"],
                    "flash: idf.py -p /dev/ttyU1 flash; env ${PORT}"
                );
                assert_eq!(out["resource_description"], "ESP32 rig");
                assert_eq!(out["unit_description"], "left board");
            }
        }
    }
}
