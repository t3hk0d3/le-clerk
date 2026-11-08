mod common;

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use leclerk::client::{Client, DaemonError, DaemonInfo, Launcher};
use leclerk::clock::{Clock, ManualClock, SystemClock};
use leclerk::config::{Config, Paths};
use leclerk::health::HealthResult;
use leclerk::inventory::{Inventory, NewResource, NewUnit};
use leclerk::model::{
    Caller, HealthSpec, HolderRef, LeaseId, Priority, ResourceId, ResourceType, Scope, UnitId,
    UnitSettings,
};
use leclerk::scheduler::{AcquireOutcome, AcquireRequest, Confirm, HealthProbe, Scheduler, Target};
use leclerk::store::notices::{Notice, NoticeKind};
use leclerk::store::{Store, StoreError, Tx, audit};
use serde_json::{Value, json};

use common::TestEnv;

/// Upper bound for anything event-driven; generous so a loaded machine never
/// flakes.
const PATIENCE: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Process helpers
// ---------------------------------------------------------------------------

/// The `CLERK_HOME=<home>` entry a process environment must contain.
fn home_entry(home: &Path) -> Vec<u8> {
    let mut entry = b"CLERK_HOME=".to_vec();
    entry.extend_from_slice(home.as_os_str().as_encoded_bytes());
    entry
}

/// True when `pid` exists and is not a zombie.
fn alive(pid: u32) -> bool {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        // The state follows the parenthesised command name.
        Ok(stat) => stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z')),
        Err(_) => false,
    }
}

/// Live processes whose environment holds `CLERK_HOME=<home>`.
fn processes_under(home: &Path) -> Vec<u32> {
    let entry = home_entry(home);
    let Ok(dir) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            fs::read(format!("/proc/{pid}/environ"))
                .is_ok_and(|env| env.split(|b| *b == 0).any(|v| v == entry.as_slice()))
        })
        .filter(|pid| alive(*pid))
        .collect()
}

/// Live `clerk daemon` processes under `home`.
fn daemons_under(home: &Path) -> Vec<u32> {
    processes_under(home)
        .into_iter()
        .filter(|pid| {
            fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|cmd| {
                let args: Vec<&[u8]> = cmd.split(|b| *b == 0).filter(|a| !a.is_empty()).collect();
                args.len() == 2 && args[1] == b"daemon"
            })
        })
        .collect()
}

/// Wait (bounded) until `done` holds.
fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Stops every process left under a `CLERK_HOME` when dropped, also on
/// panic: SIGTERM first, SIGKILL after a grace period.
struct Reaper {
    home: PathBuf,
}

impl Drop for Reaper {
    fn drop(&mut self) {
        let signal_all = |sig| {
            for pid in processes_under(&self.home) {
                // A pid always fits a pid_t.
                unsafe {
                    libc::kill(pid.cast_signed(), sig);
                }
            }
        };
        signal_all(libc::SIGTERM);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !processes_under(&self.home).is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        signal_all(libc::SIGKILL);
    }
}

/// A private `CLERK_HOME` with a daemon reaper. The reaper is declared
/// first so it runs before the tempdir goes away.
struct Harness {
    _reaper: Reaper,
    env: TestEnv,
    paths: Paths,
}

impl Harness {
    fn new() -> Self {
        let env = TestEnv::new();
        let home = env.home().to_path_buf();
        let lookup = home.clone();
        let paths = Paths::from_lookup(move |k| {
            (k == "CLERK_HOME").then(|| lookup.clone().into_os_string())
        })
        .unwrap();
        Self {
            _reaper: Reaper { home },
            env,
            paths,
        }
    }

    /// A harness whose config file sets the given TOML.
    fn with_config(toml: &str) -> Self {
        let h = Self::new();
        fs::create_dir_all(h.paths.config.parent().unwrap()).unwrap();
        fs::write(&h.paths.config, toml).unwrap();
        h
    }

    fn home(&self) -> &Path {
        self.env.home()
    }

    fn launcher(&self) -> Launcher {
        self.launcher_with(&[])
    }

    fn launcher_with(&self, extra: &[(&str, &str)]) -> Launcher {
        let mut env: Vec<(OsString, OsString)> =
            vec![("CLERK_HOME".into(), self.home().as_os_str().to_owned())];
        env.extend(extra.iter().map(|(k, v)| ((*k).into(), (*v).into())));
        Launcher {
            program: PathBuf::from(env!("CARGO_BIN_EXE_clerk")),
            env,
        }
    }

    async fn start(&self) -> Client {
        Client::ensure_running_with(&self.paths, &self.launcher())
            .await
            .expect("daemon starts")
    }

    fn daemons(&self) -> Vec<u32> {
        daemons_under(self.home())
    }

    /// Stop the daemon through `daemon_stop` and wait until it is gone.
    async fn stop(&self, client: &Client) {
        let pid = client.daemon().pid;
        client
            .call("daemon_stop", &user("admin"), json!({}))
            .await
            .expect("daemon_stop");
        let socket = self.paths.socket.clone();
        tokio::task::spawn_blocking(move || {
            until("the daemon exits", || !alive(pid));
            until("the socket is gone", || !socket.exists());
        })
        .await
        .unwrap();
    }
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

fn now_ms() -> i64 {
    SystemClock.now_ms()
}

async fn ok(client: &Client, method: &str, who: &Caller, args: Value) -> Value {
    client
        .call(method, who, args)
        .await
        .unwrap_or_else(|e| panic!("{method} failed: {e:?}"))
}

/// A user-scope generic resource `board` with one unit `u1`.
async fn seed_board(client: &Client) {
    let admin = user("admin");
    ok(
        client,
        "create_resource",
        &admin,
        json!({"id": "board", "type": "generic", "scope": "user"}),
    )
    .await;
    ok(
        client,
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

async fn acquire(client: &Client, who: &Caller, duration_ms: u64) -> Value {
    ok(
        client,
        "acquire",
        who,
        json!({"target": {"resource": "board"}, "duration_ms": duration_ms, "purpose": "test"}),
    )
    .await
}

/// The occupancy of `board/u1` in a status report.
async fn occupancy(client: &Client) -> Value {
    let status = ok(client, "status", &user("admin"), json!({})).await;
    status["resources"][0]["units"][0]["occupancy"].clone()
}

/// Long-poll `who` until a notice of `kind` arrives; returns it.
async fn notice_of(client: &Client, who: &HolderRef, kind: NoticeKind) -> Notice {
    let deadline = Instant::now() + PATIENCE;
    loop {
        assert!(Instant::now() < deadline, "no {kind:?} notice for {who:?}");
        let got = client.wait_notices(who, 5).await.expect("wait_notices");
        if let Some(n) = got.into_iter().find(|n| n.kind == kind) {
            return n;
        }
    }
}

async fn audit_actions(client: &Client, action: &str) -> Vec<Value> {
    let list = ok(
        client,
        "list_audit",
        &user("admin"),
        json!({"action": action}),
    )
    .await;
    list.as_array().unwrap().clone()
}

/// Sleep (real time) until the wall clock passes `at`.
async fn sleep_past(at: i64) {
    let left = at.saturating_sub(now_ms()) + 50;
    if left > 0 {
        tokio::time::sleep(Duration::from_millis(u64::try_from(left).unwrap())).await;
    }
}

// ---------------------------------------------------------------------------
// Daemon (BDD subtree)
// ---------------------------------------------------------------------------

mod daemon {
    use super::*;

    mod ensure_running {
        use super::*;

        mod on_daemon_running_and_same_version {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn connects_without_spawning() {
                let h = Harness::new();
                let first = h.start().await;
                let second = h.start().await;
                assert_eq!(first.daemon().pid, second.daemon().pid);
                assert_eq!(h.daemons(), vec![first.daemon().pid]);
            }
        }

        mod on_no_daemon {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn spawns_a_detached_daemon_that_answers() {
                let h = Harness::new();
                let client = h.start().await;
                let pid = client.daemon().pid;
                assert_eq!(h.daemons(), vec![pid]);
                // setsid: the daemon leads its own session.
                let sid = unsafe { libc::getsid(pid.cast_signed()) };
                assert_eq!(sid, pid.cast_signed());
                let status = ok(&client, "status", &user("a"), json!({})).await;
                assert_eq!(status, json!({"resources": []}));
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn connect_alone_reports_unavailable() {
                let h = Harness::new();
                let err = Client::connect(&h.paths).await.err().unwrap();
                assert!(matches!(err, DaemonError::Unavailable), "{err:?}");
                assert_eq!(h.daemons(), Vec::<u32>::new());
            }
        }

        mod on_concurrent_calls {
            use super::*;

            /// Eight threads, each with its own runtime, race to start the
            /// daemon. Returns the daemon pid each one connected to.
            fn race(h: &Harness) -> Vec<DaemonInfo> {
                let barrier = Arc::new(std::sync::Barrier::new(8));
                std::thread::scope(|s| {
                    let joins: Vec<_> = (0..8)
                        .map(|_| {
                            let barrier = barrier.clone();
                            s.spawn(move || {
                                let rt = tokio::runtime::Builder::new_current_thread()
                                    .enable_all()
                                    .build()
                                    .unwrap();
                                barrier.wait();
                                rt.block_on(async {
                                    let client =
                                        Client::ensure_running_with(&h.paths, &h.launcher())
                                            .await
                                            .expect("ensure_running");
                                    // The client works.
                                    ok(&client, "status", &user("a"), json!({})).await;
                                    client.daemon().clone()
                                })
                            })
                        })
                        .collect();
                    joins.into_iter().map(|j| j.join().unwrap()).collect()
                })
            }

            #[test]
            fn leaves_exactly_one_daemon_process() {
                let h = Harness::new();
                let infos = race(&h);
                // Losers exit at once; wait for them, then expect one left.
                until("only one daemon is left", || h.daemons().len() <= 1);
                assert_eq!(h.daemons(), vec![infos[0].pid]);
            }

            #[test]
            fn gives_every_caller_a_working_client_of_the_same_daemon() {
                let h = Harness::new();
                let infos = race(&h);
                assert_eq!(infos.len(), 8);
                assert!(infos.iter().all(|i| i.pid == infos[0].pid), "{infos:?}");
            }
        }

        /// A "daemon" that holds the lock and has bound the socket but never
        /// accepts or answers (a wedged or stopped process).
        mod on_daemon_that_never_answers {
            use super::*;

            use fs4::fs_std::FileExt;

            /// Hold the lock and a listening socket nobody serves.
            fn wedge(h: &Harness) -> (fs::File, StdUnixListener) {
                fs::create_dir_all(h.paths.socket.parent().unwrap()).unwrap();
                let mut lock = fs::File::create(&h.paths.lock).unwrap();
                assert!(FileExt::try_lock_exclusive(&lock).unwrap());
                // The pid record a real daemon writes; this process stands in.
                std::io::Write::write_all(
                    &mut lock,
                    format!("{}\n", std::process::id()).as_bytes(),
                )
                .unwrap();
                let listener = StdUnixListener::bind(&h.paths.socket).unwrap();
                (lock, listener)
            }

            fn unresponsive() -> DaemonError {
                DaemonError::Unresponsive {
                    pid: Some(std::process::id()),
                }
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn ensure_running_gives_up_with_unresponsive_within_six_seconds() {
                let h = Harness::new();
                let _wedged = wedge(&h);
                let started = Instant::now();
                let err = Client::ensure_running_with(&h.paths, &h.launcher())
                    .await
                    .err()
                    .unwrap();
                assert_eq!(err, unresponsive());
                assert!(
                    started.elapsed() < Duration::from_secs(6),
                    "{:?}",
                    started.elapsed()
                );
                assert_eq!(h.daemons(), Vec::<u32>::new());
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn connect_gives_up_after_the_probe_timeout() {
                let h = Harness::new();
                let _wedged = wedge(&h);
                let started = Instant::now();
                let err = Client::connect(&h.paths).await.err().unwrap();
                assert_eq!(err, unresponsive());
                assert!(
                    started.elapsed() < Duration::from_secs(2),
                    "{:?}",
                    started.elapsed()
                );
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn stop_reports_unresponsive_promptly() {
                let h = Harness::new();
                let _wedged = wedge(&h);
                let started = Instant::now();
                let stopped = Client::stop(&h.paths, &user("admin")).await;
                assert_eq!(stopped, Err(unresponsive()));
                assert!(
                    started.elapsed() < Duration::from_secs(2),
                    "{:?}",
                    started.elapsed()
                );
            }

            #[test]
            fn status_reports_unresponsive_and_exits_5() {
                let h = Harness::new();
                let _wedged = wedge(&h);
                let started = Instant::now();
                let out = h
                    .env
                    .cmd()
                    .args(["daemon", "status"])
                    .timeout(PATIENCE)
                    .output()
                    .unwrap();
                assert!(
                    started.elapsed() < Duration::from_secs(3),
                    "{:?}",
                    started.elapsed()
                );
                assert_eq!(out.status.code(), Some(5), "{out:?}");
                let body: Value = serde_json::from_slice(&out.stdout).unwrap();
                assert_eq!(body["error"]["code"], "daemon_unresponsive");
                let message = body["error"]["message"].as_str().unwrap();
                assert!(message.contains("unresponsive"), "{message}");
                assert!(
                    message.contains(&format!("pid {}", std::process::id())),
                    "{message}"
                );
                assert!(!message.contains("not running"), "{message}");
            }
        }

        mod on_daemon_that_never_comes_up {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn starts_at_most_two_daemons() {
                let h = Harness::new();
                // A "clerk" that records each start and exits at once.
                let starts = h.home().join("starts");
                let script = h.home().join("fake-clerk");
                fs::write(
                    &script,
                    format!("#!/bin/sh\necho \"$1\" >> '{}'\n", starts.display()),
                )
                .unwrap();
                fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
                let launcher = Launcher {
                    program: script,
                    env: Vec::new(),
                };
                let err = Client::ensure_running_with(&h.paths, &launcher)
                    .await
                    .err()
                    .unwrap();
                assert!(matches!(err, DaemonError::Unavailable), "{err:?}");
                // Each start appends before exiting; the child was reaped.
                assert_eq!(fs::read_to_string(&starts).unwrap(), "daemon\ndaemon\n");
            }
        }

        mod on_version_mismatch {
            use super::*;

            async fn other_version_daemon(h: &Harness) {
                let launcher = h.launcher_with(&[("CLERK_TEST_DAEMON_VERSION", "0.0.0-other")]);
                let err = Client::ensure_running_with(&h.paths, &launcher)
                    .await
                    .err()
                    .unwrap();
                assert!(
                    matches!(err, DaemonError::VersionMismatch { .. }),
                    "{err:?}"
                );
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn returns_version_mismatch_with_both_versions() {
                let h = Harness::new();
                other_version_daemon(&h).await;
                let err = Client::ensure_running_with(&h.paths, &h.launcher())
                    .await
                    .err()
                    .unwrap();
                match err {
                    DaemonError::VersionMismatch { client, daemon } => {
                        assert_eq!(client, leclerk::VERSION);
                        assert_eq!(daemon, "0.0.0-other");
                    }
                    other @ (DaemonError::Unavailable | DaemonError::Unresponsive { .. }) => {
                        panic!("expected a version mismatch, got {other:?}")
                    }
                }
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn does_not_spawn_another_daemon() {
                let h = Harness::new();
                other_version_daemon(&h).await;
                let before = h.daemons();
                let _ = Client::ensure_running_with(&h.paths, &h.launcher()).await;
                assert_eq!(before.len(), 1);
                assert_eq!(h.daemons(), before);
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn connect_reports_it_too() {
                let h = Harness::new();
                other_version_daemon(&h).await;
                let err = Client::connect(&h.paths).await.err().unwrap();
                assert!(
                    matches!(err, DaemonError::VersionMismatch { .. }),
                    "{err:?}"
                );
            }
        }
    }

    mod startup {
        use super::*;

        mod on_stale_socket_file_and_no_live_daemon {
            use super::*;

            /// A socket file nobody listens on.
            fn plant_stale_socket(h: &Harness) {
                fs::create_dir_all(h.paths.socket.parent().unwrap()).unwrap();
                drop(StdUnixListener::bind(&h.paths.socket).unwrap());
                assert!(h.paths.socket.exists());
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn removes_it_and_binds() {
                let h = Harness::new();
                plant_stale_socket(&h);
                let client = h.start().await;
                let status = ok(&client, "status", &user("a"), json!({})).await;
                assert_eq!(status, json!({"resources": []}));
                assert!(
                    fs::metadata(&h.paths.socket)
                        .unwrap()
                        .file_type()
                        .is_socket()
                );
            }
        }

        mod on_lock_taken {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn records_the_daemon_pid_in_the_lock_file() {
                let h = Harness::new();
                let client = h.start().await;
                let recorded = fs::read_to_string(&h.paths.lock).unwrap();
                assert_eq!(recorded.trim(), client.daemon().pid.to_string());
            }
        }

        mod on_lock_already_held {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn second_daemon_exits_zero_quietly() {
                let h = Harness::new();
                let client = h.start().await;
                let out = tokio::task::spawn_blocking({
                    let mut cmd = h.env.cmd();
                    move || cmd.arg("daemon").timeout(PATIENCE).output().unwrap()
                })
                .await
                .unwrap();
                assert!(out.status.success(), "{out:?}");
                assert!(out.stdout.is_empty(), "{out:?}");
                // The first daemon still serves.
                ok(&client, "status", &user("a"), json!({})).await;
                assert_eq!(h.daemons(), vec![client.daemon().pid]);
            }
        }

        mod permissions {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn socket_is_mode_0600() {
                let h = Harness::new();
                h.start().await;
                let mode = fs::metadata(&h.paths.socket).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o600);
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn run_dir_is_mode_0700() {
                let h = Harness::new();
                h.start().await;
                let run = h.paths.socket.parent().unwrap();
                let mode = fs::metadata(run).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o700);
            }
        }

        /// Review Focus 3: a held lease whose end passed while the daemon was
        /// down.
        mod on_held_lease_that_expired_during_downtime {
            use super::*;

            const DURATION_MS: u64 = 2_000;

            /// Start, grant `a` the unit for [`DURATION_MS`], queue `b`, stop
            /// the daemon, let the lease end pass, and start again. Returns
            /// the new client, a's lease id and the time just before the
            /// restart.
            async fn expire_while_down(h: &Harness) -> (Client, LeaseId, i64) {
                let client = h.start().await;
                seed_board(&client).await;
                let grant = acquire(&client, &user("a"), DURATION_MS).await;
                assert_eq!(grant["outcome"], "granted");
                let lease: LeaseId = serde_json::from_value(grant["lease_id"].clone()).unwrap();
                let ends_at = grant["ends_at"].as_i64().unwrap();
                assert_eq!(
                    acquire(&client, &user("b"), 60_000).await["outcome"],
                    "queued"
                );
                h.stop(&client).await;
                assert!(
                    now_ms() < ends_at,
                    "the lease ended before the daemon stopped"
                );
                sleep_past(ends_at).await;
                let before = now_ms();
                let client = h.start().await;
                // The expiry was processed by the restarted daemon.
                let expired = audit_actions(&client, "expired").await;
                assert_eq!(expired.len(), 1);
                assert!(expired[0]["at"].as_i64().unwrap() >= before);
                (client, lease, before)
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn lease_is_cleaning_with_a_fresh_window_from_startup() {
                let h = Harness::with_config("cleanup_window = 60\n");
                let (client, lease, before) = expire_while_down(&h).await;
                let after = now_ms();
                let occ = occupancy(&client).await;
                assert_eq!(occ["lease_id"], json!(lease));
                assert_eq!(occ["state"], "cleaning");
                let started =
                    notice_of(&client, &holder("a"), NoticeKind::CleanupWindowStarted).await;
                let deadline = started.deadline.unwrap();
                assert!(deadline >= before + 60_000, "{deadline} vs start {before}");
                assert!(deadline <= after + 60_000, "{deadline} vs start {after}");
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn unit_is_offered_to_the_waiter_once_the_window_passes() {
                let h = Harness::with_config("cleanup_window = 2\n");
                let (client, _lease, before) = expire_while_down(&h).await;
                let offered = notice_of(&client, &holder("b"), NoticeKind::Offered).await;
                // Not before the fresh window from startup ran out.
                assert!(offered.created_at >= before + 2_000);
                assert_eq!(offered.unit.unwrap().as_str(), "u1");
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn unit_is_offered_to_the_waiter_when_the_holder_confirms() {
                let h = Harness::with_config("cleanup_window = 60\n");
                let (client, lease, _before) = expire_while_down(&h).await;
                ok(
                    &client,
                    "confirm",
                    &user("a"),
                    json!({"lease_id": lease, "how": "clean"}),
                )
                .await;
                let offered = notice_of(&client, &holder("b"), NoticeKind::Offered).await;
                assert_eq!(offered.unit.unwrap().as_str(), "u1");
                assert!(offered.dirty.is_none());
            }
        }

        /// The restart grace (spec §7.2): a cleaning lease whose window
        /// passed while the daemon was down.
        mod on_cleaning_lease_past_its_deadline_during_downtime {
            use super::*;

            const WINDOW_S: i64 = 4;

            async fn clean_past_deadline(h: &Harness) -> (Client, LeaseId, i64) {
                let client = h.start().await;
                seed_board(&client).await;
                let grant = acquire(&client, &user("a"), 60_000).await;
                let lease: LeaseId = serde_json::from_value(grant["lease_id"].clone()).unwrap();
                let released = ok(&client, "release", &user("a"), json!({"lease_id": lease})).await;
                let deadline = released["cleanup_deadline"].as_i64().unwrap();
                h.stop(&client).await;
                assert!(
                    now_ms() < deadline,
                    "the window ended before the daemon stopped"
                );
                sleep_past(deadline).await;
                let before = now_ms();
                let client = h.start().await;
                (client, lease, before)
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn lease_gets_a_fresh_cleanup_window_from_startup() {
                let h = Harness::with_config(&format!("cleanup_window = {WINDOW_S}\n"));
                let (client, lease, before) = clean_past_deadline(&h).await;
                let grace = audit_actions(&client, "restart_grace").await;
                assert_eq!(grace.len(), 1, "{grace:?}");
                assert_eq!(grace[0]["detail"]["lease"], json!(lease));
                let deadline = grace[0]["detail"]["cleanup_deadline"].as_i64().unwrap();
                assert!(
                    deadline >= before + WINDOW_S * 1_000,
                    "{deadline} vs {before}"
                );
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn holder_is_told_the_new_deadline() {
                let h = Harness::with_config(&format!("cleanup_window = {WINDOW_S}\n"));
                let (client, lease, _before) = clean_past_deadline(&h).await;
                let grace = audit_actions(&client, "restart_grace").await;
                let deadline = grace[0]["detail"]["cleanup_deadline"].as_i64().unwrap();
                let mut started = Vec::new();
                let patience = Instant::now() + PATIENCE;
                while started.len() < 2 {
                    assert!(Instant::now() < patience, "got only {started:?}");
                    let got = client.wait_notices(&holder("a"), 5).await.unwrap();
                    started.extend(
                        got.into_iter()
                            .filter(|n| n.kind == NoticeKind::CleanupWindowStarted),
                    );
                }
                // The release's notice, then the grace's with the new deadline.
                assert_eq!(started.len(), 2, "{started:?}");
                assert_eq!(started[1].lease_id.as_ref(), Some(&lease));
                assert_eq!(started[1].deadline, Some(deadline));
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn holder_can_still_confirm() {
                let h = Harness::with_config(&format!("cleanup_window = {WINDOW_S}\n"));
                let (client, lease, _before) = clean_past_deadline(&h).await;
                // Served before the fresh window ends: the lease is still
                // cleaning, so the confirm is accepted and the unit is clean.
                ok(
                    &client,
                    "confirm",
                    &user("a"),
                    json!({"lease_id": lease, "how": "clean"}),
                )
                .await;
                let status = ok(&client, "status", &user("admin"), json!({})).await;
                let unit = &status["resources"][0]["units"][0];
                assert_eq!(unit["occupancy"]["kind"], "free");
                assert!(unit["dirty"].is_null(), "{unit}");
            }
        }
    }

    mod shutdown {
        use super::*;

        mod on_no_daemon {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn stop_reports_not_running() {
                let h = Harness::new();
                assert_eq!(Client::stop(&h.paths, &user("admin")).await, Ok(None));
            }
        }

        mod on_daemon_stop {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn removes_the_socket_and_exits() {
                let h = Harness::new();
                let client = h.start().await;
                h.stop(&client).await;
                assert_eq!(h.daemons(), Vec::<u32>::new());
                assert!(!h.paths.socket.exists());
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn releases_the_lock_so_a_new_daemon_starts() {
                let h = Harness::new();
                let client = h.start().await;
                let old = client.daemon().pid;
                h.stop(&client).await;
                let client = h.start().await;
                assert_ne!(client.daemon().pid, old);
                assert_eq!(h.daemons(), vec![client.daemon().pid]);
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn wakes_a_parked_long_poll() {
                let h = Harness::new();
                let client = h.start().await;
                let poller = Client::connect(&h.paths).await.unwrap();
                let wait =
                    tokio::spawn(async move { poller.wait_notices(&holder("idle"), 60).await });
                // Give the long-poll a moment to park; the stop must wake it
                // whether or not it has parked yet.
                tokio::time::sleep(Duration::from_millis(100)).await;
                let started = Instant::now();
                h.stop(&client).await;
                let outcome = tokio::time::timeout(PATIENCE, wait).await.unwrap().unwrap();
                assert!(started.elapsed() < Duration::from_secs(20));
                match outcome {
                    Ok(notices) => assert_eq!(notices, Vec::new()),
                    Err(e) => assert!(
                        ["engine_unavailable", "daemon_unavailable"].contains(&e.code.as_str()),
                        "{e:?}"
                    ),
                }
            }
        }

        mod on_sigterm {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn removes_the_socket_and_exits() {
                let h = Harness::new();
                let client = h.start().await;
                let pid = client.daemon().pid;
                unsafe {
                    libc::kill(pid.cast_signed(), libc::SIGTERM);
                }
                let socket = h.paths.socket.clone();
                tokio::task::spawn_blocking(move || {
                    until("the daemon exits", || !alive(pid));
                    until("the socket is gone", || !socket.exists());
                })
                .await
                .unwrap();
            }
        }
    }

    mod client {
        use super::*;

        mod call {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn returns_the_api_error_code_and_message() {
                let h = Harness::new();
                let client = h.start().await;
                let err = client
                    .call("get_resource", &user("a"), json!({"id": "nope"}))
                    .await
                    .unwrap_err();
                assert_eq!(err.code, "not_found");
                assert_ne!(err.message, "");
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn reports_daemon_unavailable_once_the_daemon_is_gone() {
                let h = Harness::new();
                let client = h.start().await;
                h.stop(&client).await;
                let err = client
                    .call("status", &user("a"), json!({}))
                    .await
                    .unwrap_err();
                assert_eq!(err.code, "daemon_unavailable");
            }
        }

        mod wait_notices {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn returns_pending_notices() {
                let h = Harness::new();
                let client = h.start().await;
                seed_board(&client).await;
                let grant = acquire(&client, &user("a"), 60_000).await;
                ok(
                    &client,
                    "release",
                    &user("a"),
                    json!({"lease_id": grant["lease_id"]}),
                )
                .await;
                let got = client.wait_notices(&holder("a"), 5).await.unwrap();
                assert!(
                    got.iter()
                        .any(|n| n.kind == NoticeKind::CleanupWindowStarted)
                );
            }
        }
    }

    mod cli {
        use super::*;

        fn run(h: &Harness, args: &[&str]) -> std::process::Output {
            h.env
                .cmd()
                .arg("daemon")
                .args(args)
                .timeout(PATIENCE)
                .output()
                .unwrap()
        }

        mod status {
            use super::*;

            #[test]
            fn reports_not_running_without_starting_one() {
                let h = Harness::new();
                let out = run(&h, &["status"]);
                assert!(out.status.success(), "{out:?}");
                let body: Value = serde_json::from_slice(&out.stdout).unwrap();
                assert_eq!(body, json!({"running": false}));
                assert_eq!(h.daemons(), Vec::<u32>::new());
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn reports_pid_and_version_of_a_running_daemon() {
                let h = Harness::new();
                let client = h.start().await;
                let out = tokio::task::spawn_blocking({
                    let mut cmd = h.env.cmd();
                    move || {
                        cmd.args(["daemon", "status"])
                            .timeout(PATIENCE)
                            .output()
                            .unwrap()
                    }
                })
                .await
                .unwrap();
                assert!(out.status.success(), "{out:?}");
                let body: Value = serde_json::from_slice(&out.stdout).unwrap();
                assert_eq!(
                    body,
                    json!({
                        "running": true,
                        "pid": client.daemon().pid,
                        "version": leclerk::VERSION,
                    })
                );
            }
        }

        mod stop {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn stops_the_daemon() {
                let h = Harness::new();
                let client = h.start().await;
                let pid = client.daemon().pid;
                let out = tokio::task::spawn_blocking({
                    let mut cmd = h.env.cmd();
                    move || {
                        cmd.args(["daemon", "stop"])
                            .timeout(PATIENCE)
                            .output()
                            .unwrap()
                    }
                })
                .await
                .unwrap();
                assert!(out.status.success(), "{out:?}");
                let socket = h.paths.socket.clone();
                tokio::task::spawn_blocking(move || {
                    until("the daemon exits", || !alive(pid));
                    until("the socket is gone", || !socket.exists());
                })
                .await
                .unwrap();
            }
        }

        mod restart {
            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            async fn replaces_the_running_daemon() {
                let h = Harness::new();
                let client = h.start().await;
                let old = client.daemon().pid;
                let out = tokio::task::spawn_blocking({
                    let mut cmd = h.env.cmd();
                    move || {
                        cmd.args(["daemon", "restart"])
                            .timeout(PATIENCE)
                            .output()
                            .unwrap()
                    }
                })
                .await
                .unwrap();
                assert!(out.status.success(), "{out:?}");
                let fresh = Client::connect(&h.paths).await.unwrap();
                assert_ne!(fresh.daemon().pid, old);
                assert_eq!(h.daemons(), vec![fresh.daemon().pid]);
            }

            #[test]
            fn starts_one_when_none_runs() {
                let h = Harness::new();
                let out = run(&h, &["restart"]);
                assert!(out.status.success(), "{out:?}");
                until("one daemon runs", || h.daemons().len() == 1);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Restart grace (spec §7.2), against a ManualClock
// ---------------------------------------------------------------------------

mod restart_grace {
    use super::*;

    const T0: i64 = 1_000_000;
    const SEC: i64 = 1_000;

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
        sched: Scheduler,
        clock: Arc<ManualClock>,
        config: Config,
    }

    fn fixture() -> Fixture {
        let clock = Arc::new(ManualClock::new(T0));
        let config = Config::default();
        let mut store = Store::open_in_memory().unwrap();
        let inv = Inventory::new(clock.clone());
        let admin = user("admin");
        store
            .tx(|tx| -> Result<(), leclerk::inventory::InventoryError> {
                inv.create_resource(
                    tx,
                    &admin,
                    &NewResource {
                        id: ResourceId::parse("board").unwrap(),
                        ty: ResourceType::Generic,
                        scope: Scope::User,
                        description: String::new(),
                        notes: String::new(),
                        health: None,
                    },
                )?;
                inv.add_unit(
                    tx,
                    &admin,
                    &ResourceId::parse("board").unwrap(),
                    &NewUnit {
                        id: UnitId::parse("u1").unwrap(),
                        settings: UnitSettings::Generic {
                            identity_key: "key-u1".into(),
                        },
                        notes: String::new(),
                    },
                )?;
                Ok(())
            })
            .unwrap();
        Fixture {
            sched: Scheduler::new(clock.clone(), config.clone(), Arc::new(Healthy)),
            store,
            clock,
            config,
        }
    }

    fn request(minutes: u64) -> AcquireRequest {
        AcquireRequest {
            target: Target::Resource(ResourceId::parse("board").unwrap()),
            duration: Duration::from_secs(minutes * 60),
            interruptible: false,
            priority: Priority::Normal,
            purpose: "test".into(),
            override_reason: None,
        }
    }

    impl Fixture {
        fn with_tx<T>(&mut self, f: impl FnOnce(&Tx, &Scheduler) -> T) -> T {
            let sched = &self.sched;
            self.store
                .tx(|tx| -> Result<T, StoreError> { Ok(f(tx, sched)) })
                .unwrap()
        }

        fn grant(&mut self, who: &str) -> LeaseId {
            let who = user(who);
            match self.with_tx(|tx, s| s.acquire(tx, &who, request(10)).unwrap()) {
                AcquireOutcome::Granted(g) => g.lease_id,
                other => panic!("expected grant, got {other:?}"),
            }
        }

        fn grace(&mut self) -> usize {
            self.with_tx(|tx, s| s.restart_grace(tx).unwrap())
        }

        fn column(&mut self, lease: &LeaseId, col: &str) -> Option<i64> {
            self.with_tx(|tx, _| {
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
            self.with_tx(|tx, _| {
                tx.conn()
                    .query_row(
                        "SELECT state FROM leases WHERE lease_id = ?1",
                        [lease.as_str()],
                        |r| r.get(0),
                    )
                    .unwrap()
            })
        }

        fn notices(&mut self, who: &str, kind: NoticeKind) -> Vec<Notice> {
            let h = holder(who);
            self.with_tx(|tx, _| leclerk::store::notices::pending(tx, &h).unwrap())
                .into_iter()
                .filter(|n| n.kind == kind)
                .collect()
        }

        fn grace_audits(&mut self) -> Vec<audit::AuditEntry> {
            self.with_tx(|tx, _| audit::all(tx).unwrap())
                .into_iter()
                .filter(|e| e.action == "restart_grace")
                .collect()
        }

        /// A lease of `a` in cleaning, its window ending at T0 + window.
        fn cleaning(&mut self) -> LeaseId {
            let lease = self.grant("a");
            let a = user("a");
            self.with_tx(|tx, s| s.release(tx, &a, &lease).unwrap());
            lease
        }

        /// `b`'s offered lease, its claim window ending at T0 + window.
        fn offered(&mut self) -> LeaseId {
            let lease = self.grant("a");
            let b = user("b");
            match self.with_tx(|tx, s| s.acquire(tx, &b, request(10)).unwrap()) {
                AcquireOutcome::Queued { .. } => {}
                other => panic!("expected queued, got {other:?}"),
            }
            let a = user("a");
            self.with_tx(|tx, s| {
                s.release(tx, &a, &lease).unwrap();
                s.confirm(tx, &a, &lease, Confirm::Clean).unwrap();
                s.dispatch(tx).unwrap();
            });
            let id: String = self.with_tx(|tx, _| {
                tx.conn()
                    .query_row(
                        "SELECT lease_id FROM leases WHERE state = 'offered'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap()
            });
            serde_json::from_value(json!(id)).unwrap()
        }

        fn cleanup_ms(&self) -> i64 {
            i64::try_from(self.config.cleanup_window).unwrap() * SEC
        }

        fn claim_ms(&self) -> i64 {
            i64::try_from(self.config.claim_window).unwrap() * SEC
        }
    }

    mod on_cleaning_lease_past_its_deadline {
        use super::*;

        fn setup() -> (Fixture, LeaseId, i64) {
            let mut f = fixture();
            let lease = f.cleaning();
            let startup = T0 + f.cleanup_ms() + 30 * SEC;
            f.clock.set(startup);
            (f, lease, startup)
        }

        #[test]
        fn moves_the_deadline_to_startup_plus_the_cleanup_window() {
            let (mut f, lease, startup) = setup();
            assert_eq!(f.grace(), 1);
            assert_eq!(
                f.column(&lease, "cleanup_deadline"),
                Some(startup + f.cleanup_ms())
            );
            assert_eq!(f.state(&lease), "cleaning");
        }

        #[test]
        fn audits_restart_grace() {
            let (mut f, lease, startup) = setup();
            f.grace();
            let entries = f.grace_audits();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].at, startup);
            assert_eq!(entries[0].detail["lease"], json!(lease));
            assert_eq!(
                entries[0].detail["cleanup_deadline"],
                json!(startup + f.cleanup_ms())
            );
        }

        #[test]
        fn tells_the_holder_the_new_deadline() {
            let (mut f, lease, startup) = setup();
            f.grace();
            let started = f.notices("a", NoticeKind::CleanupWindowStarted);
            assert_eq!(started.len(), 2, "{started:?}");
            let fresh = &started[1];
            assert_eq!(fresh.lease_id.as_ref(), Some(&lease));
            assert_eq!(fresh.deadline, Some(startup + f.cleanup_ms()));
            assert_eq!(fresh.created_at, startup);
        }

        #[test]
        fn the_next_tick_leaves_it_cleaning() {
            let (mut f, lease, _) = setup();
            f.grace();
            f.with_tx(|tx, s| s.tick(tx).unwrap());
            assert_eq!(f.state(&lease), "cleaning");
        }
    }

    mod on_offered_lease_past_its_claim_deadline {
        use super::*;

        fn setup() -> (Fixture, LeaseId, i64) {
            let mut f = fixture();
            let lease = f.offered();
            let startup = T0 + f.claim_ms() + 30 * SEC;
            f.clock.set(startup);
            (f, lease, startup)
        }

        #[test]
        fn moves_the_claim_deadline_to_startup_plus_the_claim_window() {
            let (mut f, lease, startup) = setup();
            assert_eq!(f.grace(), 1);
            assert_eq!(
                f.column(&lease, "claim_deadline"),
                Some(startup + f.claim_ms())
            );
            assert_eq!(f.state(&lease), "offered");
        }

        #[test]
        fn audits_restart_grace() {
            let (mut f, lease, startup) = setup();
            f.grace();
            let entries = f.grace_audits();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].at, startup);
            assert_eq!(entries[0].detail["lease"], json!(lease));
            assert_eq!(
                entries[0].detail["claim_deadline"],
                json!(startup + f.claim_ms())
            );
        }

        #[test]
        fn tells_the_holder_the_new_deadline() {
            let (mut f, lease, startup) = setup();
            f.grace();
            let offered = f.notices("b", NoticeKind::Offered);
            assert_eq!(offered.len(), 2, "{offered:?}");
            let fresh = &offered[1];
            assert_eq!(fresh.lease_id.as_ref(), Some(&lease));
            assert_eq!(fresh.deadline, Some(startup + f.claim_ms()));
            assert_eq!(fresh.unit.as_ref().map(UnitId::as_str), Some("u1"));
        }

        #[test]
        fn the_offer_can_still_be_claimed() {
            let (mut f, lease, _) = setup();
            f.grace();
            f.with_tx(|tx, s| s.tick(tx).unwrap());
            let b = user("b");
            f.with_tx(|tx, s| s.claim(tx, &b, &lease).unwrap());
            assert_eq!(f.state(&lease), "held");
        }
    }

    mod on_held_lease_past_its_end {
        use super::*;

        #[test]
        fn is_left_alone() {
            let mut f = fixture();
            let lease = f.grant("a");
            let ends_at = f.column(&lease, "ends_at");
            f.clock.set(T0 + 20 * 60 * SEC);
            assert_eq!(f.grace(), 0);
            assert_eq!(f.column(&lease, "ends_at"), ends_at);
            assert_eq!(f.grace_audits().len(), 0);
        }

        #[test]
        fn first_tick_expires_it_with_a_full_window_from_now() {
            let mut f = fixture();
            let lease = f.grant("a");
            let startup = T0 + 20 * 60 * SEC;
            f.clock.set(startup);
            f.grace();
            f.with_tx(|tx, s| s.tick(tx).unwrap());
            assert_eq!(f.state(&lease), "cleaning");
            assert_eq!(
                f.column(&lease, "cleanup_deadline"),
                Some(startup + f.cleanup_ms())
            );
        }
    }

    mod on_deadlines_not_yet_passed {
        use super::*;

        #[test]
        fn leaves_cleaning_and_offered_leases_alone() {
            let mut f = fixture();
            let cleaning = f.cleaning();
            let before = f.column(&cleaning, "cleanup_deadline");
            f.clock.set(T0 + SEC);
            assert_eq!(f.grace(), 0);
            assert_eq!(f.column(&cleaning, "cleanup_deadline"), before);
            assert_eq!(f.grace_audits().len(), 0);
        }

        #[test]
        fn leaves_an_offer_whose_deadline_is_now_alone() {
            let mut f = fixture();
            let lease = f.offered();
            let deadline = f.column(&lease, "claim_deadline").unwrap();
            // The claim deadline is inclusive: still claimable at that instant.
            f.clock.set(deadline);
            assert_eq!(f.grace(), 0);
            assert_eq!(f.column(&lease, "claim_deadline"), Some(deadline));
        }
    }
}
