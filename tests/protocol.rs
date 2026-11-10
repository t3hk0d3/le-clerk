//! BDD for the protocol's tolerance: clients read answers from a newer
//! daemon, the store refuses binaries older than its data, and order and
//! liveness do not trust the wall clock or a recycled pid.

mod common;

use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path as RoutePath, State};
use axum::routing::{get, post};
use common::TestEnv;
use leclerk::admin::ExportUnit;
use leclerk::client::{Client, wire};
use leclerk::clock::ManualClock;
use leclerk::config::{Config, Paths};
use leclerk::engine::Command as EngineCommand;
use leclerk::error::ErrorCode;
use leclerk::health::HealthResult;
use leclerk::inventory::{Inventory, NewResource, NewUnit};
use leclerk::model::{
    Caller, HealthSpec, HolderRef, Host, LeaseId, NoticeId, Priority, RequestId, ResourceId,
    ResourceType, Scope, UnitId, UnitSettings, UnitSettingsInput,
};
use leclerk::scheduler::session::Sessions;
use leclerk::scheduler::{
    AcquireOutcome, AcquireRequest, HealthProbe, Scheduler, Target, liveness,
};
use leclerk::store::notices::{self, Notice, NoticeKind};
use leclerk::store::{Store, StoreError, Tx, leases, migrations, sessions};
use rusqlite::Connection;
use serde_json::{Value, json};
use tempfile::TempDir;
use tracing_subscriber::fmt::MakeWriter;

const T0: i64 = 1_000_000;
const MIN: Duration = Duration::from_secs(60);
/// The last migration the installed daemon has applied.
const INSTALLED_MIGRATIONS: usize = 17;

// ---------------------------------------------------------------------------
// A newer daemon, played on the wire
// ---------------------------------------------------------------------------

/// What the fake daemon answers.
#[derive(Clone)]
struct Answers {
    /// Each RPC's `result`, by method.
    rpc: Arc<Value>,
    /// The batches `/v1/notices` hands out, then empty ones.
    batches: Arc<Mutex<Vec<Value>>>,
    /// The RPC methods called, in order.
    called: Arc<Mutex<Vec<String>>>,
}

async fn answer_rpc(
    State(a): State<Answers>,
    RoutePath(method): RoutePath<String>,
) -> axum::Json<Value> {
    a.called.lock().unwrap().push(method.clone());
    axum::Json(json!({ "result": a.rpc[&method] }))
}

async fn answer_notices(State(a): State<Answers>) -> axum::Json<Value> {
    let next = {
        let mut batches = a.batches.lock().unwrap();
        (!batches.is_empty()).then(|| batches.remove(0))
    };
    if next.is_none() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    axum::Json(json!({ "result": next.unwrap_or_else(|| json!([])) }))
}

/// A daemon speaking this protocol, in `env`'s home, answering from `rpc`
/// and `batches`.
struct NewerDaemon {
    env: TestEnv,
    paths: Paths,
    called: Arc<Mutex<Vec<String>>>,
    runtime: tokio::runtime::Runtime,
}

impl NewerDaemon {
    fn new(rpc: Value, batches: Vec<Value>) -> Self {
        let env = TestEnv::new();
        let home = env.home().to_path_buf();
        let paths =
            Paths::from_lookup(move |k| (k == "CLERK_HOME").then(|| home.clone().into_os_string()))
                .unwrap();
        fs::create_dir_all(paths.socket.parent().unwrap()).unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let called = Arc::new(Mutex::new(Vec::new()));
        let answers = Answers {
            rpc: Arc::new(rpc),
            batches: Arc::new(Mutex::new(batches)),
            called: called.clone(),
        };
        let socket = paths.socket.clone();
        runtime.block_on(async move {
            let listener = tokio::net::UnixListener::bind(socket).unwrap();
            let app = Router::new()
                .route(
                    "/v1/version",
                    get(|| async {
                        axum::Json(json!({"version": leclerk::VERSION,
                                          "protocol": leclerk::PROTOCOL, "pid": 1}))
                    }),
                )
                .route("/v1/rpc/{method}", post(answer_rpc))
                .route("/v1/notices", get(answer_notices))
                .with_state(answers);
            tokio::spawn(async move { axum::serve(listener, app).await });
        });
        Self {
            env,
            paths,
            called,
            runtime,
        }
    }

    fn called(&self, method: &str) -> bool {
        self.called.lock().unwrap().iter().any(|m| m == method)
    }
}

/// A notice as a newer daemon words it.
fn wire_notice(kind: &str, action: &str, extra: &Value) -> Value {
    let mut n = json!({
        "id": NoticeId::new(),
        "holder": {"kind": "from_the_future", "where": "elsewhere"},
        "kind": kind,
        "lease_id": null, "request_id": null, "resource": null, "unit": null,
        "other_purpose": null, "deadline": null, "position": null,
        "dirty": {"reason": "from_the_future", "at": 1,
                  "previous_holder": {"kind": "from_the_future"}},
        "action": action,
        "created_at": 1,
        "a_field_from_the_future": true,
    });
    n.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    n
}

/// A held lease as a newer daemon words it, with `settings`.
fn wire_lease(lease: &LeaseId, settings: &Value) -> Value {
    json!({
        "outcome": "granted", "state": "held", "lease_id": lease,
        "resource": "board", "unit": "u1", "settings": settings,
        "resource_description": "", "unit_description": "", "usage": "",
        "resource_tags": [], "ends_at": T0, "dirty": null,
    })
}

mod client_notice_parsing {
    use super::*;

    /// `clerk acquire --wait`, queued, then told of a kind it does not know
    /// in the batch that carries its offer.
    fn acquire_through_a_future_notice() -> (NewerDaemon, std::process::Output) {
        let (request, lease) = (RequestId::new(), LeaseId::new());
        let held = wire_lease(&lease, &json!({"type": "generic", "identity_key": "k"}));
        let daemon = NewerDaemon::new(
            json!({
                "acquire": {"outcome": "queued", "request_id": request, "position": 1,
                            "estimated_wait_ms": null, "blocked_by": null},
                "claim": held,
                "rehome_holder": held,
            }),
            vec![json!([
                wire_notice("from_the_future", "Mind the flux capacitor.", &json!({})),
                wire_notice(
                    "offered",
                    "Claim it.",
                    &json!({"lease_id": lease, "request_id": request})
                ),
            ])],
        );
        let out = daemon
            .env
            .cmd()
            .args(["acquire", "board", "--for", "1m", "--wait"])
            .timeout(common::PATIENCE)
            .output()
            .unwrap();
        (daemon, out)
    }

    mod on_a_notice_whose_kind_the_client_does_not_know {
        use super::*;

        #[test]
        fn parses_it_as_unknown_and_prints_it_with_its_action_text() {
            let (_daemon, out) = acquire_through_a_future_notice();
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                stderr.contains("[unknown] Mind the flux capacitor."),
                "{out:?}"
            );
        }

        #[test]
        fn does_not_lose_the_other_notices_of_the_batch() {
            let (daemon, out) = acquire_through_a_future_notice();
            assert!(out.status.success(), "{out:?}");
            assert!(daemon.called("claim"));
        }
    }

    mod on_a_notice_that_cannot_be_read_at_all {
        use super::*;

        #[test]
        fn pauses_when_nothing_in_the_batch_can_be_read() {
            let daemon = NewerDaemon::new(json!({}), vec![json!([{"kind": "offered"}])]);
            let paths = daemon.paths.clone();
            let took = daemon.runtime.block_on(async move {
                let client = Client::connect(&paths).await.unwrap();
                let who = Caller {
                    holder: HolderRef::User { tag: "me".into() },
                    project: None,
                    override_reason: None,
                };
                let started = std::time::Instant::now();
                client.wait_notices(&who, 0).await.unwrap();
                started.elapsed()
            });
            assert!(took >= Duration::from_millis(500), "took {took:?}");
        }

        #[test]
        fn keeps_the_rest_of_the_batch() {
            let daemon = NewerDaemon::new(
                json!({}),
                vec![json!([
                    {"kind": "offered"},
                    wire_notice("offered", "Claim it.", &json!({})),
                ])],
            );
            let paths = daemon.paths.clone();
            let got = daemon.runtime.block_on(async move {
                let client = Client::connect(&paths).await.unwrap();
                let who = Caller {
                    holder: HolderRef::User { tag: "me".into() },
                    project: None,
                    override_reason: None,
                };
                client.wait_notices(&who, 0).await.unwrap()
            });
            assert_eq!(
                got.iter().map(|n| n.action.as_str()).collect::<Vec<_>>(),
                ["Claim it."]
            );
        }
    }

    /// `clerk run` granted `settings`, whose command records that it ran.
    fn run_with_settings(settings: &Value) -> (TempDir, std::process::Output) {
        let lease = LeaseId::new();
        let held = wire_lease(&lease, settings);
        let daemon = NewerDaemon::new(
            json!({
                "acquire": held,
                "get_lease": {"state": "ended", "lease_id": lease},
            }),
            Vec::new(),
        );
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("ran");
        let script = format!("echo \"$CLERK_UNIT_IDENTITY_KEY\" > {}", marker.display());
        let out = daemon
            .env
            .cmd()
            .args(["run", "board", "--for", "1m", "--", "sh", "-c", &script])
            .timeout(common::PATIENCE)
            .output()
            .unwrap();
        (dir, out)
    }

    mod on_a_grant_with_a_settings_field_the_client_does_not_know {
        use super::*;

        #[test]
        fn clerk_run_starts_the_command() {
            let (dir, out) =
                run_with_settings(&json!({"type": "generic", "identity_key": "k", "baud": 9600}));
            let ran = fs::read_to_string(dir.path().join("ran"));
            assert_eq!(ran.ok().as_deref(), Some("k\n"), "{out:?}");
        }
    }

    mod on_a_grant_with_a_settings_type_the_client_does_not_know {
        use super::*;

        #[test]
        fn clerk_run_starts_the_command() {
            let (dir, out) = run_with_settings(&json!({"type": "quantum", "qubits": 3}));
            assert!(dir.path().join("ran").exists(), "{out:?}");
        }
    }
}

mod client_notice_kinds {
    use super::*;

    #[test]
    fn know_every_kind_the_store_can_write() {
        for kind in [
            NoticeKind::Offered,
            NoticeKind::QueueDropped,
            NoticeKind::PositionChanged,
            NoticeKind::ExpiryWarning,
            NoticeKind::CleanupWindowStarted,
            NoticeKind::CleanupEnding,
            NoticeKind::YieldRequested,
            NoticeKind::YieldWithdrawn,
            NoticeKind::UnitReturned,
            NoticeKind::BorrowerOverran,
            NoticeKind::Overridden,
            NoticeKind::LeaseEnded,
            NoticeKind::ResourceRemoved,
        ] {
            let text = serde_json::to_value(kind).unwrap();
            let read: wire::NoticeKind = serde_json::from_value(text.clone()).unwrap();
            assert_eq!(serde_json::to_value(read).unwrap(), text);
        }
    }
}

mod unit_settings {
    use super::*;

    const MISSPELT: &str = r#"{"type": "generic", "identity_key": "k", "bauud": 9600}"#;

    mod as_an_answer {
        use super::*;

        #[test]
        fn tolerates_an_unknown_field() {
            let read: UnitSettings = serde_json::from_str(MISSPELT).unwrap();
            assert_eq!(
                read,
                UnitSettings::Generic {
                    identity_key: "k".into()
                }
            );
        }
    }

    mod as_a_request {
        use super::*;

        #[test]
        fn refuses_an_unknown_field() {
            assert!(serde_json::from_str::<UnitSettingsInput>(MISSPELT).is_err());
        }

        #[test]
        fn add_unit_refuses_an_unknown_field() {
            let args = json!({
                "resource": "board", "id": "u9",
                "settings": serde_json::from_str::<Value>(MISSPELT).unwrap(),
            });
            assert!(EngineCommand::parse("add_unit", caller("a"), args).is_err());
        }

        #[test]
        fn an_import_refuses_an_unknown_field() {
            let unit = json!({
                "id": "u9",
                "settings": serde_json::from_str::<Value>(MISSPELT).unwrap(),
            });
            assert!(serde_json::from_value::<ExportUnit>(unit).is_err());
        }
    }
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

/// A file database migrated as far as `migrations`, in a fresh directory.
fn database_at(migrations_applied: usize) -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("clerk.db");
    let mut conn = Connection::open(&path).unwrap();
    migrations()
        .to_version(&mut conn, migrations_applied)
        .unwrap();
    (dir, path)
}

/// Record `version` as `path`'s vocabulary, as a newer binary would.
fn stamp_vocabulary(path: &Path, version: i64) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS store_vocabulary (\
             id INTEGER PRIMARY KEY CHECK (id = 1), \
             vocabulary_version INTEGER NOT NULL);",
    )
    .unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO store_vocabulary (id, vocabulary_version) VALUES (1, ?1)",
        [version],
    )
    .unwrap();
}

fn user_version(path: &Path) -> usize {
    Connection::open(path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap()
}

fn vocabulary(path: &Path) -> i64 {
    Connection::open(path)
        .unwrap()
        .query_row(
            "SELECT vocabulary_version FROM store_vocabulary WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

mod store {
    use super::*;

    mod open {
        use super::*;

        mod on_a_db_whose_vocabulary_version_is_newer_than_the_binarys {
            use super::*;

            #[test]
            fn refuses_with_store_too_new() {
                let (_dir, path) = database_at(INSTALLED_MIGRATIONS);
                stamp_vocabulary(&path, 999);
                let refused = Store::open(&path).unwrap_err();
                assert_eq!(refused.code(), "store_too_new");
            }

            #[test]
            fn leaves_the_schema_unmigrated() {
                let (_dir, path) = database_at(INSTALLED_MIGRATIONS);
                stamp_vocabulary(&path, 999);
                let _ = Store::open(&path);
                assert_eq!(user_version(&path), INSTALLED_MIGRATIONS);
            }

            #[test]
            fn the_daemon_exits_5() {
                let env = TestEnv::new();
                let home = env.home().to_path_buf();
                let paths = Paths::from_lookup(move |k| {
                    (k == "CLERK_HOME").then(|| home.clone().into_os_string())
                })
                .unwrap();
                fs::create_dir_all(paths.db.parent().unwrap()).unwrap();
                let mut conn = Connection::open(&paths.db).unwrap();
                migrations()
                    .to_version(&mut conn, INSTALLED_MIGRATIONS)
                    .unwrap();
                drop(conn);
                stamp_vocabulary(&paths.db, 999);
                let out = env
                    .cmd()
                    .args(["--json", "daemon"])
                    .timeout(common::PATIENCE)
                    .output()
                    .unwrap();
                assert_eq!(out.status.code(), Some(5), "{out:?}");
                let answer: Value = serde_json::from_slice(&out.stdout).unwrap();
                assert_eq!(answer["error"]["code"], "store_too_new");
            }
        }

        mod on_the_installed_daemons_db {
            use super::*;

            /// A database through 0017 with one host in it, opened now.
            fn upgraded() -> (TempDir, std::path::PathBuf) {
                let (dir, path) = database_at(INSTALLED_MIGRATIONS);
                Connection::open(&path)
                    .unwrap()
                    .execute(
                        "INSERT INTO agent_hosts (session_id, pid, start_time, last_seen) \
                         VALUES ('s1', 42, 7, 1)",
                        [],
                    )
                    .unwrap();
                Store::open(&path).unwrap();
                (dir, path)
            }

            #[test]
            fn applies_the_vocabulary_migration() {
                let (_dir, path) = upgraded();
                assert!(user_version(&path) > INSTALLED_MIGRATIONS);
            }

            #[test]
            fn starts_the_vocabulary_at_1() {
                let (_dir, path) = upgraded();
                assert_eq!(vocabulary(&path), 1);
            }

            #[test]
            fn keeps_its_hosts_without_a_boot_id() {
                let (_dir, path) = upgraded();
                let boot: Option<String> = Connection::open(&path)
                    .unwrap()
                    .query_row(
                        "SELECT boot_id FROM agent_hosts WHERE session_id = 's1'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(boot, None);
            }
        }

        mod on_a_db_without_the_vocabulary_row {
            use super::*;

            fn without_row() -> (TempDir, std::path::PathBuf) {
                let (dir, path) = database_at(INSTALLED_MIGRATIONS);
                Store::open(&path).unwrap();
                Connection::open(&path)
                    .unwrap()
                    .execute("DELETE FROM store_vocabulary", [])
                    .unwrap();
                (dir, path)
            }

            #[test]
            fn opens_it() {
                let (_dir, path) = without_row();
                assert!(Store::open(&path).is_ok());
            }

            #[test]
            fn records_the_current_vocabulary() {
                let (_dir, path) = without_row();
                Store::open(&path).unwrap();
                assert_eq!(vocabulary(&path), 1);
            }
        }
    }

    mod live_leases {
        use super::*;

        mod on_one_row_with_an_unknown_enum_value {
            use super::*;

            #[test]
            fn returns_the_other_rows_and_logs_the_bad_one() {
                let mut f = fixture();
                let good = f.grant(&caller("a"), "u1");
                f.insert_lease("u2", "from_the_future");
                let (rows, log) = logged(|| f.with_tx(|tx| leases::live_leases(tx).unwrap()));
                assert_eq!(rows.iter().map(|l| &l.id).collect::<Vec<_>>(), [&good]);
                assert!(log.contains("leases"), "{log}");
            }

            #[test]
            fn logs_it_once_per_store() {
                let mut f = fixture();
                f.insert_lease("u2", "from_the_future");
                f.with_tx(|tx| leases::live_leases(tx).unwrap());
                let (_, log) = logged(|| f.with_tx(|tx| leases::live_leases(tx).unwrap()));
                assert_eq!(log, "");
            }

            #[test]
            fn its_unit_is_not_granted() {
                let mut f = fixture();
                f.insert_lease("u1", "from_the_future");
                f.insert_lease("u2", "from_the_future");
                let who = caller("a");
                let asked = f
                    .store
                    .tx(|tx| {
                        f.sched
                            .acquire(tx, &who, req(Target::Resource(rid("board"))))
                    })
                    .unwrap();
                assert!(matches!(asked, AcquireOutcome::Queued { .. }), "{asked:?}");
            }
        }
    }

    mod queued_requests {
        use super::*;

        mod on_one_row_with_an_unknown_enum_value {
            use super::*;

            #[test]
            fn returns_the_other_rows() {
                let mut f = fixture();
                f.insert_request("good", "normal", T0);
                f.insert_request("bad", "from_the_future", T0);
                let rows = f.with_tx(|tx| leases::queued_requests(tx).unwrap());
                assert_eq!(rows.len(), 1);
            }
        }
    }

    mod notices_take {
        use super::*;

        fn to(tag: &str) -> HolderRef {
            HolderRef::User { tag: tag.into() }
        }

        fn insert_future_notice(f: &mut Fixture, who: &HolderRef) -> String {
            let id = NoticeId::new();
            let payload = wire_notice("from_the_future", "?", &json!({})).to_string();
            f.with_tx(|tx| {
                tx.conn()
                    .execute(
                        "INSERT INTO notices (notice_id, holder, kind, payload, created_at) \
                         VALUES (?1, ?2, 'from_the_future', ?3, 1)",
                        rusqlite::params![
                            id.as_str(),
                            serde_json::to_string(who).unwrap(),
                            payload
                        ],
                    )
                    .unwrap();
            });
            id.as_str().to_owned()
        }

        mod on_undecodable_rows_ahead_of_good_ones {
            use super::*;

            fn with_bad_first() -> (Fixture, HolderRef, String, Notice) {
                let mut f = fixture();
                let who = to("h");
                let bad = insert_future_notice(&mut f, &who);
                let good = Notice::new(who.clone(), NoticeKind::Offered, "claim".into(), 2);
                f.with_tx(|tx| notices::publish(tx, &good).unwrap());
                (f, who, bad, good)
            }

            #[test]
            fn takes_the_good_one_within_the_limit() {
                let (mut f, who, _bad, good) = with_bad_first();
                let got = f.with_tx(|tx| notices::take(tx, &who, 1, T0).unwrap());
                assert_eq!(got, vec![good]);
            }

            #[test]
            fn leaves_the_bad_one_undelivered() {
                let (mut f, who, bad, _good) = with_bad_first();
                f.with_tx(|tx| notices::take(tx, &who, 1, T0).unwrap());
                let delivered: Option<i64> = f.with_tx(|tx| {
                    tx.conn()
                        .query_row(
                            "SELECT delivered_at FROM notices WHERE notice_id = ?1",
                            [&bad],
                            |r| r.get(0),
                        )
                        .unwrap()
                });
                assert_eq!(delivered, None);
            }

            #[test]
            fn pending_returns_the_good_one() {
                let (mut f, who, _bad, good) = with_bad_first();
                let got = f.with_tx(|tx| notices::pending(tx, &who).unwrap());
                assert_eq!(got, vec![good]);
            }
        }

        mod on_two_notices_whose_created_at_disagrees_with_insert_order {
            use super::*;

            #[test]
            fn returns_them_in_insert_order() {
                let mut f = fixture();
                let who = to("h");
                let first = Notice::new(who.clone(), NoticeKind::Offered, "1st".into(), T0 + 10);
                let second = Notice::new(who.clone(), NoticeKind::LeaseEnded, "2nd".into(), T0);
                f.with_tx(|tx| {
                    notices::publish(tx, &first).unwrap();
                    notices::publish(tx, &second).unwrap();
                });
                let got = f.with_tx(|tx| notices::take(tx, &who, 10, T0).unwrap());
                assert_eq!(got, vec![first, second]);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Dispatch and liveness
// ---------------------------------------------------------------------------

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

fn rid(s: &str) -> ResourceId {
    ResourceId::parse(s).unwrap()
}

fn uid(s: &str) -> UnitId {
    UnitId::parse(s).unwrap()
}

fn as_caller(holder: HolderRef) -> Caller {
    Caller {
        holder,
        project: None,
        override_reason: None,
    }
}

fn caller(tag: &str) -> Caller {
    as_caller(HolderRef::User { tag: tag.into() })
}

fn req(target: Target) -> AcquireRequest {
    AcquireRequest {
        target,
        duration: 60 * MIN,
        interruptible: false,
        priority: Priority::Normal,
        purpose: "test".into(),
        override_reason: None,
        skip_checks: false,
    }
}

struct Fixture {
    store: Store,
    sched: Scheduler,
}

/// A generic resource `board` with units `u1` and `u2`.
fn fixture() -> Fixture {
    let clock = Arc::new(ManualClock::new(T0));
    let mut store = Store::open_in_memory().unwrap();
    let inv = Inventory::new(clock.clone());
    let who = caller("admin");
    store
        .tx(|tx| -> Result<(), leclerk::inventory::InventoryError> {
            inv.create_resource(
                tx,
                &who,
                &NewResource {
                    id: rid("board"),
                    ty: ResourceType::Generic,
                    scope: Scope::User,
                    description: String::new(),
                    usage: String::new(),
                    health: None,
                    tags: Vec::new(),
                },
            )?;
            for u in ["u1", "u2"] {
                inv.add_unit(
                    tx,
                    &who,
                    &rid("board"),
                    &NewUnit {
                        id: uid(u),
                        settings: UnitSettings::Generic {
                            identity_key: format!("board-{u}"),
                        },
                        description: String::new(),
                    },
                )?;
            }
            Ok(())
        })
        .unwrap();
    Fixture {
        store,
        sched: Scheduler::new(clock, Config::default(), Arc::new(Healthy)),
    }
}

impl Fixture {
    fn with_tx<T>(&mut self, f: impl FnOnce(&Tx) -> T) -> T {
        self.store
            .tx(|tx| -> Result<T, StoreError> { Ok(f(tx)) })
            .unwrap()
    }

    fn grant(&mut self, who: &Caller, unit: &str) -> LeaseId {
        let target = Target::Unit(rid("board"), uid(unit));
        let asked = self
            .store
            .tx(|tx| self.sched.acquire(tx, who, req(target)))
            .unwrap();
        match asked {
            AcquireOutcome::Granted(g) => g.lease_id,
            other => panic!("expected a grant, got {other:?}"),
        }
    }

    /// A lease row on `unit` with `state` as stored text.
    fn insert_lease(&mut self, unit: &str, state: &str) {
        self.with_tx(|tx| {
            tx.conn()
                .execute(
                    "INSERT INTO leases (lease_id, unit_row, resource_row, state, holder, \
                     priority, interruptible, purpose, duration_ms, created_at) \
                     SELECT ?1, row_id, resource_row, ?2, '{\"kind\":\"user\",\"tag\":\"x\"}', \
                     'normal', 0, 'test', 60000, 0 FROM units WHERE id = ?3",
                    rusqlite::params![LeaseId::new().as_str(), state, unit],
                )
                .unwrap();
        });
    }

    /// A queued request of user `tag` for `board`.
    fn insert_request(&mut self, tag: &str, priority: &str, created_at: i64) {
        let holder = serde_json::to_string(&HolderRef::User { tag: tag.into() }).unwrap();
        self.with_tx(|tx| {
            tx.conn()
                .execute(
                    "INSERT INTO requests (request_id, resource_row, state, holder, priority, \
                     interruptible, purpose, duration_ms, created_at) \
                     SELECT ?1, row_id, 'queued', ?2, ?3, 0, 'test', 60000, ?4 FROM resources \
                     WHERE id = 'board'",
                    rusqlite::params![RequestId::new().as_str(), holder, priority, created_at],
                )
                .unwrap();
        });
    }

    fn offers_to(&mut self, tag: &str) -> usize {
        let who = HolderRef::User { tag: tag.into() };
        self.with_tx(|tx| notices::pending(tx, &who).unwrap())
            .iter()
            .filter(|n| n.kind == NoticeKind::Offered)
            .count()
    }
}

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl MakeWriter<'_> for Captured {
    type Writer = Self;

    fn make_writer(&self) -> Self {
        self.clone()
    }
}

/// `f`'s value and what it logged on this thread.
fn logged<T>(f: impl FnOnce() -> T) -> (T, String) {
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_max_level(tracing::Level::WARN)
        .finish();
    let value = tracing::subscriber::with_default(subscriber, f);
    let log = String::from_utf8_lossy(&captured.0.lock().unwrap()).into_owned();
    (value, log)
}

mod dispatch_order {
    use super::*;

    mod on_two_requests_whose_created_at_order_disagrees_with_insert_order {
        use super::*;

        /// `first` is inserted first but stamped later, as after a clock
        /// step backward; one unit is free for them.
        fn dispatched() -> Fixture {
            let mut f = fixture();
            f.grant(&caller("holder"), "u1");
            f.insert_request("first", "normal", T0 + 10);
            f.insert_request("second", "normal", T0);
            f.store.tx(|tx| f.sched.dispatch(tx)).unwrap();
            f
        }

        #[test]
        fn grants_in_insert_order() {
            let mut f = dispatched();
            assert_eq!(f.offers_to("first"), 1);
            assert_eq!(f.offers_to("second"), 0);
        }
    }
}

/// This process as a holder recorded on boot `boot`.
fn self_holder_on(boot: Option<&str>) -> HolderRef {
    let mut holder = json!({
        "kind": "cli_process",
        "pid": std::process::id(),
        "start_time": liveness::self_start_time().unwrap(),
    });
    if let Some(boot) = boot {
        holder["boot_id"] = json!(boot);
    }
    serde_json::from_value(holder).unwrap()
}

/// This process as a host recorded on boot `boot`.
fn self_host_on(boot: Option<&str>) -> Host {
    let mut host = json!({
        "pid": std::process::id(),
        "start_time": liveness::self_start_time().unwrap(),
    });
    if let Some(boot) = boot {
        host["boot_id"] = json!(boot);
    }
    serde_json::from_value(host).unwrap()
}

fn this_boot() -> String {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .unwrap()
        .trim()
        .to_owned()
}

/// What a probe finds of a lease this process holds, recorded on `boot`.
fn probe_holder_on(boot: Option<&str>) -> Vec<HolderRef> {
    let mut f = fixture();
    f.grant(&as_caller(self_holder_on(boot)), "u1");
    f.store.tx(|tx| liveness::probe_all(tx, &f.sched)).unwrap()
}

/// The hosts a fresh daemon keeps of one recorded on `boot`.
fn boot_with_host_on(boot: Option<&str>) -> Vec<Host> {
    let mut f = fixture();
    let mut known = Sessions::new(MIN);
    f.with_tx(|tx| {
        sessions::add_host(tx, "s1", &self_host_on(boot), T0).unwrap();
        known.boot(tx, T0).unwrap();
    });
    known.hosts_of("s1")
}

mod liveness_by_boot {
    use super::*;

    mod on_a_holder_recorded_with_another_boot_id {
        use super::*;

        #[test]
        fn treats_it_as_dead() {
            let holder = self_holder_on(Some("another-boot"));
            assert_eq!(probe_holder_on(Some("another-boot")), vec![holder]);
        }
    }

    mod on_a_holder_recorded_with_this_boot_id {
        use super::*;

        #[test]
        fn leaves_it_alone() {
            assert_eq!(probe_holder_on(Some(&this_boot())), Vec::new());
        }
    }

    mod on_a_legacy_holder_without_a_boot_id {
        use super::*;

        #[test]
        fn leaves_it_alone() {
            assert_eq!(probe_holder_on(None), Vec::new());
        }

        #[test]
        fn encodes_as_it_was_stored() {
            let holder = HolderRef::User { tag: "x".into() };
            assert_eq!(
                serde_json::to_value(&holder).unwrap(),
                json!({"kind": "user", "tag": "x"})
            );
            let legacy = self_holder_on(None);
            assert!(
                serde_json::to_value(&legacy)
                    .unwrap()
                    .get("boot_id")
                    .is_none()
            );
        }
    }

    mod on_a_host_recorded_with_another_boot_id {
        use super::*;

        #[test]
        fn is_forgotten_at_daemon_start() {
            assert_eq!(boot_with_host_on(Some("another-boot")), Vec::new());
        }
    }

    mod on_a_legacy_host_without_a_boot_id {
        use super::*;

        #[test]
        fn survives_daemon_start() {
            assert_eq!(boot_with_host_on(None).len(), 1);
        }

        /// A daemon that loaded the legacy host, then heard from the same
        /// process with its boot id.
        fn touched() -> (Fixture, Sessions) {
            let mut f = fixture();
            let mut known = Sessions::new(MIN);
            f.with_tx(|tx| {
                sessions::add_host(tx, "s1", &self_host_on(None), T0).unwrap();
                known.boot(tx, T0).unwrap();
                known
                    .touch(tx, "s1", Some(self_host_on(Some(&this_boot()))), T0)
                    .unwrap();
            });
            (f, known)
        }

        #[test]
        fn is_the_same_host_once_it_reports_its_boot_id() {
            let (_f, known) = touched();
            assert_eq!(known.hosts_of("s1"), vec![self_host_on(Some(&this_boot()))]);
        }

        #[test]
        fn its_session_end_ends_the_session() {
            let mut f = fixture();
            let mut known = Sessions::new(MIN);
            let sched = Scheduler::new(
                Arc::new(ManualClock::new(T0)),
                Config::default(),
                Arc::new(Healthy),
            );
            let ended = f.with_tx(|tx| {
                sessions::add_host(tx, "s1", &self_host_on(None), T0).unwrap();
                known.boot(tx, T0).unwrap();
                let host = self_host_on(Some(&this_boot()));
                known.end(tx, &sched, "s1", Some(host), "exit").unwrap()
            });
            assert!(ended.is_some());
        }

        #[test]
        fn stores_the_boot_id_it_reported() {
            let (mut f, _known) = touched();
            let hosts = f.with_tx(|tx| sessions::hosts(tx).unwrap());
            assert_eq!(
                hosts,
                vec![("s1".to_owned(), self_host_on(Some(&this_boot())))]
            );
        }
    }
}
