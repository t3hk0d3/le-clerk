// Shared by every test crate; each one uses only a subset.
#![allow(dead_code)]

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command as StdCommand, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use assert_cmd::Command;
use leclerk::client::{ApiError, Client};
use leclerk::config::Paths;
use leclerk::model::{Caller, Resource, Unit};
use leclerk::store::Tx;
use serde_json::Value;
use tempfile::TempDir;

/// An isolated clerk environment: every command it builds runs with
/// `CLERK_HOME` pointing into a private tempdir.
pub struct TestEnv {
    home: TempDir,
}

impl TestEnv {
    pub fn new() -> Self {
        Self {
            home: TempDir::new().expect("create tempdir"),
        }
    }

    pub fn home(&self) -> &std::path::Path {
        self.home.path()
    }

    /// A `clerk` command bound to this environment.
    pub fn cmd(&self) -> Command {
        let mut cmd = Command::cargo_bin("clerk").expect("clerk binary");
        cmd.env("CLERK_HOME", self.home.path());
        cmd
    }
}

/// Insert a lease row for `unit` directly, bypassing the scheduler.
/// `state` is the stored text of a `LeaseState`, e.g. `"held"`.
pub fn insert_lease(tx: &Tx, unit: &Unit, state: &str) {
    tx.conn()
        .execute(
            "INSERT INTO leases (lease_id, unit_row, resource_row, state, holder, priority, \
             interruptible, purpose, duration_ms, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'normal', 0, 'test', 60000, 0)",
            rusqlite::params![
                leclerk::model::LeaseId::new().as_str(),
                unit.row,
                unit.resource_row,
                state,
                r#"{"kind":"user","tag":"holder"}"#,
            ],
        )
        .expect("insert lease");
}

/// Insert a queued request row for `resource`, optionally pinned to `unit`.
pub fn insert_queued_request(tx: &Tx, resource: &Resource, unit: Option<&Unit>) {
    tx.conn()
        .execute(
            "INSERT INTO requests (request_id, resource_row, unit_row, state, holder, priority, \
             interruptible, purpose, duration_ms, created_at) \
             VALUES (?1, ?2, ?3, 'queued', ?4, 'normal', 0, 'test', 60000, 0)",
            rusqlite::params![
                leclerk::model::RequestId::new().as_str(),
                resource.row,
                unit.map(|u| u.row),
                r#"{"kind":"user","tag":"waiter"}"#,
            ],
        )
        .expect("insert request");
}

// ---------------------------------------------------------------------------
// Harness shared by the daemon-backed CLI tests
// ---------------------------------------------------------------------------

/// Upper bound for one step; generous so a loaded machine never flakes.
pub const PATIENCE: Duration = Duration::from_secs(30);
/// The variable that marks an agent session.
pub const SESSION_VAR: &str = "CLAUDE_CODE_SESSION_ID";
/// Ctrl+C.
pub const CTRL_C: &[u8] = b"\x03";
/// Ctrl+].
pub const CTRL_BRACKET: &[u8] = b"\x1d";

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A private clerk home. Dropping it stops the daemon its commands started.
pub struct Env {
    env: TestEnv,
}

impl Env {
    pub fn new() -> Self {
        Self {
            env: TestEnv::new(),
        }
    }

    pub fn home(&self) -> &Path {
        self.env.home()
    }

    pub fn paths(&self) -> Paths {
        let home = self.home().to_path_buf();
        Paths::from_lookup(move |k| (k == "CLERK_HOME").then(|| home.clone().into_os_string()))
            .unwrap()
    }

    /// A clerk command run by the user: no agent session, bounded by
    /// [`PATIENCE`].
    pub fn cmd(&self) -> Command {
        let mut cmd = self.env.cmd();
        cmd.env_remove(SESSION_VAR).timeout(PATIENCE);
        cmd
    }

    /// Run `args` as the user without a terminal; assert success and parse
    /// stdout as JSON.
    pub fn ok(&self, args: &[&str]) -> Value {
        let out = self.cmd().args(args).output().unwrap();
        assert!(out.status.success(), "{out:?}");
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {out:?}"))
    }

    /// A fresh directory below the home, standing for a project.
    pub fn dir(&self, name: &str) -> PathBuf {
        let dir = self.home().join("projects").join(name);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Call the daemon directly.
    pub fn rpc(&self, method: &str, caller: &Caller, args: Value) -> Result<Value, ApiError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let client = Client::connect(&self.paths()).await.expect("daemon runs");
            client.call(method, caller, args).await
        })
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = StdCommand::new(env!("CARGO_BIN_EXE_clerk"))
            .args(["daemon", "stop"])
            .env("CLERK_HOME", self.home())
            .env_remove(SESSION_VAR)
            .stdin(Stdio::null())
            .output();
    }
}

/// A shell snippet running under `script`, so clerk's stdin and stdout are
/// a terminal. `$CLERK` names the clerk binary. Dropping it kills `script`.
pub struct Tty {
    child: Child,
    stdin: Option<ChildStdin>,
    shown: Arc<Mutex<Vec<u8>>>,
}

impl Tty {
    pub fn spawn(env: &Env, sh: &str) -> Self {
        Self::spawn_with(env, sh, &[])
    }

    pub fn spawn_with(env: &Env, sh: &str, vars: &[(&str, &str)]) -> Self {
        let mut child = StdCommand::new("script")
            .args(["-qefc", sh, "/dev/null"])
            .env("CLERK", env!("CARGO_BIN_EXE_clerk"))
            .env("CLERK_HOME", env.home())
            .env_remove(SESSION_VAR)
            .envs(vars.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let mut stdout = child.stdout.take().unwrap();
        let shown = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&shown);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        Self {
            child,
            stdin,
            shown,
        }
    }

    /// What the terminal showed so far, with `\r\n` as `\n`.
    pub fn shown(&self) -> String {
        String::from_utf8_lossy(&self.shown.lock().unwrap()).replace("\r\n", "\n")
    }

    /// Wait until the terminal shows `needle`; panic after [`PATIENCE`].
    pub fn expect(&self, needle: &str) {
        let deadline = Instant::now() + PATIENCE;
        while !self.shown().contains(needle) {
            assert!(
                Instant::now() < deadline,
                "never showed {needle:?}:\n{}",
                self.shown()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Type `bytes`.
    pub fn send(&mut self, bytes: &[u8]) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(bytes).unwrap();
        stdin.flush().unwrap();
    }

    /// Wait for the snippet to end; its exit code and what it showed.
    pub fn finish(mut self) -> (i32, String) {
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                // Let the reader drain what is left.
                std::thread::sleep(Duration::from_millis(100));
                return (status.code().unwrap_or(-1), self.shown());
            }
            assert!(Instant::now() < deadline, "did not end:\n{}", self.shown());
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Tty {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
