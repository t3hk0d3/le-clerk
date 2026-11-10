//! Behaviour tests for health checks.

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use leclerk::health::{self, HealthChecker, HealthResult};
use leclerk::model::{
    BuiltinCheck, Check, HealthSpec, ResourceId, ServiceMode, UnitId, UnitSettings,
};
use leclerk::scheduler::HealthProbe;
use leclerk::scheduler::checks::{CHECK_CONCURRENCY, CheckJob, CheckPool, CheckTicket};

fn checker() -> HealthChecker {
    HealthChecker::new(Duration::from_secs(1))
}

fn spec(checks: Vec<Check>) -> HealthSpec {
    HealthSpec { checks }
}

fn builtin(c: BuiltinCheck) -> Check {
    Check::Builtin(c)
}

fn custom(cmd: &str) -> Check {
    Check::Custom {
        command: cmd.to_owned(),
    }
}

fn run(checks: Vec<Check>, settings: &UnitSettings) -> HealthResult {
    checker().check(&spec(checks), settings)
}

fn device(path: &Path) -> UnitSettings {
    UnitSettings::Device {
        path: path.to_path_buf(),
    }
}

fn service(port: u16, scheme: Option<&str>) -> UnitSettings {
    UnitSettings::Service {
        mode: ServiceMode::Connect,
        scheme: scheme.map(str::to_owned),
        host: "127.0.0.1".into(),
        port,
        service_id: None,
    }
}

fn unhealthy(r: HealthResult) -> String {
    match r {
        HealthResult::Unhealthy(m) => m,
        HealthResult::Healthy => panic!("expected Unhealthy"),
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

mod device_exists {
    use super::*;

    #[test]
    fn healthy_when_path_exists() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let r = run(vec![builtin(BuiltinCheck::Exists)], &device(f.path()));
        assert_eq!(r, HealthResult::Healthy);
    }

    #[test]
    fn unhealthy_when_path_missing() {
        let d = tempfile::tempdir().unwrap();
        let r = run(
            vec![builtin(BuiltinCheck::Exists)],
            &device(&d.path().join("nope")),
        );
        assert_eq!(unhealthy(r), "path missing");
    }
}

mod device_accessible {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn healthy_when_openable_read_write() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let r = run(vec![builtin(BuiltinCheck::Accessible)], &device(f.path()));
        assert_eq!(r, HealthResult::Healthy);
    }

    fn euid_is_root() -> bool {
        // SAFETY: geteuid has no preconditions.
        unsafe { libc::geteuid() == 0 }
    }

    #[test]
    fn unhealthy_when_not_openable() {
        if euid_is_root() {
            eprintln!("SKIPPED: root bypasses file permissions");
            return;
        }
        let f = tempfile::NamedTempFile::new().unwrap();
        fs::set_permissions(f.path(), fs::Permissions::from_mode(0o400)).unwrap();
        let r = run(vec![builtin(BuiltinCheck::Accessible)], &device(f.path()));
        assert!(unhealthy(r).starts_with("not accessible"));
    }

    #[test]
    fn healthy_for_fifo_without_reader_proving_no_open() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("fifo");
        let c = CString::new(p.as_os_str().as_bytes()).unwrap();
        // SAFETY: valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        // open(O_WRONLY|O_NONBLOCK) would fail with ENXIO here.
        let r = run(vec![builtin(BuiltinCheck::Accessible)], &device(&p));
        assert_eq!(r, HealthResult::Healthy);
    }
}

mod service_tcp_connect {
    use super::*;

    #[test]
    fn healthy_with_listening_port() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let r = run(
            vec![builtin(BuiltinCheck::TcpConnect)],
            &service(port, None),
        );
        assert_eq!(r, HealthResult::Healthy);
    }

    #[test]
    fn gives_up_on_an_unroutable_address_within_the_timeout() {
        let blackhole = UnitSettings::Service {
            mode: ServiceMode::Connect,
            scheme: None,
            // TEST-NET-1: never routed, so the connect hangs until timed out.
            host: "192.0.2.1".into(),
            port: 9,
            service_id: None,
        };
        let started = Instant::now();
        let r = run(vec![builtin(BuiltinCheck::TcpConnect)], &blackhole);
        assert_eq!(unhealthy(r), "connect failed");
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn unhealthy_with_nothing_listening() {
        let r = run(
            vec![builtin(BuiltinCheck::TcpConnect)],
            &service(free_port(), None),
        );
        assert_eq!(unhealthy(r), "connect failed");
    }
}

mod service_port_free {
    use super::*;

    #[test]
    fn healthy_when_port_unbound() {
        let r = run(
            vec![builtin(BuiltinCheck::PortFree)],
            &service(free_port(), None),
        );
        assert_eq!(r, HealthResult::Healthy);
    }

    #[test]
    fn unhealthy_when_port_bound() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let r = run(vec![builtin(BuiltinCheck::PortFree)], &service(port, None));
        assert_eq!(unhealthy(r), "port in use");
    }
}

mod service_http {
    use super::*;
    use std::convert::Infallible;

    use bytes::Bytes;
    use http_body_util::Full;
    use hyper::service::service_fn;
    use hyper::{Response, StatusCode};
    use hyper_util::rt::TokioIo;

    /// Serve fixed `status` on a port-0 listener from a background thread.
    fn serve(status: StatusCode) -> u16 {
        let std_l = TcpListener::bind("127.0.0.1:0").unwrap();
        std_l.set_nonblocking(true).unwrap();
        let port = std_l.local_addr().unwrap().port();
        thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let l = tokio::net::TcpListener::from_std(std_l).unwrap();
                loop {
                    let (s, _) = l.accept().await.unwrap();
                    tokio::spawn(async move {
                        let svc = service_fn(move |_req| async move {
                            Ok::<_, Infallible>(
                                Response::builder()
                                    .status(status)
                                    .body(Full::new(Bytes::from_static(b"hi")))
                                    .unwrap(),
                            )
                        });
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(s), svc)
                            .await;
                    });
                }
            });
        });
        port
    }

    #[test]
    fn healthy_on_2xx() {
        let port = serve(StatusCode::OK);
        let r = run(vec![builtin(BuiltinCheck::Http)], &service(port, None));
        assert_eq!(r, HealthResult::Healthy);
    }

    #[test]
    fn unhealthy_with_status_on_non_2xx() {
        let port = serve(StatusCode::SERVICE_UNAVAILABLE);
        let r = run(vec![builtin(BuiltinCheck::Http)], &service(port, None));
        assert!(unhealthy(r).contains("503"));
    }

    #[test]
    fn unhealthy_with_error_when_no_answer() {
        let r = run(
            vec![builtin(BuiltinCheck::Http)],
            &service(free_port(), None),
        );
        assert!(unhealthy(r).starts_with("http error"));
    }

    #[test]
    fn unhealthy_for_https_scheme() {
        let port = serve(StatusCode::OK);
        let r = run(
            vec![builtin(BuiltinCheck::Http)],
            &service(port, Some("https")),
        );
        assert_eq!(unhealthy(r), "https not supported");
    }
}

mod device_not_open {
    use super::*;

    fn holder(path: &Path) -> Child {
        Command::new("sh")
            .arg("-c")
            .arg("exec 3<\"$1\"; echo ready; sleep 30")
            .arg("sh")
            .arg(path)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn wait_ready(child: &mut Child) {
        use std::io::Read;
        let mut byte = [0u8; 1];
        child
            .stdout
            .as_mut()
            .unwrap()
            .read_exact(&mut byte)
            .unwrap();
    }

    #[test]
    fn unhealthy_naming_pid_while_held_then_healthy_after() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let mut child = holder(f.path());
        wait_ready(&mut child);
        let s = device(f.path());

        let msg = unhealthy(run(vec![builtin(BuiltinCheck::NotOpen)], &s));
        assert!(
            msg.starts_with(&format!("open by pid {} (", child.id())),
            "{msg}"
        );

        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(
            run(vec![builtin(BuiltinCheck::NotOpen)], &s),
            HealthResult::Healthy
        );
    }

    #[test]
    fn healthy_when_nobody_holds_it() {
        let f = tempfile::NamedTempFile::new().unwrap();
        // This test process itself holds `f` open; the check must ignore it.
        let r = run(vec![builtin(BuiltinCheck::NotOpen)], &device(f.path()));
        assert_eq!(r, HealthResult::Healthy);
    }
}

mod none_and_lists {
    use super::*;

    #[test]
    fn none_is_healthy() {
        let s = UnitSettings::Generic {
            identity_key: "k".into(),
        };
        assert_eq!(
            run(vec![builtin(BuiltinCheck::None)], &s),
            HealthResult::Healthy
        );
    }

    #[test]
    fn list_stops_at_first_failure() {
        let d = tempfile::tempdir().unwrap();
        let r = run(
            vec![
                builtin(BuiltinCheck::Exists),
                builtin(BuiltinCheck::NotOpen),
                custom("touch \"$CLERK_UNIT_PATH.ran\""),
            ],
            &device(&d.path().join("nope")),
        );
        assert_eq!(unhealthy(r), "path missing");
        assert!(!d.path().join("nope.ran").exists());
    }

    #[test]
    fn list_is_healthy_when_every_entry_passes() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let r = run(
            vec![
                builtin(BuiltinCheck::Exists),
                builtin(BuiltinCheck::NotOpen),
                custom("true"),
            ],
            &device(f.path()),
        );
        assert_eq!(r, HealthResult::Healthy);
    }
}

mod custom_command {
    use super::*;

    #[test]
    fn exit_zero_is_healthy_and_env_not_argv_carries_settings() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let argv_out = dir.path().join("argv");
        let dev = dir.path().join("$(touch pwned)");
        let cmd = format!(
            "printf '%s' \"$CLERK_UNIT_PATH\" > {out}; \
             printf '%s|%s' \"$CLERK_RESOURCE\" \"$CLERK_UNIT\" > {out}.ids; \
             tr '\\0' ' ' < /proc/$$/cmdline > {argv}",
            out = out.display(),
            argv = argv_out.display()
        );
        let r = checker().check_unit(
            &spec(vec![custom(&cmd)]),
            &ResourceId::parse("esp32").unwrap(),
            &UnitId::parse("u1").unwrap(),
            &device(&dev),
        );
        assert_eq!(r, HealthResult::Healthy);
        assert_eq!(fs::read_to_string(&out).unwrap(), dev.display().to_string());
        assert_eq!(
            fs::read_to_string(dir.path().join("out.ids")).unwrap(),
            "esp32|u1"
        );
        assert!(
            !fs::read_to_string(&argv_out)
                .unwrap()
                .contains(&dev.display().to_string())
        );
    }

    #[test]
    fn settings_are_never_executed() {
        let dir = tempfile::tempdir().unwrap();
        let pwned = dir.path().join("pwned");
        let key = format!("$(touch {})", pwned.display());
        let s = UnitSettings::Generic { identity_key: key };
        let r = run(vec![custom("test -n \"$CLERK_UNIT_IDENTITY_KEY\"")], &s);
        assert_eq!(r, HealthResult::Healthy);
        let dev = device(&PathBuf::from(format!("`touch {}`", pwned.display())));
        let r = run(vec![custom("echo \"$CLERK_UNIT_PATH\" >/dev/null")], &dev);
        assert_eq!(r, HealthResult::Healthy);
        assert!(!pwned.exists());
    }

    #[test]
    fn nonzero_exit_is_unhealthy_with_stderr() {
        let s = UnitSettings::Generic {
            identity_key: "k".into(),
        };
        let msg = unhealthy(run(vec![custom("echo boom >&2; exit 3")], &s));
        assert!(msg.contains("boom"), "{msg}");
    }

    #[test]
    fn stderr_is_truncated_to_1_kib() {
        let s = UnitSettings::Generic {
            identity_key: "k".into(),
        };
        let msg = unhealthy(run(
            vec![custom("head -c 5000 /dev/zero | tr '\\0' x >&2; exit 1")],
            &s,
        ));
        assert!(msg.matches('x').count() == 1024, "{}", msg.len());
    }

    #[test]
    fn stdin_is_null() {
        let s = UnitSettings::Generic {
            identity_key: "k".into(),
        };
        let r = run(vec![custom("cat >/dev/null")], &s);
        assert_eq!(r, HealthResult::Healthy);
    }

    #[test]
    fn exceeding_timeout_is_unhealthy_and_child_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let s = UnitSettings::Generic {
            identity_key: "k".into(),
        };
        let cmd = format!("echo $$ > {}; sleep 30", pidfile.display());
        let start = Instant::now();
        let msg = unhealthy(run(vec![custom(&cmd)], &s));
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_eq!(msg, "timeout");
        let pid = fs::read_to_string(&pidfile).unwrap().trim().to_owned();
        let proc = PathBuf::from("/proc").join(pid);
        let deadline = Instant::now() + Duration::from_secs(2);
        while proc.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!proc.exists(), "child still alive");
    }
}

mod http_check {
    use super::*;

    #[test]
    fn does_not_build_a_runtime_per_call() {
        let port = free_port();
        for _ in 0..3 {
            let r = run(vec![builtin(BuiltinCheck::Http)], &service(port, None));
            assert_ne!(r, HealthResult::Healthy);
        }
        assert_eq!(health::runtimes_built(), 1);
    }
}

/// A job for unit row `row` with one custom check.
fn job(row: i64) -> CheckJob {
    CheckJob {
        ticket: CheckTicket { row, token: 1 },
        spec: spec(vec![custom("unused")]),
        resource: ResourceId::parse("board").unwrap(),
        unit: UnitId::parse("u1").unwrap(),
        settings: UnitSettings::Generic {
            identity_key: "k".into(),
        },
    }
}

/// A probe that does not answer until the test ends (the sender drops).
struct Hanging {
    release: Mutex<mpsc::Receiver<()>>,
}

impl HealthProbe for Hanging {
    fn check(
        &self,
        _spec: &HealthSpec,
        _resource: &ResourceId,
        _unit: &UnitId,
        _settings: &UnitSettings,
    ) -> HealthResult {
        let _ = self.release.lock().unwrap().recv();
        HealthResult::Healthy
    }
}

/// A probe that notes how many of its checks run at once.
#[derive(Default)]
struct Counting {
    running: AtomicUsize,
    most: AtomicUsize,
}

impl HealthProbe for Counting {
    fn check(
        &self,
        _spec: &HealthSpec,
        _resource: &ResourceId,
        _unit: &UnitId,
        _settings: &UnitSettings,
    ) -> HealthResult {
        let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.most.fetch_max(now, Ordering::SeqCst);
        // Hold until the pool is full, then long enough for any excess
        // check to show.
        let patience = Instant::now() + Duration::from_secs(5);
        while self.most.load(Ordering::SeqCst) < CHECK_CONCURRENCY && Instant::now() < patience {
            thread::sleep(Duration::from_millis(1));
        }
        thread::sleep(Duration::from_millis(100));
        self.running.fetch_sub(1, Ordering::SeqCst);
        HealthResult::Healthy
    }
}

mod scheduler_checks {
    use super::*;

    mod run {
        use super::*;

        const LIMIT: Duration = Duration::from_secs(1);

        fn pool(
            probe: Arc<dyn HealthProbe>,
        ) -> (CheckPool, mpsc::Receiver<(CheckTicket, HealthResult)>) {
            let (tx, rx) = mpsc::channel();
            let tx = Mutex::new(tx);
            let pool = CheckPool::new(probe, LIMIT, move |ticket, result| {
                tx.lock().unwrap().send((ticket, result)).unwrap();
            })
            .unwrap();
            (pool, rx)
        }

        mod on_a_check_that_hangs_past_health_timeout {
            use super::*;

            #[test]
            fn reports_unhealthy_timed_out_after_health_timeout() {
                let (_release, held) = mpsc::channel();
                let (pool, done) = pool(Arc::new(Hanging {
                    release: Mutex::new(held),
                }));
                let started = Instant::now();
                pool.run(job(7));
                let (ticket, result) = done.recv_timeout(Duration::from_secs(5)).unwrap();
                assert_eq!(ticket, CheckTicket { row: 7, token: 1 });
                assert_eq!(result, HealthResult::Unhealthy("timed out".into()));
                let took = started.elapsed();
                assert!(took >= LIMIT, "reported after {took:?}");
                assert!(took < LIMIT * 3, "reported after {took:?}");
            }
        }

        mod on_more_checks_than_the_bound {
            use super::*;

            #[test]
            fn runs_at_most_four_at_once() {
                let probe = Arc::new(Counting::default());
                let (pool, done) = pool(probe.clone());
                for row in 0..12 {
                    pool.run(job(row));
                }
                for _ in 0..12 {
                    let (_, result) = done.recv_timeout(Duration::from_secs(10)).unwrap();
                    assert_eq!(result, HealthResult::Healthy);
                }
                assert_eq!(CHECK_CONCURRENCY, 4);
                assert_eq!(probe.most.load(Ordering::SeqCst), 4);
            }
        }
    }
}
