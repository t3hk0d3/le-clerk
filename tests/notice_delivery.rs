//! Acknowledged notice delivery as the CLI's waiters live it: a real engine
//! behind a socket that can lose or hide `/v1/notices` answers, and the real
//! `clerk` binary.

mod common;

use std::fs;
use std::process::{Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::time::Duration;

use assert_cmd::cargo::cargo_bin;
use axum::Router;
use axum::body::{self, Body};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use common::{PATIENCE, PID_VAR, SESSION_VAR, TestEnv};
use leclerk::clock::SystemClock;
use leclerk::config::{Config, Paths};
use leclerk::daemon::server as http;
use leclerk::engine::{AgentWait, Command, Engine, EngineHandle};
use leclerk::health::HealthResult;
use leclerk::model::{Caller, HealthSpec, HolderRef, Host, ResourceId, UnitId, UnitSettings};
use leclerk::scheduler::{HealthProbe, liveness};
use leclerk::store::Store;
use leclerk::store::notices::{Notice, NoticeKind};
use serde_json::{Value, json};
use tokio::process::{Child, Command as ProcessCommand};
use tower::ServiceExt;

/// `/v1/notices` goes straight to the engine.
const PASS: u8 = 0;
/// The first answer with notices in it is lost after the engine made it, as
/// when a stalled engine answers a client that gave up.
const LOSE_FIRST: u8 = 1;
/// `/v1/notices` answers nothing, as for a waiter between two polls.
const HIDE: u8 = 2;
/// As [`HIDE`], and the waiter cannot read its request's state either: the
/// armed wait is sure to see the news first.
const BLIND: u8 = 3;

const SESSION: &str = "sess-1";

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

#[derive(Clone)]
struct Gate {
    inner: Router,
    mode: Arc<AtomicU8>,
    lost: Arc<AtomicUsize>,
    reading: Arc<AtomicUsize>,
}

async fn through_gate(State(gate): State<Gate>, request: Request) -> Response {
    if request.uri().path() == "/v1/rpc/list_my_leases" {
        return state_read(gate, request).await;
    }
    let notices = request.uri().path() == "/v1/notices";
    let mode = gate.mode.load(Ordering::SeqCst);
    if notices && (mode == HIDE || mode == BLIND) {
        tokio::time::sleep(Duration::from_millis(20)).await;
        return axum::Json(json!({ "result": [] })).into_response();
    }
    let response = gate.inner.clone().oneshot(request).await.unwrap();
    if !notices || mode != LOSE_FIRST {
        return response;
    }
    let (parts, answer) = response.into_parts();
    let bytes = body::to_bytes(answer, usize::MAX).await.unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    let has_notices = value["result"].as_array().is_some_and(|l| !l.is_empty());
    if has_notices
        && gate
            .lost
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    {
        let error = json!({"error": {"code": "engine_unavailable", "message": "lost"}});
        return (StatusCode::SERVICE_UNAVAILABLE, axum::Json(error)).into_response();
    }
    Response::from_parts(parts, Body::from(bytes))
}

/// A waiter reading its request's state: refused while [`BLIND`]. It counts
/// as reading before it looks at the mode, so [`Daemon::blind_waiters`] can
/// wait out the reads that got past.
async fn state_read(gate: Gate, request: Request) -> Response {
    gate.reading.fetch_add(1, Ordering::SeqCst);
    let response = if gate.mode.load(Ordering::SeqCst) == BLIND {
        let error = json!({"error": {"code": "internal_error", "message": "blind"}});
        (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(error)).into_response()
    } else {
        gate.inner.clone().oneshot(request).await.unwrap()
    };
    gate.reading.fetch_sub(1, Ordering::SeqCst);
    response
}

/// A real engine serving a private clerk home through a [`Gate`].
struct Daemon {
    env: TestEnv,
    engine: EngineHandle,
    mode: Arc<AtomicU8>,
    lost: Arc<AtomicUsize>,
    reading: Arc<AtomicUsize>,
}

impl Daemon {
    async fn new() -> Self {
        Self::with_config(Config::default()).await
    }

    /// A daemon whose leases warn of their expiry almost at once.
    async fn warning_early() -> Self {
        Self::with_config(Config {
            warning_lead_ratio: 0.99,
            warning_lead_max: 86_400,
            ..Config::default()
        })
        .await
    }

    async fn with_config(config: Config) -> Self {
        let env = TestEnv::new();
        let home = env.home().to_path_buf();
        let paths =
            Paths::from_lookup(move |k| (k == "CLERK_HOME").then(|| home.clone().into_os_string()))
                .unwrap();
        fs::create_dir_all(paths.socket.parent().unwrap()).unwrap();
        let engine = Engine::spawn(
            Store::open_in_memory().unwrap(),
            Arc::new(SystemClock),
            config,
            Arc::new(Healthy),
        )
        .unwrap();
        let mode = Arc::new(AtomicU8::new(PASS));
        let lost = Arc::new(AtomicUsize::new(0));
        let reading = Arc::new(AtomicUsize::new(0));
        let gate = Gate {
            inner: http::router(engine.clone()),
            mode: mode.clone(),
            lost: lost.clone(),
            reading: reading.clone(),
        };
        let app = Router::new().fallback(through_gate).with_state(gate);
        let listener = tokio::net::UnixListener::bind(&paths.socket).unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        let daemon = Self {
            env,
            engine,
            mode,
            lost,
            reading,
        };
        daemon.seed_board().await;
        daemon
    }

    fn set_mode(&self, mode: u8) {
        self.mode.store(mode, Ordering::SeqCst);
    }

    /// From here on waiters learn nothing, until the mode changes: no read
    /// of a request's state is still on its way to the engine.
    async fn blind_waiters(&self) {
        self.set_mode(BLIND);
        until("state reads in flight end", || async {
            self.reading.load(Ordering::SeqCst) == 0
        })
        .await;
    }

    async fn rpc(&self, method: &str, caller: &Caller, args: Value) -> Value {
        let command = Command::parse(method, caller.clone(), args).unwrap();
        self.engine.call_from(command, Some(me())).await.unwrap()
    }

    async fn seed_board(&self) {
        let admin = user("admin");
        self.rpc(
            "create_resource",
            &admin,
            json!({"id": "board", "type": "generic", "scope": "user"}),
        )
        .await;
        self.rpc(
            "add_unit",
            &admin,
            json!({
                "resource": "board",
                "id": "u1",
                "settings": {"type": "generic", "identity_key": "key-u1"},
            }),
        )
        .await;
    }

    /// `user(tag)` takes the board for a minute; returns the lease id.
    async fn hold(&self, tag: &str) -> String {
        let grant = self
            .rpc(
                "acquire",
                &user(tag),
                json!({"target": {"resource": "board"}, "duration_ms": 60_000, "purpose": "t"}),
            )
            .await;
        grant["lease_id"].as_str().unwrap().to_owned()
    }

    /// `tag` gives `lease` back with its cleanup confirmed.
    async fn give_back(&self, tag: &str, lease: &str) {
        let args = json!({"lease_id": lease});
        self.rpc("release", &user(tag), args.clone()).await;
        self.rpc("confirm", &user(tag), args).await;
    }

    /// `clerk args…`, no terminal, as the user or as agent session
    /// [`SESSION`] run by this test process.
    fn spawn(&self, args: &[&str], agent: bool) -> Child {
        let mut command = ProcessCommand::new(cargo_bin("clerk"));
        command
            .args(args)
            .env("CLERK_HOME", self.env.home())
            .env_remove(SESSION_VAR)
            .env_remove(PID_VAR)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if agent {
            command
                .env(SESSION_VAR, SESSION)
                .env(PID_VAR, std::process::id().to_string());
        }
        command.spawn().unwrap()
    }

    /// The board's occupancy once it has a lease: (lease id, holder).
    async fn occupied(&self) -> (String, HolderRef) {
        for _ in 0..3000 {
            let status = self
                .rpc("status", &user("admin"), json!({"view": "all"}))
                .await;
            let occupancy = &status["resources"][0]["units"][0]["occupancy"];
            if let Some(lease) = occupancy["lease_id"].as_str() {
                let holder = serde_json::from_value(occupancy["holder"].clone()).unwrap();
                return (lease.to_owned(), holder);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the board was never held");
    }

    async fn until_parked(&self, who: &HolderRef) {
        until("the waiter is parked", || async {
            self.engine.is_waiting(who)
        })
        .await;
    }

    async fn pending_kinds(&self, who: &HolderRef) -> Vec<NoticeKind> {
        let pending: Vec<Notice> = self.engine.pending_notices(who.clone()).await.unwrap();
        pending.iter().map(|n| n.kind).collect()
    }

    /// The queued request of agent session [`SESSION`], once it has one.
    async fn queued_request(&self) -> String {
        let caller = agent();
        let mut request = None;
        for _ in 0..3000 {
            let mine = self.rpc("list_my_leases", &caller, json!({})).await;
            request = mine["requests"][0]["request_id"]
                .as_str()
                .map(str::to_owned);
            if request.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        request.expect("the session's request is queued")
    }

    /// The armed `clerk wait` reads the session's notices and acknowledges
    /// them all.
    async fn armed_wait_reads_all(&self) {
        let ids = self
            .armed_wait_step()
            .await
            .iter()
            .map(|n| n.id.clone())
            .collect();
        self.engine
            .ack_notices(session_holder(), ids)
            .await
            .unwrap();
    }

    /// One step of the armed `clerk wait`: returns the session's notices.
    async fn armed_wait_step(&self) -> Vec<Notice> {
        let answer = self
            .engine
            .agent_wait(SESSION.to_owned(), me(), PATIENCE, true, "armed".to_owned())
            .await
            .unwrap();
        match answer {
            AgentWait::Notices(notices) => notices,
            other => panic!("the armed wait got no notices: {other:?}"),
        }
    }
}

/// Poll `check` until it holds, for at most [`PATIENCE`].
async fn until<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let waited = tokio::time::timeout(PATIENCE, async {
        while !check().await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(waited.is_ok(), "timed out waiting until {what}");
}

/// `child`'s output once it ends, within [`PATIENCE`].
async fn finished(child: Child) -> Output {
    tokio::time::timeout(PATIENCE, child.wait_with_output())
        .await
        .expect("the command did not end")
        .unwrap()
}

fn me() -> Host {
    let pid = std::process::id();
    Host {
        pid,
        start_time: liveness::start_time(pid).expect("this process is running"),
        boot_id: liveness::boot_id(),
    }
}

fn user(tag: &str) -> Caller {
    Caller {
        holder: HolderRef::User { tag: tag.into() },
        project: None,
        override_reason: None,
    }
}

fn session_holder() -> HolderRef {
    HolderRef::AgentSession {
        session_id: SESSION.to_owned(),
    }
}

fn agent() -> Caller {
    Caller {
        holder: session_holder(),
        project: None,
        override_reason: None,
    }
}

const RUN: &[&str] = &["run", "board", "--for", "10m", "--", "sleep", "60"];

mod clerk_run {
    use super::*;

    /// The running command's lease is ended by an administrator; returns
    /// the holder of the lease.
    async fn end_the_lease(daemon: &Daemon) -> HolderRef {
        let (lease, holder) = daemon.occupied().await;
        daemon.until_parked(&holder).await;
        daemon
            .rpc("admin_release", &user("admin"), json!({"lease_id": lease}))
            .await;
        holder
    }

    mod on_a_lease_ended_notice_arriving_during_an_engine_stall {
        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn still_sees_it_and_stops_the_child() {
            let daemon = Daemon::new().await;
            daemon.set_mode(LOSE_FIRST);
            let child = daemon.spawn(RUN, false);
            end_the_lease(&daemon).await;
            let out = finished(child).await;
            assert_eq!(daemon.lost.load(Ordering::SeqCst), 1);
            assert_eq!(out.status.code(), Some(128 + 15), "{out:?}");
        }
    }

    mod in_an_agent_session_on_the_armed_wait_reading_the_end_first {
        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn still_stops_the_child() {
            let daemon = Daemon::new().await;
            daemon.set_mode(HIDE);
            let child = daemon.spawn(RUN, true);
            let (lease, _) = daemon.occupied().await;
            daemon
                .rpc("admin_release", &user("admin"), json!({"lease_id": lease}))
                .await;
            daemon.armed_wait_reads_all().await;
            let out = finished(child).await;
            assert_eq!(out.status.code(), Some(128 + 15), "{out:?}");
        }
    }

    mod in_an_agent_session_on_the_armed_wait_reading_the_cleanup_window_first {
        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn stops_the_child_at_the_windows_deadline() {
            let daemon = Daemon::with_config(Config {
                cleanup_window: 1,
                ..Config::default()
            })
            .await;
            daemon.set_mode(HIDE);
            let child = daemon.spawn(RUN, true);
            let (lease, _) = daemon.occupied().await;
            daemon
                .rpc("release", &agent(), json!({"lease_id": lease}))
                .await;
            daemon.armed_wait_reads_all().await;
            let out = finished(child).await;
            assert_eq!(out.status.code(), Some(128 + 15), "{out:?}");
        }
    }

    mod on_a_notice_it_handled {
        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn acknowledges_it() {
            let daemon = Daemon::new().await;
            let child = daemon.spawn(RUN, false);
            let holder = end_the_lease(&daemon).await;
            finished(child).await;
            assert_eq!(daemon.pending_kinds(&holder).await, Vec::new());
        }
    }

    mod in_an_agent_session_on_a_notice_it_does_not_act_on {
        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn leaves_it_pending_for_the_armed_wait() {
            let daemon = Daemon::warning_early().await;
            let dir = tempfile::TempDir::new().unwrap();
            let go = dir.path().join("go");
            let script = format!("until [ -e {0} ]; do sleep 0.05; done", go.display());
            let child = daemon.spawn(
                &["run", "board", "--for", "1m", "--", "sh", "-c", &script],
                true,
            );
            let holder = session_holder();
            until("the expiry warning is out", || async {
                daemon
                    .pending_kinds(&holder)
                    .await
                    .contains(&NoticeKind::ExpiryWarning)
            })
            .await;
            daemon.until_parked(&holder).await;
            fs::write(&go, "").unwrap();
            let out = finished(child).await;
            assert!(out.status.success(), "{out:?}");
            assert!(
                daemon
                    .pending_kinds(&holder)
                    .await
                    .contains(&NoticeKind::ExpiryWarning)
            );
        }
    }
}

/// An agent session cannot run `acquire --wait` (it is refused as a usage
/// error); `clerk run` is how a session comes to wait for an offer, through
/// the same `await_offer`.
mod waiting_for_an_offer_in_an_agent_session {
    use super::*;

    /// A session's `clerk run` writing its lease id to `marker`.
    fn run_recording(daemon: &Daemon, marker: &std::path::Path) -> Child {
        let script = format!("echo \"$CLERK_LEASE_ID\" > {}", marker.display());
        daemon.spawn(
            &[
                "run", "board", "--for", "10m", "--wait", "--", "sh", "-c", &script,
            ],
            true,
        )
    }

    /// A session's `clerk run` queued behind `other`, whose lease is
    /// returned.
    async fn offered(daemon: &Daemon, marker: &std::path::Path) -> Child {
        let other = daemon.hold("other").await;
        let child = run_recording(daemon, marker);
        daemon.queued_request().await;
        daemon.give_back("other", &other).await;
        child
    }

    /// As [`offered`], with the waiter blind until the test lets it see.
    async fn offered_unseen(daemon: &Daemon, marker: &std::path::Path) -> Child {
        let other = daemon.hold("other").await;
        let child = run_recording(daemon, marker);
        daemon.queued_request().await;
        daemon.blind_waiters().await;
        daemon.give_back("other", &other).await;
        child
    }

    mod on_the_armed_wait_claiming_the_offer_first {
        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn ends_with_the_granted_lease() {
            let daemon = Daemon::new().await;
            let dir = tempfile::TempDir::new().unwrap();
            let marker = dir.path().join("lease");
            daemon.set_mode(HIDE);
            let child = offered_unseen(&daemon, &marker).await;
            let notices = daemon.armed_wait_step().await;
            let offer = notices
                .iter()
                .find(|n| n.kind == NoticeKind::Offered)
                .expect("the offer");
            let lease = offer.lease_id.clone().unwrap();
            daemon
                .rpc("claim", &agent(), json!({"lease_id": lease}))
                .await;
            let ids = notices.iter().map(|n| n.id.clone()).collect();
            daemon
                .engine
                .ack_notices(session_holder(), ids)
                .await
                .unwrap();
            daemon.set_mode(HIDE);
            let out = finished(child).await;
            assert!(out.status.success(), "{out:?}");
            let ran_with = fs::read_to_string(&marker).unwrap();
            assert_eq!(ran_with.trim(), lease.as_str(), "{out:?}");
        }
    }

    mod on_the_armed_wait_reading_the_drop_first {
        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn ends_as_dropped() {
            let daemon = Daemon::new().await;
            let dir = tempfile::TempDir::new().unwrap();
            daemon.hold("other").await;
            daemon.set_mode(HIDE);
            let child = run_recording(&daemon, &dir.path().join("lease"));
            let request = daemon.queued_request().await;
            daemon.blind_waiters().await;
            daemon
                .rpc(
                    "admin_dequeue",
                    &user("admin"),
                    json!({"request_id": request}),
                )
                .await;
            let notices = daemon.armed_wait_step().await;
            let ids = notices.iter().map(|n| n.id.clone()).collect();
            daemon
                .engine
                .ack_notices(session_holder(), ids)
                .await
                .unwrap();
            daemon.set_mode(HIDE);
            let out = finished(child).await;
            assert_eq!(out.status.code(), Some(4), "{out:?}");
        }
    }

    mod on_an_offer_it_claims_itself {
        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn acknowledges_it() {
            let daemon = Daemon::new().await;
            let dir = tempfile::TempDir::new().unwrap();
            let child = offered(&daemon, &dir.path().join("lease")).await;
            let out = finished(child).await;
            assert!(out.status.success(), "{out:?}");
            assert!(
                !daemon
                    .pending_kinds(&session_holder())
                    .await
                    .contains(&NoticeKind::Offered)
            );
        }
    }
}

mod clerk_wait_for_the_user {
    use super::*;

    mod on_a_batch_it_printed {
        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn acknowledges_it() {
            let daemon = Daemon::warning_early().await;
            daemon.hold("a").await;
            let holder = user("a").holder;
            until("the expiry warning is out", || async {
                !daemon.pending_kinds(&holder).await.is_empty()
            })
            .await;
            let child = daemon.spawn(&["--json", "wait", "--once"], false);
            let out = finished(child).await;
            assert!(out.status.success(), "{out:?}");
            assert_eq!(daemon.pending_kinds(&holder).await, Vec::new());
        }
    }
}

mod live_view {
    use super::*;

    /// An agent session's live `clerk acquire`, whose notices are hidden
    /// from it, queued behind `other`'s lease; returns the lease's id.
    async fn queued_view(daemon: &Daemon) -> (common::Tty, String) {
        let other = daemon.hold("other").await;
        daemon.set_mode(HIDE);
        let clerk = cargo_bin("clerk");
        let pid = std::process::id().to_string();
        let tty = common::Tty::spawn_in(
            daemon.env.home(),
            "\"$CLERK\" acquire board --for 10m; echo EXIT=$?",
            &[
                ("CLERK", clerk.to_str().unwrap()),
                (SESSION_VAR, SESSION),
                (PID_VAR, &pid),
            ],
        );
        daemon.queued_request().await;
        daemon.blind_waiters().await;
        (tty, other)
    }

    mod on_the_armed_wait_claiming_the_offer_first {
        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn moves_on_to_the_held_lease() {
            let daemon = Daemon::new().await;
            let (tty, other) = queued_view(&daemon).await;
            daemon.give_back("other", &other).await;
            let notices = daemon.armed_wait_step().await;
            let offer = notices
                .iter()
                .find(|n| n.kind == NoticeKind::Offered)
                .expect("the offer");
            daemon
                .rpc("claim", &agent(), json!({"lease_id": offer.lease_id}))
                .await;
            let ids = notices.iter().map(|n| n.id.clone()).collect();
            daemon
                .engine
                .ack_notices(session_holder(), ids)
                .await
                .unwrap();
            daemon.set_mode(HIDE);
            tty.expect("Ctrl+C releases");
        }
    }

    mod on_the_armed_wait_reading_the_offer_without_claiming_it {
        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn claims_it() {
            let daemon = Daemon::new().await;
            let (tty, other) = queued_view(&daemon).await;
            daemon.give_back("other", &other).await;
            daemon.armed_wait_reads_all().await;
            daemon.set_mode(HIDE);
            tty.expect("Ctrl+C releases");
        }
    }

    mod on_the_armed_wait_reading_the_drop_first {
        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn ends_as_dropped() {
            let daemon = Daemon::new().await;
            let (tty, _) = queued_view(&daemon).await;
            let request = daemon.queued_request().await;
            daemon
                .rpc(
                    "admin_dequeue",
                    &user("admin"),
                    json!({"request_id": request}),
                )
                .await;
            daemon.armed_wait_reads_all().await;
            daemon.set_mode(HIDE);
            tty.expect("EXIT=4");
        }
    }

    mod in_an_agent_session_on_a_notice_it_does_not_act_on {
        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn leaves_it_pending_for_the_armed_wait() {
            let daemon = Daemon::warning_early().await;
            let clerk = cargo_bin("clerk");
            let pid = std::process::id().to_string();
            let tty = common::Tty::spawn_in(
                daemon.env.home(),
                "\"$CLERK\" acquire board --for 1m; echo EXIT=$?",
                &[
                    ("CLERK", clerk.to_str().unwrap()),
                    (SESSION_VAR, SESSION),
                    (PID_VAR, &pid),
                ],
            );
            let (lease, holder) = daemon.occupied().await;
            until("the expiry warning is out", || async {
                daemon
                    .pending_kinds(&holder)
                    .await
                    .contains(&NoticeKind::ExpiryWarning)
            })
            .await;
            daemon.until_parked(&holder).await;
            daemon
                .rpc("admin_release", &user("admin"), json!({"lease_id": lease}))
                .await;
            tty.expect("EXIT=0");
            assert!(
                daemon
                    .pending_kinds(&holder)
                    .await
                    .contains(&NoticeKind::ExpiryWarning)
            );
        }
    }

    mod on_the_end_of_its_lease {
        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn acknowledges_the_notice() {
            let daemon = Daemon::new().await;
            let clerk = cargo_bin("clerk");
            let tty = common::Tty::spawn_in(
                daemon.env.home(),
                "\"$CLERK\" acquire board --for 10m; echo EXIT=$?",
                &[("CLERK", clerk.to_str().unwrap())],
            );
            let (lease, holder) = daemon.occupied().await;
            daemon.until_parked(&holder).await;
            daemon
                .rpc("admin_release", &user("admin"), json!({"lease_id": lease}))
                .await;
            tty.expect("EXIT=0");
            assert_eq!(daemon.pending_kinds(&holder).await, Vec::new());
        }
    }
}
