mod common;

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use fs4::fs_std::FileExt;
use leclerk::client::{ApiError, Client};
use leclerk::config::Paths;
use leclerk::model::{Caller, HolderRef};
use leclerk::scope::ProjectResolver;
use serde_json::{Value, json};

use common::TestEnv;

/// Upper bound for one command; generous so a loaded machine never flakes.
const PATIENCE: Duration = Duration::from_secs(30);
/// The variable that marks an agent session.
const SESSION_VAR: &str = "CLAUDE_CODE_SESSION_ID";

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

    /// A clerk command run by the user: no agent session, stdin closed.
    fn cmd(&self) -> Command {
        let mut cmd = self.env.cmd();
        cmd.env_remove(SESSION_VAR).timeout(PATIENCE);
        cmd
    }

    /// A clerk command run by agent session `session` in `cwd`.
    fn agent(&self, session: &str, cwd: &Path) -> Command {
        let mut cmd = self.cmd();
        cmd.env(SESSION_VAR, session).current_dir(cwd);
        cmd
    }

    /// Run `args` as the user; assert success and parse stdout as JSON.
    fn ok(&self, args: &[&str]) -> Value {
        json_of(self.cmd().args(args))
    }

    /// A fresh directory below the home, standing for a project.
    fn dir(&self, name: &str) -> PathBuf {
        let dir = self.home().join("projects").join(name);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// True once a daemon has served this home.
    fn daemon_started(&self) -> bool {
        self.paths().socket.exists() || self.paths().db.exists()
    }

    /// Run the shell snippet `sh` under `script`, so clerk's stdin and
    /// stdout are a terminal. `$CLERK` names the clerk binary. Returns
    /// what the terminal showed.
    fn tty(&self, sh: &str) -> String {
        let out = Command::new("script")
            .args(["-qec", sh, "/dev/null"])
            .env("CLERK", env!("CARGO_BIN_EXE_clerk"))
            .env("CLERK_HOME", self.home())
            .env_remove(SESSION_VAR)
            .timeout(PATIENCE)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n")
    }

    /// Call the daemon directly, for set-up the CLI cannot do yet.
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
        let _ = std::process::Command::new(env!("CARGO_BIN_EXE_clerk"))
            .args(["daemon", "stop"])
            .env("CLERK_HOME", self.home())
            .env_remove(SESSION_VAR)
            .stdin(Stdio::null())
            .output();
    }
}

/// Run `cmd`, assert it succeeded and parse its stdout as one JSON value.
fn json_of(cmd: &mut Command) -> Value {
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{out:?}");
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {out:?}"))
}

/// Run `cmd`, assert its exit code and parse stdout as a JSON error.
fn error_of(cmd: &mut Command, code: i32) -> Value {
    let out = cmd.output().unwrap();
    assert_eq!(out.status.code(), Some(code), "{out:?}");
    let body: Value =
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {out:?}"));
    body["error"].clone()
}

fn user(tag: &str) -> Caller {
    Caller {
        holder: HolderRef::User { tag: tag.into() },
        project: None,
        override_reason: None,
    }
}

fn agent_caller(session: &str, cwd: &Path) -> Caller {
    Caller {
        holder: HolderRef::AgentSession {
            session_id: session.into(),
        },
        project: Some(ProjectResolver::resolve(cwd).unwrap()),
        override_reason: None,
    }
}

fn acquire(env: &Env, caller: &Caller, resource: &str) -> Value {
    env.rpc(
        "acquire",
        caller,
        json!({"target": {"resource": resource}, "duration_ms": 600_000, "purpose": "flash it"}),
    )
    .unwrap()
}

/// A user-scope generic resource `id` with one unit `u1` keyed `key`.
fn seed(env: &Env, id: &str, key: &str) {
    env.ok(&[
        "resource",
        "create",
        id,
        "--type",
        "generic",
        "--description",
        "A test board",
        "--tag",
        "esp32,thread",
    ]);
    env.ok(&[
        "unit",
        "add",
        id,
        "u1",
        "--key",
        key,
        "--description",
        "The left one",
    ]);
}

/// The fields of a resource that do not depend on when it was made.
fn stable(resource: &Value) -> Value {
    let mut r = resource.clone();
    r.as_object_mut().unwrap().remove("created_at");
    for unit in r["units"].as_array_mut().unwrap() {
        unit.as_object_mut().unwrap().remove("created_at");
    }
    r
}

fn canonical(dir: &Path) -> String {
    dir.canonicalize().unwrap().to_string_lossy().into_owned()
}

/// Hold the daemon lock and a socket nobody serves, as a wedged daemon
/// would.
fn wedge(paths: &Paths) -> (fs::File, UnixListener) {
    fs::create_dir_all(paths.socket.parent().unwrap()).unwrap();
    let mut lock = fs::File::create(&paths.lock).unwrap();
    assert!(FileExt::try_lock_exclusive(&lock).unwrap());
    lock.write_all(format!("{}\n", std::process::id()).as_bytes())
        .unwrap();
    let listener = UnixListener::bind(&paths.socket).unwrap();
    (lock, listener)
}

/// A `CLERK_HOME` below `env` that cannot be written to.
fn unwritable_home(env: &Env) -> PathBuf {
    let home = env.home().join("readonly");
    fs::create_dir_all(&home).unwrap();
    fs::set_permissions(&home, fs::Permissions::from_mode(0o555)).unwrap();
    home
}

fn restore(home: &Path) {
    fs::set_permissions(home, fs::Permissions::from_mode(0o755)).unwrap();
}

// ---------------------------------------------------------------------------
// Behaviour
// ---------------------------------------------------------------------------

mod cli {
    use super::*;

    mod output {
        use super::*;

        mod on_stdout_tty {
            use super::*;

            #[test]
            fn prints_human_text() {
                let env = Env::new();
                seed(&env, "board", "k1");
                let shown = env.tty("\"$CLERK\" resources");
                assert!(shown.contains("board"), "{shown}");
                assert!(shown.contains("A test board"), "{shown}");
                assert!(
                    serde_json::from_str::<Value>(shown.trim()).is_err(),
                    "{shown}"
                );
            }
        }

        mod on_json_flag {
            use super::*;

            #[test]
            fn emits_json_on_a_tty() {
                let env = Env::new();
                seed(&env, "board", "k1");
                let shown = env.tty("\"$CLERK\" resources --json");
                let body: Value = serde_json::from_str(shown.trim()).unwrap();
                assert_eq!(body[0]["id"], "board");
            }
        }

        mod on_non_tty {
            use super::*;

            #[test]
            fn emits_json_only_on_stdout() {
                let env = Env::new();
                seed(&env, "board", "k1");
                let body = env.ok(&["resources"]);
                assert_eq!(body[0]["id"], "board");
            }

            #[test]
            fn logs_never_reach_stdout() {
                let env = Env::new();
                let home = unwritable_home(&env);
                let out = env
                    .cmd()
                    .env("CLERK_HOME", &home)
                    .arg("status")
                    .output()
                    .unwrap();
                restore(&home);
                let body: Value = serde_json::from_slice(&out.stdout).unwrap();
                assert_eq!(body["error"]["code"], "daemon_unavailable");
                let stderr = String::from_utf8_lossy(&out.stderr);
                assert!(stderr.contains("cannot start the clerk daemon"), "{stderr}");
            }
        }
    }

    mod inventory {
        use super::*;

        mod resource_create {
            use super::*;

            mod on_flags {
                use super::*;

                #[test]
                fn sends_every_flag_to_create_resource() {
                    let env = Env::new();
                    let body = env.ok(&[
                        "resource",
                        "create",
                        "probe",
                        "--type",
                        "device",
                        "--description",
                        "A debug probe",
                        "--usage",
                        "Flash with {unit.path}",
                        "--tag",
                        "swd,arm",
                        "--tag",
                        "probe",
                        "--health",
                        "exists,accessible",
                        "--health-command",
                        "true",
                    ]);
                    let r = &body["resource"];
                    assert_eq!(r["id"], "probe");
                    assert_eq!(r["type"], "device");
                    assert_eq!(r["description"], "A debug probe");
                    assert_eq!(r["usage"], "Flash with {unit.path}");
                    assert_eq!(r["tags"], json!(["arm", "probe", "swd"]));
                    assert_eq!(
                        r["health"],
                        json!([{"builtin": "exists"}, {"builtin": "accessible"}, {"custom": "true"}])
                    );
                }

                #[test]
                fn defaults_to_user_scope() {
                    let env = Env::new();
                    let body = env.ok(&["resource", "create", "probe", "--type", "generic"]);
                    assert_eq!(body["resource"]["scope"], "user");
                }

                #[test]
                fn leaves_health_at_the_type_default_without_health_flags() {
                    let env = Env::new();
                    let body = env.ok(&["resource", "create", "probe", "--type", "generic"]);
                    assert_eq!(body["resource"]["health"], Value::Null);
                }
            }

            mod on_scope_project {
                use super::*;

                #[test]
                fn uses_the_callers_project() {
                    let env = Env::new();
                    let dir = env.dir("alpha");
                    let body = json_of(env.cmd().current_dir(&dir).args([
                        "resource", "create", "probe", "--type", "generic", "--scope", "project",
                    ]));
                    assert_eq!(
                        body["resource"]["scope"],
                        json!({"project": canonical(&dir)})
                    );
                }
            }

            mod on_scope_project_without_project {
                use super::*;

                #[test]
                fn exits_2_without_daemon_call() {
                    let env = Env::new();
                    let base = env.dir("base");
                    // The working directory is gone by the time clerk runs.
                    let out = Command::new("sh")
                        .current_dir(&base)
                        .env("CLERK", env!("CARGO_BIN_EXE_clerk"))
                        .env("CLERK_HOME", env.home())
                        .env_remove(SESSION_VAR)
                        .args([
                            "-c",
                            "mkdir gone && cd gone && rmdir ../gone && exec \"$CLERK\" resource \
                             create probe --type generic --scope project",
                        ])
                        .timeout(PATIENCE)
                        .output()
                        .unwrap();
                    assert_eq!(out.status.code(), Some(2), "{out:?}");
                    let body: Value = serde_json::from_slice(&out.stdout).unwrap();
                    assert!(
                        body["error"]["message"]
                            .as_str()
                            .unwrap()
                            .contains("project"),
                        "{body}"
                    );
                    assert!(!env.daemon_started());
                }
            }

            mod on_usage_file {
                use super::*;

                #[test]
                fn sends_the_file_contents() {
                    let env = Env::new();
                    let file = env.home().join("usage.md");
                    fs::write(&file, "# Probe\n\nUse {unit.identity_key}.\n").unwrap();
                    let body = env.ok(&[
                        "resource",
                        "create",
                        "probe",
                        "--type",
                        "generic",
                        "--usage-file",
                        file.to_str().unwrap(),
                    ]);
                    assert_eq!(
                        body["resource"]["usage"],
                        "# Probe\n\nUse {unit.identity_key}.\n"
                    );
                }
            }
        }

        mod on_duplicate_in_scope {
            use super::*;

            #[test]
            fn exits_4_with_error_json() {
                let env = Env::new();
                seed(&env, "board", "k1");
                let error = error_of(
                    env.cmd()
                        .args(["unit", "add", "board", "u2", "--key", "k1"]),
                    4,
                );
                assert_eq!(error["code"], "duplicate_in_scope");
                assert_eq!(error["details"]["unit"], "u1");
            }
        }

        mod on_similar_in_other_project {
            use super::*;

            #[test]
            fn prints_warning_with_promotion_id() {
                let env = Env::new();
                let (a, b) = (env.dir("alpha"), env.dir("beta"));
                for (dir, id) in [(&a, "a-board"), (&b, "b-board")] {
                    json_of(env.cmd().current_dir(dir).args([
                        "resource", "create", id, "--type", "generic", "--scope", "project",
                    ]));
                }
                json_of(
                    env.cmd()
                        .current_dir(&a)
                        .args(["unit", "add", "a-board", "u", "--key", "k1"]),
                );
                let body = json_of(
                    env.cmd()
                        .current_dir(&b)
                        .args(["unit", "add", "b-board", "u", "--key", "k1"]),
                );
                let warning = &body["warnings"][0];
                assert_eq!(warning["kind"], "similar_in_other_project");
                assert_eq!(warning["project"], canonical(&a));
                assert_ne!(warning["promotion_id"].as_str().unwrap(), "");
            }
        }

        mod on_unknown_resource {
            use super::*;

            #[test]
            fn exits_3() {
                let env = Env::new();
                let error = error_of(env.cmd().args(["resource", "show", "ghost"]), 3);
                assert_eq!(error["code"], "not_found");
            }
        }

        mod on_invalid_value {
            use super::*;

            #[test]
            fn exits_2_with_the_daemons_code() {
                let env = Env::new();
                let error = error_of(
                    env.cmd().args([
                        "resource", "create", "probe", "--type", "generic", "--tag", "ESP32",
                    ]),
                    2,
                );
                assert_eq!(error["code"], "invalid_tag");
                assert_eq!(error["details"], json!({"tag": "ESP32"}));
            }
        }

        mod resource_update {
            use super::*;

            mod on_add_and_remove_tags {
                use super::*;

                #[test]
                fn applies_both_edits() {
                    let env = Env::new();
                    seed(&env, "board", "k1");
                    let body = env.ok(&[
                        "resource",
                        "update",
                        "board",
                        "--add-tag",
                        "zigbee,devboard",
                        "--remove-tag",
                        "thread",
                    ]);
                    assert_eq!(body["tags"], json!(["devboard", "esp32", "zigbee"]));
                }
            }
        }

        mod resource_delete {
            use super::*;

            #[test]
            fn removes_the_resource() {
                let env = Env::new();
                env.ok(&["resource", "create", "probe", "--type", "generic"]);
                env.ok(&["resource", "delete", "probe"]);
                error_of(env.cmd().args(["resource", "show", "probe"]), 3);
            }
        }

        mod resource_list {
            use super::*;

            #[test]
            fn lists_only_resources_carrying_every_tag() {
                let env = Env::new();
                seed(&env, "board", "k1");
                env.ok(&[
                    "resource", "create", "other", "--type", "generic", "--tag", "esp32",
                ]);
                let body = env.ok(&["resource", "list", "--tag", "esp32", "--tag", "thread"]);
                let ids: Vec<&str> = body
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|r| r["id"].as_str().unwrap())
                    .collect();
                assert_eq!(ids, ["board"]);
            }
        }

        mod resource_show {
            use super::*;

            #[test]
            fn prints_the_raw_usage_template() {
                let env = Env::new();
                env.ok(&[
                    "resource",
                    "create",
                    "probe",
                    "--type",
                    "generic",
                    "--usage",
                    "Use key {unit.identity_key} carefully",
                ]);
                env.ok(&["unit", "add", "probe", "u1", "--key", "k9"]);
                let shown = env.tty("\"$CLERK\" resource show probe");
                assert!(
                    shown.contains("Use key {unit.identity_key} carefully"),
                    "{shown}"
                );
            }
        }

        mod unit_add {
            use super::*;

            mod on_service_flags {
                use super::*;

                #[test]
                fn sends_service_settings() {
                    let env = Env::new();
                    env.ok(&["resource", "create", "api", "--type", "service"]);
                    let body = env.ok(&[
                        "unit",
                        "add",
                        "api",
                        "dev",
                        "--host",
                        "127.0.0.1",
                        "--port",
                        "18081",
                        "--listen",
                        "--scheme",
                        "http",
                        "--service-id",
                        "api-dev",
                        "--description",
                        "Dev server",
                    ]);
                    assert_eq!(
                        body["unit"]["settings"],
                        json!({"type": "service", "mode": "listen", "scheme": "http",
                               "host": "127.0.0.1", "port": 18081, "service_id": "api-dev"})
                    );
                    assert_eq!(body["unit"]["description"], "Dev server");
                }
            }

            mod on_device_flags {
                use super::*;

                #[test]
                fn sends_device_settings() {
                    let env = Env::new();
                    env.ok(&["resource", "create", "tty", "--type", "device"]);
                    let body =
                        env.ok(&["unit", "add", "tty", "u1", "--path", "/dev/null-clerk-test"]);
                    assert_eq!(
                        body["unit"]["settings"],
                        json!({"type": "device", "path": "/dev/null-clerk-test"})
                    );
                }
            }

            mod on_settings_of_two_kinds {
                use super::*;

                #[test]
                fn exits_2_without_daemon_call() {
                    let env = Env::new();
                    let error = error_of(
                        env.cmd()
                            .args(["unit", "add", "r", "u", "--path", "/dev/x", "--key", "k"]),
                        2,
                    );
                    assert_eq!(error["code"], "usage_error");
                    assert!(!env.daemon_started());
                }
            }
        }

        mod unit_update {
            use super::*;

            mod on_one_setting {
                use super::*;

                #[test]
                fn keeps_the_other_settings() {
                    let env = Env::new();
                    env.ok(&["resource", "create", "api", "--type", "service"]);
                    env.ok(&[
                        "unit",
                        "add",
                        "api",
                        "dev",
                        "--host",
                        "127.0.0.1",
                        "--port",
                        "18081",
                        "--scheme",
                        "http",
                    ]);
                    let body = env.ok(&["unit", "update", "api", "dev", "--port", "18082"]);
                    assert_eq!(
                        body["unit"]["settings"],
                        json!({"type": "service", "mode": "connect", "scheme": "http",
                               "host": "127.0.0.1", "port": 18082, "service_id": null})
                    );
                }
            }
        }

        mod unit_remove {
            use super::*;

            #[test]
            fn removes_the_unit() {
                let env = Env::new();
                seed(&env, "board", "k1");
                env.ok(&["unit", "remove", "board", "u1"]);
                let body = env.ok(&["resource", "show", "board"]);
                assert_eq!(body["units"], json!([]));
            }
        }

        mod unit_retire {
            use super::*;

            #[test]
            fn marks_a_held_unit_retiring() {
                let env = Env::new();
                seed(&env, "board", "k1");
                acquire(&env, &user("holder"), "board");
                env.ok(&["unit", "retire", "board", "u1"]);
                let body = env.ok(&["resource", "show", "board"]);
                assert_eq!(body["units"][0]["retiring"], true);
            }
        }
    }

    mod yaml_input {
        use super::*;

        mod on_a_document_with_the_commands_fields {
            use super::*;

            #[test]
            fn sends_the_same_call_as_the_equivalent_flags() {
                let flags = Env::new();
                let by_flags = flags.ok(&[
                    "resource",
                    "create",
                    "probe",
                    "--type",
                    "device",
                    "--description",
                    "A debug probe",
                    "--usage",
                    "Flash it",
                    "--tag",
                    "swd,arm",
                    "--health",
                    "exists",
                ]);
                let yaml = Env::new();
                let file = yaml.home().join("probe.yaml");
                fs::write(
                    &file,
                    "type: device\ndescription: A debug probe\nusage: Flash it\n\
                     tags: [swd, arm]\nhealth: [{builtin: exists}]\n",
                )
                .unwrap();
                let by_yaml =
                    yaml.ok(&["resource", "create", "probe", "-f", file.to_str().unwrap()]);
                assert_eq!(stable(&by_yaml["resource"]), stable(&by_flags["resource"]));
            }

            #[test]
            fn delivers_a_multi_line_usage_block_byte_for_byte() {
                let env = Env::new();
                let file = env.home().join("probe.yaml");
                fs::write(
                    &file,
                    "type: generic\nusage: |\n  # Probe\n\n    indented: ${PORT}\n  last line\n",
                )
                .unwrap();
                let body = env.ok(&["resource", "create", "probe", "-f", file.to_str().unwrap()]);
                assert_eq!(
                    body["resource"]["usage"],
                    "# Probe\n\n  indented: ${PORT}\nlast line\n"
                );
            }

            #[test]
            fn adds_a_unit_like_the_equivalent_flags() {
                let env = Env::new();
                env.ok(&["resource", "create", "api", "--type", "service"]);
                let file = env.home().join("unit.yaml");
                fs::write(
                    &file,
                    "host: 127.0.0.1\nport: 18081\nmode: listen\ndescription: Dev server\n",
                )
                .unwrap();
                let body = env.ok(&["unit", "add", "api", "dev", "-f", file.to_str().unwrap()]);
                assert_eq!(
                    body["unit"]["settings"],
                    json!({"type": "service", "mode": "listen", "scheme": null,
                           "host": "127.0.0.1", "port": 18081, "service_id": null})
                );
                assert_eq!(body["unit"]["description"], "Dev server");
            }
        }

        mod on_heredoc_usage_block {
            use super::*;

            #[test]
            fn matches_flag_equivalent() {
                let usage = "Connect:\n  ssh {unit.identity_key}\n";
                let flags = Env::new();
                let by_flags = flags.ok(&[
                    "resource", "create", "box", "--type", "generic", "--usage", usage,
                ]);
                let yaml = Env::new();
                let by_yaml = json_of(
                    yaml.cmd()
                        .args(["resource", "create", "box", "-f", "-"])
                        .write_stdin(
                            "type: generic\nusage: |\n  Connect:\n    ssh {unit.identity_key}\n",
                        ),
                );
                assert_eq!(by_yaml["resource"]["usage"], usage);
                assert_eq!(stable(&by_yaml["resource"]), stable(&by_flags["resource"]));
            }
        }

        mod on_no_file_flag_with_stdin_a_pipe_that_never_closes {
            use super::*;

            #[test]
            fn never_reads_stdin() {
                let env = Env::new();
                let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_clerk"))
                    .args(["resource", "create", "probe", "--type", "generic"])
                    .env("CLERK_HOME", env.home())
                    .env_remove(SESSION_VAR)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap();
                let _open_stdin = child.stdin.take();
                let status = wait_within(&mut child, Duration::from_secs(10));
                assert_eq!(status.and_then(|s| s.code()), Some(0));
            }
        }

        mod on_update_with_a_subset_of_keys {
            use super::*;

            #[test]
            fn changes_only_those_fields() {
                let env = Env::new();
                seed(&env, "board", "k1");
                env.ok(&["resource", "update", "board", "--usage", "Keep me"]);
                let file = env.home().join("patch.yaml");
                fs::write(&file, "description: A new text\n").unwrap();
                let body = env.ok(&["resource", "update", "board", "-f", file.to_str().unwrap()]);
                assert_eq!(body["description"], "A new text");
                assert_eq!(body["usage"], "Keep me");
                assert_eq!(body["tags"], json!(["esp32", "thread"]));
            }
        }

        mod on_flag_and_key_both_set {
            use super::*;

            #[test]
            fn exits_2_naming_field() {
                let env = Env::new();
                let error = error_of(
                    env.cmd()
                        .args([
                            "resource",
                            "create",
                            "probe",
                            "--description",
                            "x",
                            "-f",
                            "-",
                        ])
                        .write_stdin("type: generic\ndescription: from the file\n"),
                    2,
                );
                assert!(
                    error["message"].as_str().unwrap().contains("description"),
                    "{error}"
                );
                assert!(!env.daemon_started());
            }
        }

        mod on_unknown_key {
            use super::*;

            #[test]
            fn exits_2_naming_the_key() {
                let env = Env::new();
                let error = error_of(
                    env.cmd()
                        .args(["resource", "create", "probe", "-f", "-"])
                        .write_stdin("type: generic\ncolour: red\n"),
                    2,
                );
                assert!(
                    error["message"].as_str().unwrap().contains("colour"),
                    "{error}"
                );
                assert!(!env.daemon_started());
            }
        }

        mod on_malformed_yaml {
            use super::*;

            fn rejects(doc: &str) -> String {
                let env = Env::new();
                let error = error_of(
                    env.cmd()
                        .args(["resource", "create", "probe", "-f", "-"])
                        .write_stdin(doc.to_owned()),
                    2,
                );
                assert!(!env.daemon_started());
                error["message"].as_str().unwrap().to_owned()
            }

            #[test]
            fn exits_2_with_line_and_column() {
                let message = rejects("type: generic\nusage: a: b\n");
                assert!(message.contains("line 2, column"), "{message}");
            }

            #[test]
            fn exits_2_on_a_document_that_is_not_a_mapping() {
                let message = rejects("- type\n- generic\n");
                assert!(message.contains("line 1, column 1"), "{message}");
            }

            #[test]
            fn exits_2_on_more_than_one_document() {
                let message = rejects("type: generic\n---\ntype: device\n");
                assert!(message.contains("line 3, column 1"), "{message}");
            }
        }

        mod on_input_over_64_kib {
            use super::*;

            #[test]
            fn exits_2_without_daemon_call() {
                let env = Env::new();
                let doc = format!("type: generic\nusage: \"{}\"\n", "x".repeat(70 * 1024));
                let error = error_of(
                    env.cmd()
                        .args(["resource", "create", "probe", "-f", "-"])
                        .write_stdin(doc),
                    2,
                );
                assert!(
                    error["message"].as_str().unwrap().contains("64 KiB"),
                    "{error}"
                );
                assert!(!env.daemon_started());
            }
        }

        mod on_numeric_tag {
            use super::*;

            #[test]
            fn exits_2_asking_to_quote() {
                let env = Env::new();
                let error = error_of(
                    env.cmd()
                        .args(["resource", "create", "probe", "-f", "-"])
                        .write_stdin("type: generic\ntags: [esp32, 2024, no]\n"),
                    2,
                );
                let message = error["message"].as_str().unwrap();
                assert!(message.contains("2024"), "{message}");
                assert!(message.contains("quoted"), "{message}");
                assert!(!env.daemon_started());
            }
        }

        mod on_missing_file {
            use super::*;

            #[test]
            fn exits_2_naming_the_path() {
                let env = Env::new();
                let missing = env.home().join("nope.yaml");
                let error = error_of(
                    env.cmd().args([
                        "resource",
                        "create",
                        "probe",
                        "-f",
                        missing.to_str().unwrap(),
                    ]),
                    2,
                );
                assert!(
                    error["message"]
                        .as_str()
                        .unwrap()
                        .contains(missing.to_str().unwrap()),
                    "{error}"
                );
            }
        }
    }

    mod caller {
        use super::*;

        /// The actor of the newest `resource_created` audit entry.
        fn creator(env: &Env) -> Value {
            let body = env.ok(&["log", "--action", "resource_created", "--limit", "1"]);
            body[0]["actor"].clone()
        }

        mod on_session_id_set {
            use super::*;

            #[test]
            fn holder_is_the_agent_session() {
                let env = Env::new();
                let dir = env.dir("alpha");
                json_of(
                    env.agent("sess-1", &dir)
                        .args(["resource", "create", "probe", "--type", "generic"]),
                );
                assert_eq!(
                    creator(&env),
                    json!({"kind": "agent_session", "session_id": "sess-1"})
                );
            }

            #[test]
            fn project_comes_from_the_working_directory() {
                let env = Env::new();
                let dir = env.dir("alpha");
                let body = json_of(env.agent("sess-1", &dir).args([
                    "resource", "create", "probe", "--type", "generic", "--scope", "project",
                ]));
                assert_eq!(
                    body["resource"]["scope"],
                    json!({"project": canonical(&dir)})
                );
            }
        }

        mod on_cli_without_session_id {
            use super::*;

            #[test]
            fn holder_is_this_cli_process() {
                let env = Env::new();
                env.ok(&["resource", "create", "probe", "--type", "generic"]);
                let actor = creator(&env);
                assert_eq!(actor["kind"], "cli_process");
                assert!(actor["pid"].as_u64().is_some(), "{actor}");
                assert!(actor["start_time"].as_u64().is_some(), "{actor}");
            }
        }

        mod on_cwd_with_spaces_and_unicode {
            use super::*;

            #[test]
            fn resolves_project() {
                let env = Env::new();
                let dir = env.dir("my project ä 名");
                let body = json_of(env.cmd().current_dir(&dir).args([
                    "resource", "create", "probe", "--type", "generic", "--scope", "project",
                ]));
                assert_eq!(
                    body["resource"]["scope"],
                    json!({"project": canonical(&dir)})
                );
                let listed = json_of(
                    env.cmd()
                        .current_dir(&dir)
                        .args(["resource", "show", "probe"]),
                );
                assert_eq!(listed["id"], "probe");
            }
        }

        mod on_empty_override_reason {
            use super::*;

            #[test]
            fn exits_2() {
                let env = Env::new();
                let error = error_of(env.cmd().args(["status", "--override", "  "]), 2);
                assert_eq!(error["code"], "usage_error");
                assert!(!env.daemon_started());
            }
        }
    }

    mod daemon_unavailable {
        use super::*;

        mod on_unwritable_state_dir {
            use super::*;

            #[test]
            fn exits_5_within_6s() {
                let env = Env::new();
                let home = unwritable_home(&env);
                let started = Instant::now();
                let out = env
                    .cmd()
                    .env("CLERK_HOME", &home)
                    .arg("status")
                    .output()
                    .unwrap();
                let took = started.elapsed();
                restore(&home);
                assert_eq!(out.status.code(), Some(5), "{out:?}");
                assert!(took < Duration::from_secs(6), "{took:?}");
                let body: Value = serde_json::from_slice(&out.stdout).unwrap();
                let message = body["error"]["message"].as_str().unwrap();
                assert!(message.starts_with("the clerk daemon"), "{message}");
            }
        }

        mod on_unresponsive_daemon {
            use super::*;

            #[test]
            fn exits_5_naming_pid() {
                let env = Env::new();
                let _wedged = wedge(&env.paths());
                let error = error_of(env.cmd().arg("status"), 5);
                assert_eq!(error["code"], "daemon_unresponsive");
                let message = error["message"].as_str().unwrap();
                assert!(
                    message.contains(&format!("pid {}", std::process::id())),
                    "{message}"
                );
            }
        }

        mod on_version_mismatch {
            use super::*;

            #[test]
            fn exits_5_saying_daemon_restart() {
                let env = Env::new();
                let error = error_of(
                    env.cmd()
                        .env("CLERK_TEST_DAEMON_VERSION", "0.0.0-other")
                        .arg("status"),
                    5,
                );
                assert_eq!(error["code"], "version_mismatch");
                let message = error["message"].as_str().unwrap();
                assert!(message.contains("clerk daemon restart"), "{message}");
            }
        }
    }

    mod error_output {
        use super::*;

        fn bad_usage(cmd: &mut Command) -> &mut Command {
            cmd.args([
                "resource",
                "create",
                "probe",
                "--type",
                "generic",
                "--usage",
                "Open {unit.path}",
            ])
        }

        mod on_unknown_placeholder {
            use super::*;

            #[test]
            fn json_details_carry_valid_keys() {
                let env = Env::new();
                let error = error_of(bad_usage(&mut env.cmd()), 2);
                assert_eq!(error["code"], "unknown_placeholder");
                assert_eq!(error["details"]["placeholder"], "unit.path");
                assert!(
                    error["details"]["valid"]
                        .as_array()
                        .unwrap()
                        .contains(&json!("unit.identity_key")),
                    "{error}"
                );
            }

            #[test]
            fn human_prints_one_key_value_line_per_detail() {
                let env = Env::new();
                let shown = env.tty(
                    "\"$CLERK\" resource create probe --type generic --usage 'Open {unit.path}'",
                );
                assert!(shown.contains("\n  placeholder: unit.path\n"), "{shown}");
                assert!(shown.contains("\n  valid: "), "{shown}");
                assert!(shown.contains("unit.identity_key"), "{shown}");
            }
        }

        mod on_error_without_details {
            use super::*;

            #[test]
            fn json_has_no_details_key() {
                let env = Env::new();
                let error = error_of(env.cmd().args(["resource", "show", "ghost"]), 3);
                assert!(error.get("details").is_none(), "{error}");
            }
        }
    }

    mod status {
        use super::*;

        #[test]
        fn shows_descriptions_and_tags() {
            let env = Env::new();
            seed(&env, "board", "k1");
            let body = env.ok(&["status"]);
            let r = &body["resources"][0];
            assert_eq!(r["description"], "A test board");
            assert_eq!(r["tags"], json!(["esp32", "thread"]));
            assert_eq!(r["units"][0]["description"], "The left one");
        }

        #[test]
        fn human_truncates_descriptions_to_the_terminal_width() {
            let env = Env::new();
            let long = format!("Board {}", "x".repeat(150));
            env.ok(&[
                "resource",
                "create",
                "board",
                "--type",
                "generic",
                "--description",
                &long,
            ]);
            let shown = env.tty("stty cols 60 rows 20; \"$CLERK\" status");
            assert!(!shown.contains(&long), "{shown}");
            assert!(shown.contains("Board xxx"), "{shown}");
            for line in shown.lines() {
                assert!(line.chars().count() <= 60, "{line:?} in {shown}");
            }
        }

        mod on_holder_in_an_invisible_project {
            use super::*;

            #[test]
            fn shows_held_by_an_agent_in_another_project() {
                let env = Env::new();
                let (a, b) = (env.dir("alpha"), env.dir("beta"));
                seed(&env, "shared", "k1");
                json_of(env.agent("sess-a", &a).args([
                    "resource", "create", "a-board", "--type", "generic", "--scope", "project",
                ]));
                json_of(
                    env.agent("sess-a", &a)
                        .args(["unit", "add", "a-board", "u", "--key", "k1"]),
                );
                acquire(&env, &agent_caller("sess-a", &a), "a-board");
                let shown = env.tty(&format!(
                    "cd '{}' && {SESSION_VAR}=sess-b \"$CLERK\" status",
                    b.display()
                ));
                assert!(
                    shown.contains("held by an agent in another project"),
                    "{shown}"
                );
            }
        }

        mod on_agent_session {
            use super::*;

            #[test]
            fn omits_other_projects_resources() {
                let env = Env::new();
                let (a, b) = (env.dir("alpha"), env.dir("beta"));
                json_of(env.agent("sess-a", &a).args([
                    "resource", "create", "a-board", "--type", "generic", "--scope", "project",
                ]));
                let body = json_of(env.agent("sess-b", &b).arg("status"));
                assert_eq!(body["resources"], json!([]));
            }
        }

        mod on_user {
            use super::*;

            #[test]
            fn shows_every_projects_resources() {
                let env = Env::new();
                let a = env.dir("alpha");
                json_of(env.agent("sess-a", &a).args([
                    "resource", "create", "a-board", "--type", "generic", "--scope", "project",
                ]));
                let body = env.ok(&["status"]);
                assert_eq!(body["resources"][0]["id"], "a-board");
            }
        }
    }

    mod resources {
        use super::*;

        #[test]
        fn human_list_shows_tags() {
            let env = Env::new();
            seed(&env, "board", "k1");
            let shown = env.tty("\"$CLERK\" resources --tag esp32");
            assert!(shown.contains("esp32, thread"), "{shown}");
        }
    }

    mod views {
        use super::*;

        mod on_stdin_pipe_never_closed {
            use super::*;

            #[test]
            fn exits_without_reading_stdin() {
                let env = Env::new();
                let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_clerk"))
                    .arg("status")
                    .env("CLERK_HOME", env.home())
                    .env_remove(SESSION_VAR)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap();
                let _open_stdin = child.stdin.take();
                let status = wait_within(&mut child, Duration::from_secs(5));
                assert_eq!(status.and_then(|s| s.code()), Some(0));
            }
        }
    }

    mod leases {
        use super::*;

        mod without_mine {
            use super::*;

            #[test]
            fn lists_every_lease_from_the_status_view() {
                let env = Env::new();
                let a = env.dir("alpha");
                seed(&env, "board", "k1");
                json_of(env.agent("sess-a", &a).args([
                    "resource", "create", "a-board", "--type", "generic", "--scope", "project",
                ]));
                json_of(
                    env.agent("sess-a", &a)
                        .args(["unit", "add", "a-board", "u", "--key", "k2"]),
                );
                let mine = acquire(&env, &user("me"), "board");
                let theirs = acquire(&env, &agent_caller("sess-a", &a), "a-board");
                let body = env.ok(&["leases"]);
                let mut ids: Vec<&str> = body
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|l| l["occupancy"]["lease_id"].as_str().unwrap())
                    .collect();
                ids.sort_unstable();
                let mut want = [
                    mine["lease_id"].as_str().unwrap(),
                    theirs["lease_id"].as_str().unwrap(),
                ];
                want.sort_unstable();
                assert_eq!(ids, want);
            }
        }

        mod on_mine {
            use super::*;

            #[test]
            fn prints_each_lease_with_its_brief() {
                let env = Env::new();
                let a = env.dir("alpha");
                env.ok(&[
                    "resource",
                    "create",
                    "board",
                    "--type",
                    "generic",
                    "--description",
                    "A test board",
                    "--usage",
                    "Talk to {unit.identity_key}",
                    "--tag",
                    "esp32",
                ]);
                env.ok(&[
                    "unit",
                    "add",
                    "board",
                    "u1",
                    "--key",
                    "k1",
                    "--description",
                    "Left",
                ]);
                acquire(&env, &agent_caller("sess-a", &a), "board");
                acquire(&env, &user("other"), "board");
                let body = json_of(env.agent("sess-a", &a).args(["leases", "--mine"]));
                let leases = body["leases"].as_array().unwrap();
                assert_eq!(leases.len(), 1, "{body}");
                assert_eq!(leases[0]["resource_description"], "A test board");
                assert_eq!(leases[0]["unit_description"], "Left");
                assert_eq!(leases[0]["usage"], "Talk to k1");
                assert_eq!(leases[0]["resource_tags"], json!(["esp32"]));
            }
        }

        mod on_mine_without_session_id {
            use super::*;

            #[test]
            fn exits_2_explaining_it_needs_an_agent_session() {
                let env = Env::new();
                let error = error_of(env.cmd().args(["leases", "--mine"]), 2);
                assert!(
                    error["message"].as_str().unwrap().contains("agent session"),
                    "{error}"
                );
            }
        }
    }

    mod queue {
        use super::*;

        #[test]
        fn lists_the_queued_requests() {
            let env = Env::new();
            seed(&env, "board", "k1");
            acquire(&env, &user("first"), "board");
            let queued = acquire(&env, &user("second"), "board");
            let body = env.ok(&["queue", "board"]);
            assert_eq!(body[0]["queue"][0]["request_id"], queued["request_id"]);
            assert_eq!(
                body[0]["queue"][0]["holder"],
                json!({"kind": "user", "tag": "second"})
            );
        }

        mod on_unknown_resource {
            use super::*;

            #[test]
            fn exits_3() {
                let env = Env::new();
                let error = error_of(env.cmd().args(["queue", "ghost"]), 3);
                assert_eq!(error["code"], "not_found");
            }
        }
    }

    mod log {
        use super::*;

        #[test]
        fn filters_by_action() {
            let env = Env::new();
            seed(&env, "board", "k1");
            let body = env.ok(&["log", "--action", "unit_added"]);
            let actions: Vec<&str> = body
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["action"].as_str().unwrap())
                .collect();
            assert_eq!(actions, ["unit_added"]);
        }

        #[test]
        fn since_drops_older_entries() {
            let env = Env::new();
            seed(&env, "board", "k1");
            assert_ne!(env.ok(&["log", "--since", "1h"]), json!([]));
            let at = env.ok(&["log"])[0]["at"].as_i64().unwrap();
            let later = (at + 60_000).to_string();
            assert_eq!(env.ok(&["log", "--since", &later]), json!([]));
        }
    }

    /// Two projects registering the same key: beta's unit raises a
    /// promotion of alpha's resource.
    fn promotion(env: &Env) -> String {
        let (a, b) = (env.dir("alpha"), env.dir("beta"));
        for (dir, id) in [(&a, "a-board"), (&b, "b-board")] {
            json_of(env.cmd().current_dir(dir).args([
                "resource", "create", id, "--type", "generic", "--scope", "project",
            ]));
            json_of(
                env.cmd()
                    .current_dir(dir)
                    .args(["unit", "add", id, "u", "--key", "k1"]),
            );
        }
        let body = env.ok(&["promotions"]);
        body[0]["id"].as_str().unwrap().to_owned()
    }

    mod promotions {
        use super::*;

        #[test]
        fn lists_pending_promotions() {
            let env = Env::new();
            promotion(&env);
            let body = env.ok(&["promotions"]);
            assert_eq!(body[0]["resource"], "a-board");
        }
    }

    mod approve {
        use super::*;

        mod on_agent_without_override_reason {
            use super::*;

            #[test]
            fn exits_4() {
                let env = Env::new();
                let dir = env.dir("alpha");
                let error = error_of(env.agent("sess-a", &dir).args(["approve", "prom_x"]), 4);
                assert_eq!(error["code"], "override_required");
                assert!(!env.daemon_started());
            }
        }

        mod on_agent_with_override_reason {
            use super::*;

            #[test]
            fn approves_and_records_the_reason() {
                let env = Env::new();
                let id = promotion(&env);
                let dir = env.dir("gamma");
                json_of(env.agent("sess-a", &dir).args([
                    "approve",
                    &id,
                    "--override",
                    "the user asked to share it",
                ]));
                let entry = &env.ok(&["log", "--limit", "1"])[0];
                assert_eq!(entry["override_reason"], "the user asked to share it");
            }
        }

        mod on_user {
            use super::*;

            #[test]
            fn promotes_the_resource_to_user_scope() {
                let env = Env::new();
                let id = promotion(&env);
                let body = env.ok(&["approve", &id, "--as", "board"]);
                assert_eq!(body["id"], "board");
                assert_eq!(body["scope"], "user");
            }
        }
    }
}

/// Wait up to `limit` for `child`; kill it when it overruns.
fn wait_within(
    child: &mut std::process::Child,
    limit: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
