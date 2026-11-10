//! Agent-session liveness (`scheduler::session`): connections, the
//! reconnect grace, its expiry, the grace from boot, `end_now`, the
//! SessionStart-resume alias and hook session resolution. Driven on the
//! scheduler with a manual clock, plus the engine wiring (timers, boot,
//! handle operations).

use std::sync::Arc;
use std::time::{Duration, Instant};

use leclerk::clock::ManualClock;
use leclerk::config::Config;
use leclerk::engine::{Engine, EngineHandle, EngineOptions};
use leclerk::health::HealthResult;
use leclerk::inventory::{Inventory, NewResource, NewUnit};
use leclerk::model::{
    Caller, HealthSpec, HolderRef, LeaseId, Priority, ProjectKey, RequestId, ResourceId,
    ResourceType, Scope, UnitId, UnitSettings,
};
use leclerk::scheduler::session::{AliasRefusal, Aliased, Sessions};
use leclerk::scheduler::{AcquireOutcome, AcquireRequest, HealthProbe, Scheduler, Target};
use leclerk::store::audit::{self, AuditEntry};
use leclerk::store::notices::{self, NoticeKind};
use leclerk::store::{Store, StoreError, Tx};
use ulid::Ulid;

const T0: i64 = 1_000_000;
const GRACE: Duration = Duration::from_secs(60);
const GRACE_MS: i64 = 60_000;

fn agent(id: &str) -> HolderRef {
    HolderRef::AgentSession {
        session_id: id.into(),
    }
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

fn req() -> AcquireRequest {
    AcquireRequest {
        target: Target::Resource(ResourceId::parse("board").unwrap()),
        duration: Duration::from_secs(3600),
        interruptible: false,
        priority: Priority::Normal,
        purpose: "test".into(),
        override_reason: None,
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

/// Seed `board` with one unit `u1`.
fn seed(store: &mut Store, inv: &Inventory) {
    let who = as_caller(user("admin"));
    store
        .tx(|tx| -> Result<(), leclerk::inventory::InventoryError> {
            inv.create_resource(
                tx,
                &who,
                &NewResource {
                    id: ResourceId::parse("board").unwrap(),
                    ty: ResourceType::Generic,
                    scope: Scope::User,
                    description: String::new(),
                    usage: String::new(),
                    health: None,
                    tags: Vec::new(),
                },
            )?;
            inv.add_unit(
                tx,
                &who,
                &ResourceId::parse("board").unwrap(),
                &NewUnit {
                    id: UnitId::parse("u1").unwrap(),
                    settings: UnitSettings::Generic {
                        identity_key: "board-u1".into(),
                    },
                    description: String::new(),
                },
            )?;
            Ok(())
        })
        .unwrap();
}

/// The lease state, end cause and holder of `lease`.
fn lease_row(tx: &Tx, lease: &LeaseId) -> (String, Option<String>, HolderRef) {
    tx.conn()
        .query_row(
            "SELECT state, end_cause, holder FROM leases WHERE lease_id = ?1",
            [lease.as_str()],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .map(|(s, c, h)| (s, c, serde_json::from_str(&h).unwrap()))
        .unwrap()
}

/// The holders of the queued requests.
fn queued_holders(tx: &Tx) -> Vec<HolderRef> {
    let mut stmt = tx
        .conn()
        .prepare("SELECT holder FROM requests WHERE state = 'queued' ORDER BY row_id")
        .unwrap();
    stmt.query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|h| serde_json::from_str(&h.unwrap()).unwrap())
        .collect()
}

fn project_key(dir: &str) -> ProjectKey {
    ProjectKey(std::path::PathBuf::from(dir))
}

// ---------------------------------------------------------------------------
// Scheduler-level fixture
// ---------------------------------------------------------------------------

struct Fixture {
    store: Store,
    sched: Scheduler,
    clock: Arc<ManualClock>,
    sessions: Sessions,
}

fn fixture() -> Fixture {
    let clock = Arc::new(ManualClock::new(T0));
    let inv = Inventory::new(clock.clone());
    let mut store = Store::open_in_memory().unwrap();
    seed(&mut store, &inv);
    Fixture {
        store,
        sched: Scheduler::new(clock.clone(), Config::default(), Arc::new(Healthy)),
        clock,
        sessions: Sessions::new(GRACE),
    }
}

impl Fixture {
    fn now(&self) -> i64 {
        leclerk::clock::Clock::now_ms(&*self.clock)
    }

    fn at(&self, ms: i64) {
        self.clock.set(ms);
    }

    fn with_tx<T>(&mut self, f: impl FnOnce(&Tx) -> T) -> T {
        self.store
            .tx(|tx| -> Result<T, StoreError> { Ok(f(tx)) })
            .unwrap()
    }

    fn acquire(&mut self, holder: HolderRef) -> AcquireOutcome {
        let who = as_caller(holder);
        self.store
            .tx(|tx| self.sched.acquire(tx, &who, req()))
            .unwrap()
    }

    fn grant(&mut self, holder: HolderRef) -> LeaseId {
        match self.acquire(holder) {
            AcquireOutcome::Granted(g) => g.lease_id,
            other => panic!("expected grant, got {other:?}"),
        }
    }

    fn queue(&mut self, holder: HolderRef) -> RequestId {
        match self.acquire(holder) {
            AcquireOutcome::Queued { request_id, .. } => request_id,
            other => panic!("expected queued, got {other:?}"),
        }
    }

    fn connect(&mut self, session: &str, connection: Ulid, pid: Option<u32>) {
        self.sessions.connect(session, connection, pid, None);
    }

    fn connect_in(&mut self, session: &str, connection: Ulid, project: &str) {
        self.sessions
            .connect(session, connection, None, Some(project_key(project)));
    }

    /// A daemon restart: the liveness record starts over from the store.
    fn restart(&mut self) {
        self.sessions = Sessions::new(GRACE);
        self.boot();
    }

    fn audit(&mut self, action: &str) -> Vec<AuditEntry> {
        self.store
            .tx(audit::all)
            .unwrap()
            .into_iter()
            .filter(|e| e.action == action)
            .collect()
    }

    fn disconnect(&mut self, connection: Ulid) {
        let now = self.now();
        self.sessions.disconnect(connection, now);
    }

    fn expire(&mut self) -> Vec<String> {
        let now = self.now();
        let (sessions, sched) = (&mut self.sessions, &self.sched);
        self.store.tx(|tx| sessions.expire(tx, sched, now)).unwrap()
    }

    fn boot(&mut self) -> Vec<String> {
        let now = self.now();
        let sessions = &mut self.sessions;
        self.store.tx(|tx| sessions.boot(tx, now)).unwrap()
    }

    fn end_now(&mut self, session: &str) {
        let (sessions, sched) = (&mut self.sessions, &self.sched);
        self.store
            .tx(|tx| sessions.end_now(tx, sched, session))
            .unwrap();
    }

    fn alias(&mut self, from: &str, to: &str) -> Aliased {
        let (sessions, sched) = (&mut self.sessions, &self.sched);
        self.store
            .tx(|tx| sessions.alias(tx, sched, from, to))
            .unwrap()
    }

    fn lease(&mut self, lease: &LeaseId) -> (String, Option<String>, HolderRef) {
        self.with_tx(|tx| lease_row(tx, lease))
    }

    fn queued(&mut self) -> Vec<HolderRef> {
        self.with_tx(queued_holders)
    }

    fn offered_to(&mut self, holder: &HolderRef) -> bool {
        self.with_tx(|tx| notices::pending(tx, holder).unwrap())
            .iter()
            .any(|n| n.kind == NoticeKind::Offered)
    }

    /// `s1` holds `board/u1` through connection `c`, a user `w` waits for
    /// it, and `c` has closed at T0.
    fn in_grace(&mut self) -> (LeaseId, Ulid) {
        let c = Ulid::new();
        self.connect("s1", c, Some(42));
        let lease = self.grant(agent("s1"));
        self.queue(user("w"));
        self.disconnect(c);
        (lease, c)
    }
}

// ---------------------------------------------------------------------------
// scheduler::session
// ---------------------------------------------------------------------------

mod scheduler {
    use super::*;

    mod session {
        use super::*;

        mod connect {
            use super::*;

            mod on_a_new_session {
                use super::*;

                #[test]
                fn marks_it_alive() {
                    let mut f = fixture();
                    f.connect("s1", Ulid::new(), Some(42));
                    assert!(f.sessions.is_alive("s1"));
                }
            }

            mod on_a_session_inside_its_grace {
                use super::*;

                #[test]
                fn cancels_the_grace_the_new_connection_takes_over() {
                    let mut f = fixture();
                    let (lease, _) = f.in_grace();
                    f.at(T0 + GRACE_MS / 2);
                    let c2 = Ulid::new();
                    f.connect("s1", c2, Some(43));
                    assert!(f.sessions.is_alive("s1"));
                    assert_eq!(f.sessions.grace_ends("s1"), None);
                    f.at(T0 + 2 * GRACE_MS);
                    assert_eq!(f.expire(), Vec::<String>::new());
                    assert_eq!(f.lease(&lease).0, "held");
                    // The new connection is now the one that counts.
                    f.disconnect(c2);
                    assert_eq!(f.sessions.grace_ends("s1"), Some(T0 + 3 * GRACE_MS));
                }
            }
        }

        mod disconnect {
            use super::*;

            mod on_the_sessions_last_open_connection {
                use super::*;

                #[test]
                fn starts_reconnect_grace() {
                    let mut f = fixture();
                    let c = Ulid::new();
                    f.connect("s1", c, None);
                    f.at(T0 + 5);
                    f.disconnect(c);
                    assert!(!f.sessions.is_alive("s1"));
                    assert_eq!(f.sessions.grace_ends("s1"), Some(T0 + 5 + GRACE_MS));
                }
            }

            mod on_another_connection_still_open {
                use super::*;

                #[test]
                fn nothing_changes() {
                    let mut f = fixture();
                    let (c1, c2) = (Ulid::new(), Ulid::new());
                    f.connect("s1", c1, None);
                    f.connect("s1", c2, None);
                    f.disconnect(c1);
                    assert!(f.sessions.is_alive("s1"));
                    assert_eq!(f.sessions.grace_ends("s1"), None);
                }
            }
        }

        mod on_grace_expiry {
            use super::*;

            #[test]
            fn nothing_ends_before_the_grace_is_over() {
                let mut f = fixture();
                let (lease, _) = f.in_grace();
                f.at(T0 + GRACE_MS - 1);
                assert_eq!(f.expire(), Vec::<String>::new());
                assert_eq!(f.lease(&lease).0, "held");
            }

            #[test]
            fn ends_the_sessions_leases_with_cause_holder_gone() {
                let mut f = fixture();
                let (lease, _) = f.in_grace();
                f.at(T0 + GRACE_MS);
                assert_eq!(f.expire(), vec!["s1".to_owned()]);
                let (state, cause, _) = f.lease(&lease);
                assert_eq!(state, "ended");
                assert_eq!(cause.as_deref(), Some("holder_gone"));
            }

            #[test]
            fn drops_the_sessions_requests() {
                let mut f = fixture();
                let c = Ulid::new();
                f.connect("s1", c, None);
                f.grant(user("h"));
                f.queue(agent("s1"));
                f.disconnect(c);
                f.at(T0 + GRACE_MS);
                f.expire();
                assert_eq!(f.queued(), Vec::<HolderRef>::new());
            }

            #[test]
            fn next_waiter_is_offered_the_unit() {
                let mut f = fixture();
                f.in_grace();
                f.at(T0 + GRACE_MS);
                f.expire();
                assert!(f.offered_to(&user("w")));
            }

            #[test]
            fn the_session_is_forgotten() {
                let mut f = fixture();
                f.in_grace();
                f.at(T0 + GRACE_MS);
                f.expire();
                assert_eq!(f.sessions.grace_ends("s1"), None);
                assert_eq!(f.sessions.next_expiry(), None);
            }
        }

        mod on_daemon_start {
            use super::*;

            #[test]
            fn every_agent_session_holder_with_live_leases_or_requests_gets_reconnect_grace_from_boot()
             {
                let mut f = fixture();
                f.grant(agent("holder"));
                f.queue(agent("waiter"));
                f.queue(user("w"));
                f.at(T0 + 7);
                let mut graced = f.boot();
                graced.sort();
                assert_eq!(graced, vec!["holder".to_owned(), "waiter".to_owned()]);
                assert_eq!(f.sessions.grace_ends("holder"), Some(T0 + 7 + GRACE_MS));
                assert_eq!(f.sessions.grace_ends("waiter"), Some(T0 + 7 + GRACE_MS));
            }
        }

        mod end_now {
            use super::*;

            #[test]
            fn ends_the_sessions_leases_and_requests_without_grace() {
                let mut f = fixture();
                let c = Ulid::new();
                f.connect("s1", c, None);
                let lease = f.grant(agent("s1"));
                f.queue(agent("s1"));
                f.end_now("s1");
                let (state, cause, _) = f.lease(&lease);
                assert_eq!(state, "ended");
                assert_eq!(cause.as_deref(), Some("holder_gone"));
                assert_eq!(f.queued(), Vec::<HolderRef>::new());
            }
        }

        mod alias {
            use super::*;

            /// `old` holds `board/u1` and queues again; its proxy is gone
            /// (grace running). A new proxy session `new` with Claude PID 42
            /// is connected, and the SessionStart-resume alias runs.
            fn resumed() -> (Fixture, LeaseId) {
                let mut f = fixture();
                let c = Ulid::new();
                f.connect("old", c, Some(41));
                let lease = f.grant(agent("old"));
                f.queue(agent("old"));
                f.disconnect(c);
                f.connect("new", Ulid::new(), Some(42));
                assert_eq!(f.alias("new", "old"), Aliased::TakenOver);
                (f, lease)
            }

            /// `s1` holds `board/u1` and is gone; `s2` resumed it and is gone
            /// too. Returns the lease.
            fn resumed_once(f: &mut Fixture) -> LeaseId {
                let c1 = Ulid::new();
                f.connect("s1", c1, Some(41));
                let lease = f.grant(agent("s1"));
                f.disconnect(c1);
                let c2 = Ulid::new();
                f.connect("s2", c2, Some(42));
                assert_eq!(f.alias("s2", "s1"), Aliased::TakenOver);
                f.disconnect(c2);
                lease
            }

            /// The reasons of the `session_alias_refused` audit entries.
            fn refusals(f: &mut Fixture) -> Vec<serde_json::Value> {
                f.audit("session_alias_refused")
                    .into_iter()
                    .map(|e| e.detail["reason"].clone())
                    .collect()
            }

            mod on_the_transaction_failing_after_it {
                use super::*;

                /// `old` holds `board/u1` and is gone; `new` is connected;
                /// the alias runs in a transaction that then fails (as a
                /// failed commit would), through [`Sessions::transact`].
                fn failed() -> Fixture {
                    let mut f = fixture();
                    let c = Ulid::new();
                    f.connect("old", c, Some(41));
                    f.grant(agent("old"));
                    f.disconnect(c);
                    f.connect("new", Ulid::new(), Some(42));
                    let sched = &f.sched;
                    let result: Result<(), StoreError> =
                        f.sessions.transact(&mut f.store, |tx, sessions| {
                            let done = sessions.alias(tx, sched, "new", "old").unwrap();
                            assert_eq!(done, Aliased::TakenOver);
                            Err(StoreError::Corrupt("injected"))
                        });
                    assert!(result.is_err());
                    f
                }

                #[test]
                fn the_resumed_id_still_resolves_to_itself() {
                    let f = failed();
                    assert_eq!(f.sessions.resolve(None, "old"), "old");
                }

                #[test]
                fn the_resumed_session_keeps_its_grace() {
                    let f = failed();
                    assert_eq!(f.sessions.grace_ends("old"), Some(T0 + GRACE_MS));
                }

                #[test]
                fn a_later_alias_still_takes_it_over() {
                    let mut f = failed();
                    assert_eq!(f.alias("new", "old"), Aliased::TakenOver);
                }
            }

            mod on_the_transaction_committing {
                use super::*;

                #[test]
                fn transact_keeps_the_changes() {
                    let mut f = fixture();
                    let c = Ulid::new();
                    f.connect("old", c, Some(41));
                    f.grant(agent("old"));
                    f.disconnect(c);
                    f.connect("new", Ulid::new(), Some(42));
                    let sched = &f.sched;
                    let done: Result<Aliased, leclerk::scheduler::SchedulerError> =
                        f.sessions.transact(&mut f.store, |tx, sessions| {
                            sessions.alias(tx, sched, "new", "old")
                        });
                    assert_eq!(done.unwrap(), Aliased::TakenOver);
                    assert_eq!(f.sessions.resolve(None, "old"), "new");
                }
            }

            mod on_two_resumes_in_a_row {
                use super::*;

                #[test]
                fn the_latest_proxy_session_holds_the_leases() {
                    let mut f = fixture();
                    let lease = resumed_once(&mut f);
                    f.connect("s3", Ulid::new(), Some(43));
                    assert_eq!(f.alias("s3", "s1"), Aliased::TakenOver);
                    assert_eq!(f.lease(&lease).2, agent("s3"));
                }
            }

            mod on_a_daemon_restart_between_two_resumes {
                use super::*;

                #[test]
                fn the_latest_proxy_session_holds_the_leases() {
                    let mut f = fixture();
                    let lease = resumed_once(&mut f);
                    f.restart();
                    f.connect("s3", Ulid::new(), Some(43));
                    assert_eq!(f.alias("s3", "s1"), Aliased::TakenOver);
                    assert_eq!(f.lease(&lease).2, agent("s3"));
                }

                #[test]
                fn the_resumed_id_still_resolves_to_its_proxy_session() {
                    let mut f = fixture();
                    resumed_once(&mut f);
                    f.restart();
                    assert_eq!(f.sessions.resolve(None, "s1"), "s2");
                }
            }

            mod on_the_resumed_session_still_connected {
                use super::*;

                fn refused() -> (Fixture, LeaseId, Aliased) {
                    let mut f = fixture();
                    f.connect("old", Ulid::new(), Some(41));
                    let lease = f.grant(agent("old"));
                    f.connect("new", Ulid::new(), Some(42));
                    let got = f.alias("new", "old");
                    (f, lease, got)
                }

                #[test]
                fn refuses_and_moves_nothing() {
                    let (mut f, lease, got) = refused();
                    assert_eq!(got, Aliased::Refused(AliasRefusal::ResumedStillConnected));
                    assert_eq!(f.lease(&lease).2, agent("old"));
                    assert_eq!(f.sessions.resolve(None, "old"), "old");
                }

                #[test]
                fn records_session_alias_refused_with_the_reason() {
                    let (mut f, _, _) = refused();
                    assert_eq!(
                        refusals(&mut f),
                        vec![serde_json::json!("resumed_still_connected")]
                    );
                }
            }

            mod on_the_new_proxy_not_connected {
                use super::*;

                fn refused() -> (Fixture, LeaseId, Aliased) {
                    let mut f = fixture();
                    let c = Ulid::new();
                    f.connect("old", c, Some(41));
                    let lease = f.grant(agent("old"));
                    f.disconnect(c);
                    let got = f.alias("new", "old");
                    (f, lease, got)
                }

                #[test]
                fn refuses_and_moves_nothing() {
                    let (mut f, lease, got) = refused();
                    assert_eq!(got, Aliased::Refused(AliasRefusal::NotConnected));
                    assert_eq!(f.lease(&lease).2, agent("old"));
                    assert_eq!(f.sessions.grace_ends("old"), Some(T0 + GRACE_MS));
                }

                #[test]
                fn records_session_alias_refused_with_the_reason() {
                    let (mut f, _, _) = refused();
                    assert_eq!(refusals(&mut f), vec![serde_json::json!("not_connected")]);
                }
            }

            mod on_sessions_of_different_projects {
                use super::*;

                fn refused() -> (Fixture, LeaseId, Aliased) {
                    let mut f = fixture();
                    let c = Ulid::new();
                    f.connect_in("old", c, "/p/one");
                    let lease = f.grant(agent("old"));
                    f.disconnect(c);
                    f.connect_in("new", Ulid::new(), "/p/two");
                    let got = f.alias("new", "old");
                    (f, lease, got)
                }

                #[test]
                fn refuses_and_moves_nothing() {
                    let (mut f, lease, got) = refused();
                    assert_eq!(got, Aliased::Refused(AliasRefusal::ProjectMismatch));
                    assert_eq!(f.lease(&lease).2, agent("old"));
                }

                #[test]
                fn records_session_alias_refused_with_the_reason() {
                    let (mut f, _, _) = refused();
                    assert_eq!(
                        refusals(&mut f),
                        vec![serde_json::json!("project_mismatch")]
                    );
                }

                #[test]
                fn refuses_after_a_restart_by_the_project_of_the_resumed_leases() {
                    let mut f = fixture();
                    let who = Caller {
                        project: Some(project_key("/p/one")),
                        ..as_caller(agent("old"))
                    };
                    f.store.tx(|tx| f.sched.acquire(tx, &who, req())).unwrap();
                    f.restart();
                    f.connect_in("new", Ulid::new(), "/p/two");
                    assert_eq!(
                        f.alias("new", "old"),
                        Aliased::Refused(AliasRefusal::ProjectMismatch)
                    );
                }
            }

            #[test]
            fn leases_of_the_resumed_id_are_held_by_the_live_proxy_session() {
                let (mut f, lease) = resumed();
                let (state, _, holder) = f.lease(&lease);
                assert_eq!(state, "held");
                assert_eq!(holder, agent("new"));
            }

            #[test]
            fn requests_of_the_resumed_id_are_held_by_the_live_proxy_session() {
                let (mut f, _) = resumed();
                assert_eq!(f.queued(), vec![agent("new")]);
            }

            #[test]
            fn its_grace_is_cancelled() {
                let (mut f, lease) = resumed();
                assert_eq!(f.sessions.grace_ends("old"), None);
                f.at(T0 + 2 * GRACE_MS);
                assert_eq!(f.expire(), Vec::<String>::new());
                assert_eq!(f.lease(&lease).0, "held");
            }

            #[test]
            fn the_resumed_id_resolves_to_the_live_proxy_session() {
                let (f, _) = resumed();
                assert_eq!(f.sessions.resolve(None, "old"), "new");
            }
        }

        mod resolve {
            use super::*;

            mod on_a_live_proxy_session_with_that_claude_pid {
                use super::*;

                #[test]
                fn returns_it() {
                    let mut f = fixture();
                    f.connect("proxy", Ulid::new(), Some(42));
                    assert_eq!(f.sessions.resolve(Some(42), "hook-id"), "proxy");
                }
            }

            mod on_none {
                use super::*;

                #[test]
                fn returns_session_id() {
                    let mut f = fixture();
                    let c = Ulid::new();
                    f.connect("proxy", c, Some(42));
                    f.disconnect(c);
                    assert_eq!(f.sessions.resolve(Some(42), "hook-id"), "hook-id");
                    assert_eq!(f.sessions.resolve(Some(7), "hook-id"), "hook-id");
                    assert_eq!(f.sessions.resolve(None, "hook-id"), "hook-id");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// engine wiring: timers, boot and handle operations
// ---------------------------------------------------------------------------

/// An engine on `store` with a manual clock, a grace of [`GRACE`] and a
/// fast loop, so advancing the clock is seen within milliseconds.
fn engine_on(store: Store, clock: Arc<ManualClock>) -> EngineHandle {
    Engine::spawn_with(
        store,
        clock,
        Config::default(),
        Arc::new(Healthy),
        EngineOptions {
            liveness_interval: Duration::from_millis(10),
        },
    )
    .unwrap()
}

/// A seeded store and its clock.
fn seeded() -> (Store, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::new(T0));
    let mut store = Store::open_in_memory().unwrap();
    seed(&mut store, &Inventory::new(clock.clone()));
    (store, clock)
}

async fn call(engine: &EngineHandle, holder: HolderRef, method: &str) -> serde_json::Value {
    let args = match method {
        "acquire" => serde_json::json!({
            "target": {"resource": "board"}, "duration_ms": 3_600_000, "purpose": "test"
        }),
        _ => serde_json::json!({}),
    };
    engine
        .call(leclerk::engine::Command::parse(method, as_caller(holder), args).unwrap())
        .await
        .unwrap()
}

/// The state of `holder`'s only live lease, or `None`.
async fn state_of(engine: &EngineHandle, holder: HolderRef) -> Option<String> {
    let mine = call(engine, holder, "list_my_leases").await;
    mine["leases"]
        .as_array()
        .unwrap()
        .first()
        .map(|l| l["state"].as_str().unwrap().to_owned())
}

async fn eventually_state(engine: &EngineHandle, holder: HolderRef, want: Option<&str>) {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let got = state_of(engine, holder.clone()).await;
        if got.as_deref() == want {
            return;
        }
        assert!(
            Instant::now() < until,
            "state stayed {got:?}, wanted {want:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

mod engine {
    use super::*;

    mod session_connect {
        use super::*;

        mod on_the_link_dropped_and_the_grace_over {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn the_engine_ends_the_lease_and_offers_the_waiter() {
                let (store, clock) = seeded();
                let engine = engine_on(store, clock.clone());
                let link = engine
                    .session_connect("s1".into(), Ulid::new(), Some(42), None)
                    .await
                    .unwrap();
                call(&engine, agent("s1"), "acquire").await;
                call(&engine, user("w"), "acquire").await;
                drop(link);
                // Jobs run in order: once this answers, the stream has closed.
                assert!(!engine.session_alive("s1".into()).await.unwrap());
                clock.advance(GRACE.checked_sub(Duration::from_millis(1)).unwrap());
                tokio::time::sleep(Duration::from_millis(100)).await;
                assert_eq!(
                    state_of(&engine, agent("s1")).await.as_deref(),
                    Some("held")
                );
                clock.advance(Duration::from_millis(1));
                eventually_state(&engine, agent("s1"), None).await;
                eventually_state(&engine, user("w"), Some("offered")).await;
            }
        }

        mod on_a_reconnect_inside_the_grace {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn the_lease_outlives_the_grace() {
                let (store, clock) = seeded();
                let engine = engine_on(store, clock.clone());
                let link = engine
                    .session_connect("s1".into(), Ulid::new(), Some(42), None)
                    .await
                    .unwrap();
                call(&engine, agent("s1"), "acquire").await;
                drop(link);
                assert!(!engine.session_alive("s1".into()).await.unwrap());
                clock.advance(GRACE / 2);
                let _again = engine
                    .session_connect("s1".into(), Ulid::new(), Some(42), None)
                    .await
                    .unwrap();
                clock.advance(2 * GRACE);
                tokio::time::sleep(Duration::from_millis(100)).await;
                assert_eq!(
                    state_of(&engine, agent("s1")).await.as_deref(),
                    Some("held")
                );
            }
        }
    }

    mod on_daemon_start {
        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn an_agent_sessions_lease_ends_after_the_grace_from_boot() {
            let (mut store, clock) = seeded();
            let sched = Scheduler::new(clock.clone(), Config::default(), Arc::new(Healthy));
            store
                .tx(|tx| sched.acquire(tx, &as_caller(agent("s1")), req()))
                .unwrap();
            let engine = engine_on(store, clock.clone());
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(
                state_of(&engine, agent("s1")).await.as_deref(),
                Some("held")
            );
            clock.advance(GRACE);
            eventually_state(&engine, agent("s1"), None).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_proxy_reconnecting_keeps_it() {
            let (mut store, clock) = seeded();
            let sched = Scheduler::new(clock.clone(), Config::default(), Arc::new(Healthy));
            store
                .tx(|tx| sched.acquire(tx, &as_caller(agent("s1")), req()))
                .unwrap();
            let engine = engine_on(store, clock.clone());
            let _link = engine
                .session_connect("s1".into(), Ulid::new(), None, None)
                .await
                .unwrap();
            clock.advance(2 * GRACE);
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(
                state_of(&engine, agent("s1")).await.as_deref(),
                Some("held")
            );
        }
    }

    mod session_end_now {
        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn ends_the_sessions_lease_at_once() {
            let (store, clock) = seeded();
            let engine = engine_on(store, clock);
            let _link = engine
                .session_connect("s1".into(), Ulid::new(), None, None)
                .await
                .unwrap();
            call(&engine, agent("s1"), "acquire").await;
            engine.session_end_now("s1".into()).await.unwrap();
            assert_eq!(state_of(&engine, agent("s1")).await, None);
        }
    }

    mod session_alias {
        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn the_live_proxy_session_keeps_the_resumed_sessions_lease_past_the_grace() {
            let (store, clock) = seeded();
            let engine = engine_on(store, clock.clone());
            let old = engine
                .session_connect("old".into(), Ulid::new(), Some(41), None)
                .await
                .unwrap();
            call(&engine, agent("old"), "acquire").await;
            drop(old);
            assert!(!engine.session_alive("old".into()).await.unwrap());
            let _new = engine
                .session_connect("new".into(), Ulid::new(), Some(42), None)
                .await
                .unwrap();
            engine
                .session_alias("new".into(), "old".into())
                .await
                .unwrap();
            clock.advance(2 * GRACE);
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(
                state_of(&engine, agent("new")).await.as_deref(),
                Some("held")
            );
            assert_eq!(
                engine.session_resolve(None, "old".into()).await.unwrap(),
                "new"
            );
        }
    }

    /// Any command, CLI or RPC, of an agent session that a resume aliased:
    /// `old` held the board and was resumed by `new`.
    mod any_command_of_an_aliased_session {
        use super::*;

        async fn resumed() -> (
            EngineHandle,
            leclerk::engine::SessionLink,
            serde_json::Value,
        ) {
            let (store, clock) = seeded();
            let engine = engine_on(store, clock);
            let old = engine
                .session_connect("old".into(), Ulid::new(), Some(41), None)
                .await
                .unwrap();
            let grant = call(&engine, agent("old"), "acquire").await;
            drop(old);
            assert!(!engine.session_alive("old".into()).await.unwrap());
            let new = engine
                .session_connect("new".into(), Ulid::new(), Some(42), None)
                .await
                .unwrap();
            engine
                .session_alias("new".into(), "old".into())
                .await
                .unwrap();
            (engine, new, grant)
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn acts_on_the_live_sessions_leases() {
            let (engine, _link, grant) = resumed().await;
            let lease = grant["lease_id"].clone();
            let released = engine
                .call(
                    leclerk::engine::Command::parse(
                        "release",
                        as_caller(agent("old")),
                        serde_json::json!({ "lease_id": lease }),
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(released["state"], "cleaning", "{released}");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_new_lease_is_held_by_the_live_session() {
            let (engine, _link, grant) = resumed().await;
            let lease = grant["lease_id"].clone();
            let as_old = |method: &str, args: serde_json::Value| {
                leclerk::engine::Command::parse(method, as_caller(agent("old")), args).unwrap()
            };
            engine
                .call(as_old("release", serde_json::json!({ "lease_id": lease })))
                .await
                .unwrap();
            engine
                .call(as_old("confirm", serde_json::json!({ "lease_id": lease })))
                .await
                .unwrap();
            call(&engine, agent("old"), "acquire").await;
            assert_eq!(
                state_of(&engine, agent("new")).await.as_deref(),
                Some("held")
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn reads_the_live_sessions_notices() {
            let (engine, _link, grant) = resumed().await;
            let admin = Caller {
                holder: user("admin"),
                project: None,
                override_reason: None,
            };
            engine
                .call(
                    leclerk::engine::Command::parse(
                        "admin_release",
                        admin,
                        serde_json::json!({ "lease_id": grant["lease_id"] }),
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
            let theirs = engine.pending_notices(agent("new")).await.unwrap();
            assert_ne!(theirs.len(), 0);
            let via_old = engine.pending_notices(agent("old")).await.unwrap();
            assert_eq!(via_old.len(), theirs.len());
        }
    }

    mod session_resolve {
        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn finds_the_live_proxy_session_by_claude_pid() {
            let (store, clock) = seeded();
            let engine = engine_on(store, clock);
            let _link = engine
                .session_connect("proxy".into(), Ulid::new(), Some(42), None)
                .await
                .unwrap();
            assert_eq!(
                engine
                    .session_resolve(Some(42), "hook".into())
                    .await
                    .unwrap(),
                "proxy"
            );
            assert!(engine.session_alive("proxy".into()).await.unwrap());
        }
    }
}
