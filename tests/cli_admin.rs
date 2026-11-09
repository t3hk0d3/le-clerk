//! `clerk admin …`: every subcommand sits behind the gate and maps 1:1 to
//! a phase 1 `admin_*` call. Confirmation prompts run under a
//! pseudo-terminal (`script`).

mod common;

use std::fs;
use std::io::Read;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use leclerk::model::{Caller, HolderRef};
use serde_json::{Value, json};

use common::{Env, PATIENCE, SESSION_VAR, Tty};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A spawned `clerk` that is killed when dropped, so no test leaves one
/// behind.
struct Spawned(Child);

impl Drop for Spawned {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn user(tag: &str) -> Caller {
    Caller {
        holder: HolderRef::User { tag: tag.into() },
        project: None,
        override_reason: None,
    }
}

/// `clerk args…` as the user, without a terminal, with a stdin that stays
/// open and never delivers: a command that read it would hang.
fn clerk(env: &Env, args: &[&str]) -> Output {
    clerk_with(env, args, &[])
}

fn clerk_with(env: &Env, args: &[&str], vars: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_clerk"));
    cmd.args(args)
        .env("CLERK_HOME", env.home())
        .env_remove(SESSION_VAR)
        .envs(vars.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = Spawned(cmd.spawn().unwrap());
    let stdin = child.0.stdin.take();
    let mut out = child.0.stdout.take().unwrap();
    let mut err = child.0.stderr.take().unwrap();
    let stdout_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out.read_to_end(&mut buf);
        buf
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err.read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + PATIENCE;
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "clerk {args:?} did not end");
        std::thread::sleep(Duration::from_millis(20));
    };
    drop(stdin);
    Output {
        status,
        stdout: stdout_reader.join().unwrap(),
        stderr: stderr_reader.join().unwrap(),
    }
}

/// `clerk admin args… --yes`, asserted to succeed; stdout as JSON.
fn admin_ok(env: &Env, args: &[&str]) -> Value {
    let mut full = vec!["admin"];
    full.extend_from_slice(args);
    full.push("--yes");
    let out = clerk(env, &full);
    assert!(out.status.success(), "{out:?}");
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {out:?}"))
}

/// `clerk args…`, asserted to end with `code`; stdout parsed as the JSON
/// error it must be.
fn failed(env: &Env, args: &[&str], code: i32) -> Value {
    let out = clerk(env, args);
    assert_eq!(out.status.code(), Some(code), "{out:?}");
    let body: Value =
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {out:?}"));
    body["error"].clone()
}

/// A resource `id` with one unit `u1` per name in `units`.
fn seed(env: &Env, id: &str, units: &[&str]) {
    env.ok(&["resource", "create", id, "--type", "generic"]);
    for unit in units {
        env.ok(&[
            "unit",
            "add",
            id,
            unit,
            "--key",
            &format!("key-{id}-{unit}"),
        ]);
    }
}

/// User `who` holds a unit of `resource` (any, or the one named).
fn hold(env: &Env, who: &str, resource: &str) -> String {
    let grant = env
        .rpc(
            "acquire",
            &user(who),
            json!({"target": {"resource": resource}, "duration_ms": 600_000, "purpose": "theirs"}),
        )
        .unwrap();
    assert_eq!(grant["outcome"], "granted", "{grant}");
    grant["lease_id"].as_str().unwrap().to_owned()
}

/// User `who` queues for `resource`; the request id.
fn queue(env: &Env, who: &str, resource: &str) -> String {
    let queued = env
        .rpc(
            "acquire",
            &user(who),
            json!({"target": {"resource": resource}, "duration_ms": 600_000, "purpose": "waiting"}),
        )
        .unwrap();
    assert_eq!(queued["outcome"], "queued", "{queued}");
    queued["request_id"].as_str().unwrap().to_owned()
}

fn status(env: &Env) -> Value {
    env.ok(&["status"])
}

/// The status entry of resource `id`.
fn resource_status(env: &Env, id: &str) -> Value {
    status(env)["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == id)
        .cloned()
        .unwrap_or_else(|| panic!("no resource {id}"))
}

/// The first unit of resource `id` in the status view.
fn unit_status(env: &Env, id: &str) -> Value {
    resource_status(env, id)["units"][0].clone()
}

fn audit(env: &Env, action: &str) -> Vec<Value> {
    env.ok(&["log", "--action", action])
        .as_array()
        .unwrap()
        .clone()
}

/// A user lease on `board`'s only unit, held by `who`; the lease id.
fn board_held(env: &Env, who: &str) -> String {
    seed(env, "board", &["u1"]);
    hold(env, who, "board")
}

const RELEASE_EFFECT: &str = "end lease";

// ---------------------------------------------------------------------------
// Behaviour
// ---------------------------------------------------------------------------

mod cli_admin {
    use super::*;

    mod gate {
        use super::*;

        mod on_non_tty_without_yes {
            use super::*;

            #[test]
            fn exits_4_and_nothing_changed() {
                let env = Env::new();
                let lease = board_held(&env, "x");
                let error = failed(&env, &["admin", "release", &lease], 4);
                assert_eq!(error["code"], "confirmation_required", "{error}");
                assert!(
                    error["message"].as_str().unwrap().contains(RELEASE_EFFECT),
                    "{error}"
                );
                assert_eq!(unit_status(&env, "board")["occupancy"]["state"], "held");
                assert_eq!(audit(&env, "admin_release"), Vec::<Value>::new());
            }

            #[test]
            fn never_waits_for_input() {
                // `clerk` runs with an open, silent stdin; reaching the end
                // of the test at all proves it did not block on it.
                let env = Env::new();
                let _lease = board_held(&env, "x");
                let out = clerk(&env, &["admin", "clean", "board/u1"]);
                assert_eq!(out.status.code(), Some(4), "{out:?}");
            }

            #[test]
            fn reaches_no_daemon() {
                let env = Env::new();
                let out = clerk(&env, &["admin", "release", "lse_01ABC"]);
                assert_eq!(out.status.code(), Some(4), "{out:?}");
                assert!(!env.paths().socket.exists(), "a daemon was started");
            }
        }

        mod on_tty_answer_n {
            use super::*;

            #[test]
            fn aborts() {
                let env = Env::new();
                let lease = board_held(&env, "x");
                let mut tty = Tty::spawn(&env, &format!("\"$CLERK\" admin release {lease}"));
                tty.expect("Proceed?");
                tty.send(b"n\n");
                let (code, shown) = tty.finish();
                assert_eq!(code, 4, "{shown}");
                assert!(shown.contains(RELEASE_EFFECT), "{shown}");
                assert!(shown.contains(&lease), "{shown}");
                assert_eq!(unit_status(&env, "board")["occupancy"]["state"], "held");
                assert_eq!(audit(&env, "admin_release"), Vec::<Value>::new());
            }
        }

        mod on_tty_answer_y {
            use super::*;

            #[test]
            fn runs_the_command() {
                let env = Env::new();
                let lease = board_held(&env, "x");
                let mut tty = Tty::spawn(&env, &format!("\"$CLERK\" admin release {lease}"));
                tty.expect("Proceed?");
                tty.send(b"y\n");
                let (code, shown) = tty.finish();
                assert_eq!(code, 0, "{shown}");
                assert_eq!(audit(&env, "admin_release").len(), 1, "{shown}");
            }
        }

        mod on_agent_caller_without_override {
            use super::*;

            #[test]
            fn exits_4_and_nothing_changed() {
                let env = Env::new();
                let lease = board_held(&env, "x");
                let out = clerk_with(
                    &env,
                    &["admin", "release", &lease, "--yes"],
                    &[(SESSION_VAR, "sess-1")],
                );
                assert_eq!(out.status.code(), Some(4), "{out:?}");
                let body: Value = serde_json::from_slice(&out.stdout).unwrap();
                assert_eq!(body["error"]["code"], "override_required", "{body}");
                assert_eq!(unit_status(&env, "board")["occupancy"]["state"], "held");
            }

            #[test]
            fn is_refused_for_read_only_subcommands_too() {
                let env = Env::new();
                for args in [vec!["admin", "export"], vec!["admin", "doctor"]] {
                    let out = clerk_with(&env, &args, &[(SESSION_VAR, "sess-1")]);
                    assert_eq!(out.status.code(), Some(4), "{args:?}: {out:?}");
                }
            }
        }

        mod on_agent_caller_with_override {
            use super::*;

            #[test]
            fn runs_and_records_the_reason() {
                let env = Env::new();
                let lease = board_held(&env, "x");
                let out = clerk_with(
                    &env,
                    &[
                        "admin",
                        "release",
                        &lease,
                        "--yes",
                        "--override",
                        "the user asked",
                    ],
                    &[(SESSION_VAR, "sess-1")],
                );
                assert!(out.status.success(), "{out:?}");
                let entry = &audit(&env, "admin_release")[0];
                assert_eq!(entry["override_reason"], "the user asked", "{entry}");
            }
        }
    }

    mod on_success {
        use super::*;

        mod release {
            use super::*;

            #[test]
            fn ends_the_lease_and_marks_the_unit_dirty() {
                let env = Env::new();
                let lease = board_held(&env, "x");
                admin_ok(&env, &["release", &lease]);
                let unit = unit_status(&env, "board");
                assert!(
                    unit["dirty"].is_object() || unit["dirty"].is_string(),
                    "{unit}"
                );
                assert_ne!(unit["occupancy"]["state"], "held", "{unit}");
                assert_eq!(audit(&env, "admin_release").len(), 1);
            }

            #[test]
            fn on_an_unknown_lease_exits_3() {
                let env = Env::new();
                seed(&env, "board", &["u1"]);
                let out = clerk(
                    &env,
                    &[
                        "admin",
                        "release",
                        "lse_01ARZ3NDEKTSV4RRFFQ69G5FAV",
                        "--yes",
                    ],
                );
                assert_eq!(out.status.code(), Some(3), "{out:?}");
            }
        }

        mod evict {
            use super::*;

            #[test]
            fn ends_the_holders_leases() {
                let env = Env::new();
                let lease = board_held(&env, "x");
                let result = admin_ok(&env, &["evict", "user:x"]);
                assert_eq!(result["leases_ended"], json!([lease]), "{result}");
            }

            #[test]
            fn on_a_malformed_holder_exits_2() {
                let env = Env::new();
                let error = failed(&env, &["admin", "evict", "nobody", "--yes"], 2);
                assert_eq!(error["code"], "usage_error", "{error}");
            }
        }

        mod dequeue {
            use super::*;

            #[test]
            fn removes_the_queued_request() {
                let env = Env::new();
                let _lease = board_held(&env, "x");
                let request = queue(&env, "y", "board");
                admin_ok(&env, &["dequeue", &request]);
                assert_eq!(resource_status(&env, "board")["queue"], json!([]));
            }
        }

        mod clear_queue {
            use super::*;

            #[test]
            fn drops_every_request() {
                let env = Env::new();
                let _lease = board_held(&env, "x");
                let first = queue(&env, "y", "board");
                let second = queue(&env, "z", "board");
                let result = admin_ok(&env, &["clear-queue", "board"]);
                let dropped = result["requests_dropped"].as_array().unwrap();
                assert!(dropped.contains(&json!(first)), "{result}");
                assert!(dropped.contains(&json!(second)), "{result}");
            }
        }

        mod remove_unit {
            use super::*;

            #[test]
            fn removes_a_held_unit() {
                let env = Env::new();
                let _lease = board_held(&env, "x");
                admin_ok(&env, &["remove-unit", "board/u1"]);
                assert_eq!(resource_status(&env, "board")["units"], json!([]));
            }

            #[test]
            fn on_a_malformed_target_exits_2() {
                let env = Env::new();
                let error = failed(&env, &["admin", "remove-unit", "board", "--yes"], 2);
                assert_eq!(error["code"], "usage_error", "{error}");
            }
        }

        mod delete_resource {
            use super::*;

            #[test]
            fn deletes_a_held_resource() {
                let env = Env::new();
                let _lease = board_held(&env, "x");
                admin_ok(&env, &["delete-resource", "board"]);
                assert_eq!(status(&env)["resources"], json!([]));
            }
        }

        mod clean {
            use super::*;

            #[test]
            fn clears_the_dirty_flag() {
                let env = Env::new();
                let lease = board_held(&env, "x");
                admin_ok(&env, &["release", &lease]);
                assert!(!unit_status(&env, "board")["dirty"].is_null());
                admin_ok(&env, &["clean", "board/u1"]);
                assert!(unit_status(&env, "board")["dirty"].is_null());
            }
        }

        mod link_and_unlink {
            use super::*;

            #[test]
            fn link_makes_the_units_exclusive() {
                let env = Env::new();
                seed(&env, "one", &["a"]);
                seed(&env, "two", &["b"]);
                admin_ok(&env, &["link", "one/a", "two/b"]);
                assert_eq!(audit(&env, "admin_link").len(), 1);
                let _lease = hold(&env, "x", "one");
                let blocked = env
                    .rpc(
                        "acquire",
                        &user("y"),
                        json!({"target": {"resource": "two"}, "duration_ms": 60_000, "purpose": "p"}),
                    )
                    .unwrap();
                assert_eq!(blocked["outcome"], "queued", "{blocked}");
            }

            #[test]
            fn unlink_separates_them_again() {
                let env = Env::new();
                seed(&env, "one", &["a"]);
                seed(&env, "two", &["b"]);
                admin_ok(&env, &["link", "one/a", "two/b"]);
                admin_ok(&env, &["unlink", "one/a", "two/b"]);
                assert_eq!(audit(&env, "admin_unlink").len(), 1);
                let _lease = hold(&env, "x", "one");
                let free = env
                    .rpc(
                        "acquire",
                        &user("y"),
                        json!({"target": {"resource": "two"}, "duration_ms": 60_000, "purpose": "p"}),
                    )
                    .unwrap();
                assert_eq!(free["outcome"], "granted", "{free}");
            }
        }

        mod move_ {
            use super::*;

            #[test]
            fn moves_the_resource_to_the_given_scope() {
                let env = Env::new();
                seed(&env, "board", &["u1"]);
                let dir = env.dir("alpha");
                let to = format!("project:{}", dir.display());
                let result = admin_ok(&env, &["move", "board", "--to", &to]);
                assert!(result["scope"]["project"].is_string(), "{result}");
                assert_eq!(audit(&env, "admin_move").len(), 1);
            }
        }

        mod rename {
            use super::*;

            #[test]
            fn renames_a_resource() {
                let env = Env::new();
                seed(&env, "board", &["u1"]);
                let result = admin_ok(&env, &["rename", "board", "panel"]);
                assert_eq!(result["id"], "panel", "{result}");
            }

            #[test]
            fn with_unit_renames_that_unit() {
                let env = Env::new();
                seed(&env, "board", &["u1"]);
                admin_ok(&env, &["rename", "board", "left", "--unit", "u1"]);
                assert_eq!(unit_status(&env, "board")["id"], "left");
            }
        }

        mod edit_lease {
            use super::*;

            #[test]
            fn changes_priority_and_interruptibility() {
                let env = Env::new();
                let lease = board_held(&env, "x");
                let result = admin_ok(
                    &env,
                    &[
                        "edit-lease",
                        &lease,
                        "--priority",
                        "high",
                        "--interruptible",
                        "true",
                    ],
                );
                assert_eq!(result["priority"], "high", "{result}");
                assert_eq!(result["interruptible"], true, "{result}");
            }

            #[test]
            fn for_moves_the_end_to_now_plus_the_duration() {
                let env = Env::new();
                let lease = board_held(&env, "x");
                let result = admin_ok(&env, &["edit-lease", &lease, "--for", "2h"]);
                let ends = result["ends_at"].as_i64().unwrap();
                let now = i64::try_from(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis(),
                )
                .unwrap();
                let two_hours = 2 * 3600 * 1000;
                assert!((ends - now - two_hours).abs() < 60_000, "{result}");
            }

            #[test]
            fn without_any_change_exits_2_and_reaches_no_daemon() {
                let env = Env::new();
                let error = failed(&env, &["admin", "edit-lease", "lse_x", "--yes"], 2);
                assert_eq!(error["code"], "usage_error", "{error}");
                assert!(!env.paths().socket.exists(), "a daemon was started");
            }

            #[test]
            fn with_a_bad_duration_exits_2() {
                let env = Env::new();
                let error = failed(
                    &env,
                    &["admin", "edit-lease", "lse_x", "--for", "0", "--yes"],
                    2,
                );
                assert_eq!(error["code"], "usage_error", "{error}");
            }
        }

        mod doctor {
            use super::*;

            #[test]
            fn on_a_healthy_inventory_finds_nothing() {
                let env = Env::new();
                seed(&env, "board", &["u1"]);
                let out = clerk(&env, &["admin", "doctor"]);
                assert!(out.status.success(), "{out:?}");
                let report: Value = serde_json::from_slice(&out.stdout).unwrap();
                assert_eq!(report["findings"], json!([]), "{report}");
            }

            #[test]
            fn without_fix_needs_no_confirmation() {
                // Changes nothing, so no `--yes`: exit 0 on a non-terminal.
                let env = Env::new();
                let out = clerk(&env, &["admin", "doctor"]);
                assert_eq!(out.status.code(), Some(0), "{out:?}");
            }

            #[test]
            fn with_fix_is_gated() {
                let env = Env::new();
                let error = failed(&env, &["admin", "doctor", "--fix"], 4);
                assert_eq!(error["code"], "confirmation_required", "{error}");
            }

            #[test]
            fn reports_unfixable_findings_without_failing() {
                let env = Env::new();
                seed(&env, "board", &["u1"]);
                clerk(&env, &["daemon", "stop"]);
                let conn = rusqlite::Connection::open(&env.paths().db).unwrap();
                conn.execute_batch("UPDATE resources SET usage = 'flash {unit.pth}';")
                    .unwrap();
                drop(conn);
                let out = clerk(&env, &["admin", "doctor", "--fix", "--yes"]);
                assert_eq!(out.status.code(), Some(0), "{out:?}");
                let report: Value = serde_json::from_slice(&out.stdout).unwrap();
                let finding = &report["findings"][0];
                assert_eq!(finding["kind"], "invalid_resource_text", "{report}");
                assert_eq!(finding["fixed"], false, "{report}");
            }

            #[test]
            fn human_output_names_each_finding() {
                let env = Env::new();
                seed(&env, "board", &["u1"]);
                clerk(&env, &["daemon", "stop"]);
                let conn = rusqlite::Connection::open(&env.paths().db).unwrap();
                conn.execute_batch("UPDATE resources SET usage = 'flash {unit.pth}';")
                    .unwrap();
                drop(conn);
                let tty = Tty::spawn(&env, "\"$CLERK\" admin doctor");
                tty.expect("board");
                let (code, shown) = tty.finish();
                assert_eq!(code, 0, "{shown}");
                assert!(shown.contains("invalid_resource_text"), "{shown}");
            }
        }

        mod export_import {
            use super::*;

            #[test]
            fn export_prints_the_document_on_stdout() {
                let env = Env::new();
                seed(&env, "board", &["u1"]);
                let out = clerk(&env, &["admin", "export"]);
                assert!(out.status.success(), "{out:?}");
                let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
                assert_eq!(doc["version"], 1, "{doc}");
                assert_eq!(doc["resources"][0]["id"], "board", "{doc}");
            }

            #[test]
            fn export_out_writes_the_file_and_nothing_else_on_stdout() {
                let env = Env::new();
                seed(&env, "board", &["u1"]);
                let file = env.home().join("inventory.json");
                let out = clerk(&env, &["admin", "export", "--out", file.to_str().unwrap()]);
                assert!(out.status.success(), "{out:?}");
                let doc: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
                assert_eq!(doc["resources"][0]["id"], "board", "{doc}");
                let said: Value = serde_json::from_slice(&out.stdout).unwrap();
                assert!(said.get("resources").is_none(), "{said}");
            }

            #[test]
            fn round_trip_via_files() {
                let source = Env::new();
                seed(&source, "board", &["u1", "u2"]);
                let file = source.home().join("inventory.json");
                let out = clerk(
                    &source,
                    &["admin", "export", "--out", file.to_str().unwrap()],
                );
                assert!(out.status.success(), "{out:?}");

                let target = Env::new();
                let result = admin_ok(&target, &["import", "--in", file.to_str().unwrap()]);
                assert_eq!(result["imported"], json!(["board (user)"]), "{result}");
                let again = target.home().join("again.json");
                let out = clerk(
                    &target,
                    &["admin", "export", "--out", again.to_str().unwrap()],
                );
                assert!(out.status.success(), "{out:?}");
                assert_eq!(
                    fs::read_to_string(&file).unwrap(),
                    fs::read_to_string(&again).unwrap()
                );
            }

            #[test]
            fn import_of_an_existing_resource_is_refused_without_replace() {
                let env = Env::new();
                seed(&env, "board", &["u1"]);
                let file = env.home().join("inventory.json");
                clerk(&env, &["admin", "export", "--out", file.to_str().unwrap()]);
                let out = clerk(
                    &env,
                    &["admin", "import", "--in", file.to_str().unwrap(), "--yes"],
                );
                assert_eq!(out.status.code(), Some(4), "{out:?}");
                let result = admin_ok(
                    &env,
                    &["import", "--in", file.to_str().unwrap(), "--replace"],
                );
                assert_eq!(result["replaced"], json!(["board (user)"]), "{result}");
            }

            #[test]
            fn import_is_gated() {
                let env = Env::new();
                let file = env.home().join("inventory.json");
                fs::write(&file, r#"{"version":1,"resources":[]}"#).unwrap();
                let error = failed(
                    &env,
                    &["admin", "import", "--in", file.to_str().unwrap()],
                    4,
                );
                assert_eq!(error["code"], "confirmation_required", "{error}");
            }

            #[test]
            fn import_of_a_missing_file_exits_2_naming_it() {
                let env = Env::new();
                let error = failed(
                    &env,
                    &["admin", "import", "--in", "/no/such/file.json", "--yes"],
                    2,
                );
                assert!(
                    error["message"]
                        .as_str()
                        .unwrap()
                        .contains("/no/such/file.json"),
                    "{error}"
                );
                assert!(!env.paths().socket.exists(), "a daemon was started");
            }

            #[test]
            fn export_out_creates_the_file_readable_by_its_owner_only() {
                use std::os::unix::fs::PermissionsExt;
                let env = Env::new();
                seed(&env, "board", &["u1"]);
                let file = env.home().join("inventory.json");
                let out = clerk(&env, &["admin", "export", "--out", file.to_str().unwrap()]);
                assert!(out.status.success(), "{out:?}");
                let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "{mode:o}");
            }

            #[test]
            fn export_out_refuses_an_existing_file_exit_4() {
                let env = Env::new();
                seed(&env, "board", &["u1"]);
                let file = env.home().join("inventory.json");
                fs::write(&file, "keep me").unwrap();
                let error = failed(
                    &env,
                    &["admin", "export", "--out", file.to_str().unwrap()],
                    4,
                );
                assert_eq!(error["code"], "already_exists", "{error}");
                assert_eq!(fs::read_to_string(&file).unwrap(), "keep me");
            }

            #[test]
            fn export_out_with_force_overwrites_an_existing_file() {
                use std::os::unix::fs::PermissionsExt;
                let env = Env::new();
                seed(&env, "board", &["u1"]);
                let file = env.home().join("inventory.json");
                fs::write(&file, "old").unwrap();
                fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
                let out = clerk(
                    &env,
                    &[
                        "admin",
                        "export",
                        "--out",
                        file.to_str().unwrap(),
                        "--force",
                    ],
                );
                assert!(out.status.success(), "{out:?}");
                let doc: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
                assert_eq!(doc["resources"][0]["id"], "board", "{doc}");
                let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "{mode:o}");
            }

            #[test]
            fn import_of_a_missing_file_exits_2_before_the_gate() {
                let env = Env::new();
                let error = failed(&env, &["admin", "import", "--in", "/no/such/file.json"], 2);
                assert_eq!(error["code"], "usage_error", "{error}");
                assert!(!env.paths().socket.exists(), "a daemon was started");
            }

            #[test]
            fn import_of_malformed_json_exits_2_before_the_gate() {
                let env = Env::new();
                let file = env.home().join("bad.json");
                fs::write(&file, "{ nope").unwrap();
                let error = failed(
                    &env,
                    &["admin", "import", "--in", file.to_str().unwrap()],
                    2,
                );
                assert_eq!(error["code"], "usage_error", "{error}");
            }

            #[test]
            fn import_of_malformed_json_exits_2() {
                let env = Env::new();
                let file = env.home().join("bad.json");
                fs::write(&file, "{ nope").unwrap();
                let error = failed(
                    &env,
                    &["admin", "import", "--in", file.to_str().unwrap(), "--yes"],
                    2,
                );
                assert_eq!(error["code"], "usage_error", "{error}");
            }
        }
    }

    /// `clerk release --force <lease>`: `admin release` by another name.
    mod release_force {
        use super::*;

        #[test]
        fn ends_the_lease_and_marks_the_unit_dirty() {
            let env = Env::new();
            let lease = board_held(&env, "x");
            let out = clerk(&env, &["release", "--force", &lease, "--yes"]);
            assert!(out.status.success(), "{out:?}");
            let unit = unit_status(&env, "board");
            assert!(
                unit["dirty"].is_object() || unit["dirty"].is_string(),
                "{unit}"
            );
            assert_eq!(audit(&env, "admin_release").len(), 1);
        }

        #[test]
        fn without_yes_off_a_terminal_is_refused_and_changes_nothing() {
            let env = Env::new();
            let lease = board_held(&env, "x");
            let error = failed(&env, &["release", "--force", &lease], 4);
            assert_eq!(error["code"], "confirmation_required", "{error}");
            assert_eq!(unit_status(&env, "board")["occupancy"]["state"], "held");
        }

        #[test]
        fn agent_without_override_is_refused() {
            let env = Env::new();
            let lease = board_held(&env, "x");
            let out = clerk_with(
                &env,
                &["release", "--force", &lease, "--yes"],
                &[(SESSION_VAR, "sess-1")],
            );
            assert_eq!(out.status.code(), Some(4), "{out:?}");
            let body: Value = serde_json::from_slice(&out.stdout).unwrap();
            assert_eq!(body["error"]["code"], "override_required", "{body}");
        }

        #[test]
        fn yes_without_force_is_a_usage_error() {
            let env = Env::new();
            let error = failed(&env, &["release", "lse_x", "--yes"], 2);
            assert_eq!(error["code"], "usage_error", "{error}");
        }
    }
}
