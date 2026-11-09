//! The interactive leasing view, driven through a pseudo-terminal: each
//! test runs `clerk` under `script -qefc` and types `\x03` (Ctrl+C) or
//! `\x1d` (Ctrl+]) into it.

mod common;

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use leclerk::client::{ApiError, Client};
use leclerk::config::Paths;
use leclerk::model::{Caller, HolderRef};
use leclerk::scope::ProjectResolver;
use serde_json::{Value, json};

use common::TestEnv;

/// Upper bound for one step; generous so a loaded machine never flakes.
const PATIENCE: Duration = Duration::from_secs(30);
/// The variable that marks an agent session.
const SESSION_VAR: &str = "CLAUDE_CODE_SESSION_ID";
/// Ctrl+C.
const CTRL_C: &[u8] = b"\x03";
/// Ctrl+].
const CTRL_BRACKET: &[u8] = b"\x1d";

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A private clerk home. Dropping it stops the daemon its commands started.
struct Env {
    env: TestEnv,
}

impl Env {
    fn new() -> Self {
        Self {
            env: TestEnv::new(),
        }
    }

    fn home(&self) -> &Path {
        self.env.home()
    }

    fn paths(&self) -> Paths {
        let home = self.home().to_path_buf();
        Paths::from_lookup(move |k| (k == "CLERK_HOME").then(|| home.clone().into_os_string()))
            .unwrap()
    }

    /// Run `args` as the user without a terminal; assert success and parse
    /// stdout as JSON.
    fn ok(&self, args: &[&str]) -> Value {
        let out = self
            .env
            .cmd()
            .env_remove(SESSION_VAR)
            .args(args)
            .timeout(PATIENCE)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {out:?}"))
    }

    /// A fresh directory below the home, standing for a project.
    fn dir(&self, name: &str) -> PathBuf {
        let dir = self.home().join("projects").join(name);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Call the daemon directly.
    fn rpc(&self, method: &str, caller: &Caller, args: Value) -> Result<Value, ApiError> {
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
        let _ = Command::new(env!("CARGO_BIN_EXE_clerk"))
            .args(["daemon", "stop"])
            .env("CLERK_HOME", self.home())
            .env_remove(SESSION_VAR)
            .stdin(Stdio::null())
            .output();
    }
}

/// A shell snippet running under `script`, so clerk's stdin and stdout are
/// a terminal. `$CLERK` names the clerk binary. Dropping it kills `script`.
struct Tty {
    child: Child,
    stdin: Option<ChildStdin>,
    shown: Arc<Mutex<Vec<u8>>>,
}

impl Tty {
    fn spawn(env: &Env, sh: &str) -> Self {
        Self::spawn_with(env, sh, &[])
    }

    fn spawn_with(env: &Env, sh: &str, vars: &[(&str, &str)]) -> Self {
        let mut child = Command::new("script")
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
    fn shown(&self) -> String {
        String::from_utf8_lossy(&self.shown.lock().unwrap()).replace("\r\n", "\n")
    }

    /// Wait until the terminal shows `needle`; panic after [`PATIENCE`].
    fn expect(&self, needle: &str) {
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
    fn send(&mut self, bytes: &[u8]) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(bytes).unwrap();
        stdin.flush().unwrap();
    }

    /// Wait for the snippet to end; its exit code and what it showed.
    fn finish(mut self) -> (i32, String) {
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

fn user(tag: &str) -> Caller {
    Caller {
        holder: HolderRef::User { tag: tag.into() },
        project: None,
        override_reason: None,
    }
}

/// The board: one unit `u1` keyed `k1`, tags and a usage template.
fn seed(env: &Env) {
    env.ok(&[
        "resource",
        "create",
        "board",
        "--type",
        "generic",
        "--description",
        "A test board",
        "--tag",
        "esp32,thread",
        "--usage",
        "Flash with key {unit.identity_key}",
    ]);
    env.ok(&[
        "unit",
        "add",
        "board",
        "u1",
        "--key",
        "k1",
        "--description",
        "The left one",
    ]);
}

/// User `who` holds the board's unit; returns the lease id.
fn hold(env: &Env, who: &str) -> String {
    let grant = env
        .rpc(
            "acquire",
            &user(who),
            json!({"target": {"resource": "board"}, "duration_ms": 600_000, "purpose": "theirs"}),
        )
        .unwrap();
    assert_eq!(grant["outcome"], "granted", "{grant}");
    grant["lease_id"].as_str().unwrap().to_owned()
}

/// User `who` releases `lease` and confirms its cleanup.
fn free(env: &Env, who: &str, lease: &str) {
    env.rpc("release", &user(who), json!({ "lease_id": lease }))
        .unwrap();
    env.rpc("confirm", &user(who), json!({ "lease_id": lease }))
        .unwrap();
}

/// An agent takes the board regardless: its holder gets `Overridden`.
fn override_board(env: &Env) {
    let dir = env.dir("alpha");
    let caller = Caller {
        holder: HolderRef::AgentSession {
            session_id: "sess-1".into(),
        },
        project: Some(ProjectResolver::resolve(&dir).unwrap()),
        override_reason: Some("the user asked".into()),
    };
    env.rpc(
        "acquire",
        &caller,
        json!({
            "target": {"resource": "board"},
            "duration_ms": 60_000,
            "purpose": "take it",
            "override_reason": "the user asked",
        }),
    )
    .unwrap();
}

/// The board unit's entry in the status view.
fn unit_status(env: &Env) -> Value {
    env.ok(&["status"])["resources"][0]["units"][0].clone()
}

/// Audit entries of `action`, newest first.
fn audit(env: &Env, action: &str) -> Vec<Value> {
    env.ok(&["log", "--action", action])
        .as_array()
        .unwrap()
        .clone()
}

/// The `lease_ended` audit detail of `lease`.
fn ending(env: &Env, lease: &str) -> Value {
    audit(env, "lease_ended")
        .into_iter()
        .map(|e| e["detail"].clone())
        .find(|d| d["lease"] == lease)
        .unwrap_or_else(|| panic!("lease {lease} never ended"))
}

/// A user lease on the board, acquired without a terminal.
fn detached_lease(env: &Env) -> String {
    env.ok(&["acquire", "board", "--for", "10m"])["lease_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

const ACQUIRE: &str = "\"$CLERK\" acquire board --for 10m";

/// A user request for the board, made without a terminal while the unit
/// is held (exit 6).
fn queue_detached(env: &Env) {
    let out = env
        .env
        .cmd()
        .env_remove(SESSION_VAR)
        .args(["acquire", "board", "--for", "10m"])
        .timeout(PATIENCE)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(6), "{out:?}");
}

// ---------------------------------------------------------------------------
// Behaviour
// ---------------------------------------------------------------------------

mod cli {
    use super::*;

    mod acquire {
        use super::*;

        mod on_stdin_tty {
            use super::*;

            #[test]
            fn shows_queue_position_then_grant_then_notices() {
                let env = Env::new();
                seed(&env);
                let theirs = hold(&env, "x");
                let mut tty = Tty::spawn(&env, ACQUIRE);
                tty.expect("position 1");
                free(&env, "x", &theirs);
                tty.expect("Granted");
                override_board(&env);
                tty.expect("overridden");
                tty.send(CTRL_C);
                let (_, shown) = tty.finish();
                let at = |s: &str| shown.find(s).unwrap();
                assert!(at("position 1") < at("Granted"), "{shown}");
                assert!(at("Granted") < at("overridden"), "{shown}");
            }

            #[test]
            fn ctrl_c_while_queued_cancels() {
                let env = Env::new();
                seed(&env);
                hold(&env, "x");
                let mut tty = Tty::spawn(&env, ACQUIRE);
                tty.expect("position 1");
                tty.send(CTRL_C);
                let (code, shown) = tty.finish();
                assert_eq!(code, 130, "{shown}");
                assert_eq!(env.ok(&["queue", "board"])[0]["queue"], json!([]));
                assert_eq!(audit(&env, "request_cancelled").len(), 1);
            }

            #[test]
            fn ctrl_c_while_held_releases_with_confirm_clean() {
                let env = Env::new();
                seed(&env);
                let mut tty = Tty::spawn(&env, ACQUIRE);
                tty.expect("Granted");
                let lease = unit_status(&env)["occupancy"]["lease_id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                tty.send(CTRL_C);
                let (code, shown) = tty.finish();
                assert_eq!(code, 130, "{shown}");
                let ended = ending(&env, &lease);
                assert_eq!(ended["cause"], "released");
                assert_eq!(ended["cleanup_confirmed"], true);
                assert_eq!(unit_status(&env)["dirty"], Value::Null);
            }

            #[test]
            fn ctrl_bracket_detaches_lease_kept_holder_user_prints_lease_id() {
                let env = Env::new();
                seed(&env);
                let mut tty = Tty::spawn(&env, ACQUIRE);
                tty.expect("Granted");
                tty.send(CTRL_BRACKET);
                let (code, shown) = tty.finish();
                assert_eq!(code, 0, "{shown}");
                let occupancy = unit_status(&env)["occupancy"].clone();
                assert_eq!(occupancy["state"], "held", "{occupancy}");
                assert_eq!(occupancy["holder"]["kind"], "user", "{occupancy}");
                let lease = occupancy["lease_id"].as_str().unwrap();
                let after_grant = &shown[shown.find("Granted").unwrap()..];
                assert!(after_grant.contains(lease), "{shown}");
            }

            #[test]
            fn terminal_restored_to_cooked_mode_on_sigterm() {
                let env = Env::new();
                seed(&env);
                let tty = Tty::spawn(&env, &format!("{ACQUIRE}; echo AFTER; stty -a"));
                tty.expect("Granted");
                let holder = unit_status(&env)["occupancy"]["holder"].clone();
                let pid = libc::pid_t::try_from(holder["pid"].as_u64().unwrap()).unwrap();
                // SAFETY: plain kill(2) on the clerk process the test started.
                assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
                let (_, shown) = tty.finish();
                let after = &shown[shown.find("AFTER").expect("clerk ended")..];
                assert!(after.contains(" icanon"), "{after}");
                assert!(!after.contains("-icanon"), "{after}");
            }
        }

        mod on_grant {
            use super::*;

            #[test]
            fn prints_brief() {
                let env = Env::new();
                seed(&env);
                let mut tty = Tty::spawn(&env, ACQUIRE);
                tty.expect("Flash with key k1");
                tty.send(CTRL_C);
                let (_, shown) = tty.finish();
                for part in ["A test board", "The left one", "esp32, thread"] {
                    assert!(shown.contains(part), "{part}: {shown}");
                }
            }
        }

        mod on_ctrl_c_with_offer_pending {
            use super::*;

            /// Queue the CLI behind user x, free the unit so the CLI is
            /// offered it, and press Ctrl+C while its claim waits. Returns
            /// the exit code, what was shown and the offered lease's id.
            fn interrupt_pending_claim(env: &Env) -> (i32, String, String) {
                seed(env);
                let theirs = hold(env, "x");
                let mut tty =
                    Tty::spawn_with(env, ACQUIRE, &[("CLERK_TEST_CLAIM_DELAY_MS", "60000")]);
                tty.expect("position 1");
                free(env, "x", &theirs);
                tty.expect("claiming");
                let occupancy = unit_status(env)["occupancy"].clone();
                assert_eq!(occupancy["state"], "offered", "{occupancy}");
                let offer = occupancy["lease_id"].as_str().unwrap().to_owned();
                tty.send(CTRL_C);
                let (code, shown) = tty.finish();
                (code, shown, offer)
            }

            #[test]
            fn offer_withdrawn_nothing_held() {
                let env = Env::new();
                let (_, shown, offer) = interrupt_pending_claim(&env);
                let cancelled = &audit(&env, "request_cancelled")[0]["detail"];
                assert_eq!(cancelled["offer"], offer.as_str(), "{shown}");
                assert_eq!(ending(&env, &offer)["cause"], "unclaimed");
                assert_eq!(audit(&env, "claimed"), Vec::<Value>::new());
                assert_eq!(audit(&env, "offer_declined"), Vec::<Value>::new());
                assert_eq!(unit_status(&env)["occupancy"]["kind"], "free");
            }

            #[test]
            fn exits_130() {
                let env = Env::new();
                let (code, shown, _) = interrupt_pending_claim(&env);
                assert_eq!(code, 130, "{shown}");
            }
        }
    }

    mod run {
        use super::*;

        mod on_ctrl_c {
            use super::*;

            #[test]
            fn cmd_gets_one_sigint() {
                let env = Env::new();
                seed(&env);
                let file = env.home().join("ints");
                let file_text = file.to_string_lossy().into_owned();
                let mut tty = Tty::spawn_with(
                    &env,
                    "\"$CLERK\" run board --for 10m -- sh -c 'trap \"echo INT >> \\\"$F\\\"\" INT; \
                     echo started; i=0; while [ $i -lt 20 ]; do sleep 0.1; i=$((i+1)); done'",
                    &[("F", &file_text)],
                );
                tty.expect("started");
                tty.send(CTRL_C);
                let (code, shown) = tty.finish();
                assert_eq!(code, 0, "{shown}");
                let ints = fs::read_to_string(&file).unwrap_or_default();
                assert_eq!(ints.lines().count(), 1, "{ints:?}\n{shown}");
                // Near-simultaneous SIGINTs can merge into one trap run, so
                // the count alone cannot see a forwarded copy; clerk says
                // when it forwards one.
                assert!(!shown.contains("Passing SIGINT on"), "{shown}");
            }
        }
    }

    mod attach {
        use super::*;

        mod on_offered_lease {
            use super::*;

            #[test]
            fn ctrl_c_cancels_the_request_withdrawing_the_offer() {
                let env = Env::new();
                seed(&env);
                let theirs = hold(&env, "x");
                queue_detached(&env);
                free(&env, "x", &theirs);
                let occupancy = unit_status(&env)["occupancy"].clone();
                assert_eq!(occupancy["state"], "offered", "{occupancy}");
                let offer = occupancy["lease_id"].as_str().unwrap().to_owned();
                let mut tty = Tty::spawn_with(
                    &env,
                    &format!("\"$CLERK\" attach {offer}"),
                    &[("CLERK_TEST_CLAIM_DELAY_MS", "60000")],
                );
                tty.expect("claiming");
                tty.send(CTRL_C);
                let (code, shown) = tty.finish();
                assert_eq!(code, 130, "{shown}");
                let cancelled = &audit(&env, "request_cancelled")[0]["detail"];
                assert_eq!(cancelled["offer"], offer.as_str(), "{shown}");
                assert_eq!(ending(&env, &offer)["cause"], "unclaimed");
                assert_eq!(audit(&env, "offer_declined"), Vec::<Value>::new());
            }
        }

        mod on_tty {
            use super::*;

            #[test]
            fn prints_brief_then_shows_notices_live() {
                let env = Env::new();
                seed(&env);
                let lease = detached_lease(&env);
                let mut tty = Tty::spawn(&env, &format!("\"$CLERK\" attach {lease}"));
                tty.expect("Flash with key k1");
                override_board(&env);
                tty.expect("overridden");
                tty.send(CTRL_C);
                let (code, shown) = tty.finish();
                assert_eq!(code, 130, "{shown}");
                for part in ["A test board", "The left one", "esp32, thread"] {
                    assert!(shown.contains(part), "{part}: {shown}");
                }
            }

            #[test]
            fn ctrl_c_releases() {
                let env = Env::new();
                seed(&env);
                let lease = detached_lease(&env);
                let mut tty = Tty::spawn(&env, &format!("\"$CLERK\" attach {lease}"));
                tty.expect("Flash with key k1");
                tty.send(CTRL_C);
                let (code, shown) = tty.finish();
                assert_eq!(code, 130, "{shown}");
                let ended = ending(&env, &lease);
                assert_eq!(ended["cause"], "released");
                assert_eq!(ended["cleanup_confirmed"], true);
            }

            #[test]
            fn ctrl_bracket_detaches_again() {
                let env = Env::new();
                seed(&env);
                let lease = detached_lease(&env);
                let before = unit_status(&env)["occupancy"].clone();
                let mut tty = Tty::spawn(&env, &format!("\"$CLERK\" attach {lease}"));
                tty.expect("Flash with key k1");
                tty.send(CTRL_BRACKET);
                let (code, shown) = tty.finish();
                assert_eq!(code, 0, "{shown}");
                let after = unit_status(&env)["occupancy"].clone();
                assert_eq!(after["lease_id"], lease.as_str());
                assert_eq!(after["state"], "held");
                assert_eq!(after["holder"], before["holder"]);
            }
        }
    }
}
