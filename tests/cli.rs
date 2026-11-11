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

use common::{AgentHosts, TestEnv};

/// Upper bound for one command; generous so a loaded machine never flakes.
const PATIENCE: Duration = Duration::from_secs(30);
/// The variable that marks an agent session.
const SESSION_VAR: &str = "CLAUDE_CODE_SESSION_ID";
const CLAUDE_PID: &str = "CLAUDE_PID";

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A private clerk home. Dropping it stops the daemon its commands started.
struct Env {
    env: TestEnv,
    hosts: AgentHosts,
}

impl Env {
    fn new() -> Self {
        Self {
            env: TestEnv::new(),
            hosts: AgentHosts::default(),
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
        cmd.env_remove(SESSION_VAR)
            .env_remove(CLAUDE_PID)
            .timeout(PATIENCE);
        cmd
    }

    /// A clerk command run by agent session `session` in `cwd`, hosted by
    /// this test process (alive for the whole test).
    fn agent(&self, session: &str, cwd: &Path) -> Command {
        let mut cmd = self.cmd();
        cmd.arg("--json")
            .env(SESSION_VAR, session)
            .env(CLAUDE_PID, std::process::id().to_string())
            .current_dir(cwd);
        cmd
    }

    /// As [`Env::agent`], without `--json`: the agent's default Markdown.
    fn agent_md(&self, session: &str, cwd: &Path) -> Command {
        let mut cmd = self.cmd();
        cmd.env(SESSION_VAR, session)
            .env(CLAUDE_PID, std::process::id().to_string())
            .current_dir(cwd);
        cmd
    }

    /// A clerk command run by agent session `session` in `cwd` without
    /// `CLAUDE_PID` (Claude Code older than 2.1.214).
    fn agent_without_pid(&self, session: &str, cwd: &Path) -> Command {
        let mut cmd = self.cmd();
        cmd.arg("--json").env(SESSION_VAR, session).current_dir(cwd);
        cmd
    }

    /// The pid of a process of its own running agent session `session`,
    /// for a test in which this test process runs another session.
    fn own_host(&self, session: &str, cwd: &Path) -> u32 {
        self.hosts.of(&agent_caller(session, cwd)).unwrap().pid
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
            .env_remove(CLAUDE_PID)
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
            let client = Client::connect(&self.paths())
                .await
                .expect("daemon runs")
                .with_host(self.hosts.of(caller));
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
            .env_remove(CLAUDE_PID)
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

/// Project resources in alpha (`sess-a`) and beta (`sess-b`), each with
/// one unit `u`. Returns the two project directories.
fn two_projects(env: &Env, alpha_id: &str, beta_id: &str) -> (PathBuf, PathBuf) {
    let (a, b) = (env.dir("alpha"), env.dir("beta"));
    for (session, dir, id, key) in [
        ("sess-a", &a, alpha_id, "k-a"),
        ("sess-b", &b, beta_id, "k-b"),
    ] {
        json_of(env.agent(session, dir).args([
            "resource", "create", id, "--type", "generic", "--scope", "project",
        ]));
        json_of(
            env.agent(session, dir)
                .args(["unit", "add", id, "u", "--key", key]),
        );
    }
    (a, b)
}

/// Project alpha's resource `probe` with unit `u`, made by agent session
/// `sess-a`; returns alpha's `--scope` value, `project:<path>`.
fn alpha_probe(env: &Env) -> String {
    let dir = env.dir("alpha");
    json_of(env.agent("sess-a", &dir).args([
        "resource", "create", "probe", "--type", "generic", "--scope", "project",
    ]));
    json_of(
        env.agent("sess-a", &dir)
            .args(["unit", "add", "probe", "u", "--key", "k-a"]),
    );
    format!("project:{}", canonical(&dir))
}

/// The values of `key` in a JSON array, as strings.
fn strings(list: &Value, key: &str) -> Vec<String> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|item| item[key].as_str().unwrap().to_owned())
        .collect()
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

    mod usage_errors {
        use super::*;

        /// The MCP server was removed (agents use the CLI).
        #[test]
        fn mcp_is_not_a_command() {
            let env = Env::new();
            let error = error_of(env.cmd().arg("mcp"), 2);
            assert_eq!(error["code"], "usage_error", "{error}");
        }

        mod on_unknown_flag_without_a_tty {
            use super::*;

            #[test]
            fn prints_a_json_error_and_exits_2() {
                let env = Env::new();
                let error = error_of(env.cmd().args(["status", "--bogus"]), 2);
                assert_eq!(error["code"], "usage_error");
                assert!(
                    error["message"].as_str().unwrap().contains("--bogus"),
                    "{error}"
                );
            }
        }

        mod on_missing_value_with_json_flag_on_a_tty {
            use super::*;

            #[test]
            fn prints_a_json_error() {
                let env = Env::new();
                let shown = env.tty("\"$CLERK\" --json log --action; echo \"exit=$?\"");
                let (json_text, exit) = shown.trim().rsplit_once('\n').unwrap();
                let body: Value = serde_json::from_str(json_text.trim()).unwrap();
                assert_eq!(body["error"]["code"], "usage_error");
                assert_eq!(exit, "exit=2");
            }
        }

        mod on_unknown_flag_on_a_tty {
            use super::*;

            #[test]
            fn prints_clap_text() {
                let env = Env::new();
                let shown = env.tty("\"$CLERK\" status --bogus");
                assert!(shown.contains("unexpected argument '--bogus'"), "{shown}");
                assert!(serde_json::from_str::<Value>(shown.trim()).is_err());
            }
        }

        mod on_help {
            use super::*;

            #[test]
            fn prints_help_text_even_without_a_tty() {
                let env = Env::new();
                let out = env.cmd().args(["status", "--help"]).output().unwrap();
                assert_eq!(out.status.code(), Some(0), "{out:?}");
                assert!(String::from_utf8_lossy(&out.stdout).contains("Usage: clerk status"));
            }

            #[test]
            fn wait_help_describes_markdown_blocks_and_json_lines_only_with_json() {
                let env = Env::new();
                let out = env.cmd().args(["wait", "--help"]).output().unwrap();
                let text = String::from_utf8_lossy(&out.stdout).replace('\n', " ");
                let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
                assert!(text.contains("Markdown block per notice"), "{text}");
                assert!(text.contains("JSON lines with --json"), "{text}");
                assert!(!text.contains("print one JSON line per notice"), "{text}");
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

            mod on_agent_session_without_scope {
                use super::*;

                #[test]
                fn defaults_to_the_sessions_project() {
                    let env = Env::new();
                    let dir = env.dir("alpha");
                    let body = json_of(
                        env.agent("sess-1", &dir)
                            .args(["resource", "create", "probe", "--type", "generic"]),
                    );
                    assert_eq!(
                        body["resource"]["scope"],
                        json!({"project": canonical(&dir)})
                    );
                }
            }

            mod on_user_without_scope_inside_a_project {
                use super::*;

                #[test]
                fn defaults_to_user_scope() {
                    let env = Env::new();
                    let dir = env.dir("alpha");
                    let body = json_of(
                        env.cmd()
                            .current_dir(&dir)
                            .args(["resource", "create", "probe", "--type", "generic"]),
                    );
                    assert_eq!(body["resource"]["scope"], "user");
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
                        .env_remove(CLAUDE_PID)
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

            mod on_scope_flag {
                use super::*;

                #[test]
                fn changes_another_projects_resource() {
                    let env = Env::new();
                    let scope = alpha_probe(&env);
                    env.ok(&[
                        "resource",
                        "update",
                        "probe",
                        "--scope",
                        &scope,
                        "--description",
                        "moved",
                    ]);
                    let body = env.ok(&["resource", "show", "probe", "--scope", &scope]);
                    assert_eq!(body["description"], "moved");
                }

                #[test]
                fn agent_session_is_refused_exit_4() {
                    let env = Env::new();
                    let scope = alpha_probe(&env);
                    let dir = env.dir("alpha");
                    let error = error_of(
                        env.agent("sess-a", &dir).args([
                            "resource",
                            "update",
                            "probe",
                            "--scope",
                            &scope,
                            "--description",
                            "x",
                        ]),
                        4,
                    );
                    assert_eq!(error["code"], "scope_not_allowed");
                }
            }

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

            mod on_scope_flag {
                use super::*;

                #[test]
                fn deletes_another_projects_resource() {
                    let env = Env::new();
                    let scope = alpha_probe(&env);
                    env.ok(&["unit", "remove", "probe", "u", "--scope", &scope]);
                    env.ok(&["resource", "delete", "probe", "--scope", &scope]);
                    error_of(
                        env.cmd()
                            .args(["resource", "show", "probe", "--scope", &scope]),
                        3,
                    );
                }
            }

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

            mod on_scope_flag {
                use super::*;

                #[test]
                fn shows_another_projects_resource() {
                    let env = Env::new();
                    let scope = alpha_probe(&env);
                    let body = env.ok(&["resource", "show", "probe", "--scope", &scope]);
                    assert_eq!(body["units"][0]["id"], "u", "{body}");
                }

                #[test]
                fn without_it_another_projects_resource_is_not_found() {
                    let env = Env::new();
                    alpha_probe(&env);
                    let error = error_of(env.cmd().args(["resource", "show", "probe"]), 3);
                    assert_eq!(error["code"], "not_found");
                }

                #[test]
                fn bad_scope_exits_2_without_daemon_call() {
                    let env = Env::new();
                    let error = error_of(
                        env.cmd()
                            .args(["resource", "show", "probe", "--scope", "everywhere"]),
                        2,
                    );
                    assert_eq!(error["code"], "usage_error");
                    assert!(!env.daemon_started());
                }
            }

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

            mod on_scope_flag {
                use super::*;

                #[test]
                fn adds_to_another_projects_resource() {
                    let env = Env::new();
                    let scope = alpha_probe(&env);
                    env.ok(&[
                        "unit", "add", "probe", "u2", "--key", "k-2", "--scope", &scope,
                    ]);
                    let body = env.ok(&["resource", "show", "probe", "--scope", &scope]);
                    assert_eq!(strings(&body["units"], "id"), ["u", "u2"]);
                }
            }

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

            mod on_scope_flag {
                use super::*;

                #[test]
                fn changes_another_projects_unit() {
                    let env = Env::new();
                    let scope = alpha_probe(&env);
                    env.ok(&[
                        "unit", "update", "probe", "u", "--key", "k-new", "--scope", &scope,
                    ]);
                    let body = env.ok(&["resource", "show", "probe", "--scope", &scope]);
                    assert_eq!(body["units"][0]["settings"]["identity_key"], "k-new");
                }
            }

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

            mod on_scope_flag {
                use super::*;

                #[test]
                fn removes_another_projects_unit() {
                    let env = Env::new();
                    let scope = alpha_probe(&env);
                    env.ok(&["unit", "remove", "probe", "u", "--scope", &scope]);
                    let body = env.ok(&["resource", "show", "probe", "--scope", &scope]);
                    assert_eq!(body["units"], json!([]));
                }
            }

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

            mod on_scope_flag {
                use super::*;

                #[test]
                fn retires_another_projects_unit() {
                    let env = Env::new();
                    let scope = alpha_probe(&env);
                    env.ok(&["unit", "retire", "probe", "u", "--scope", &scope]);
                    let body = env.ok(&["resource", "show", "probe", "--scope", &scope]);
                    assert_eq!(body["units"], json!([]));
                }
            }

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
                    .env_remove(CLAUDE_PID)
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

        mod on_session_id_without_claude_pid {
            use super::*;

            #[test]
            fn acquire_fails_with_agent_unidentified_naming_2_1_214() {
                let env = Env::new();
                seed(&env, "board", "k1");
                let dir = env.dir("alpha");
                let error = error_of(
                    env.agent_without_pid("sess-1", &dir)
                        .args(["acquire", "board", "--for", "10m"]),
                    2,
                );
                assert_eq!(error["code"], "agent_unidentified", "{error}");
                assert!(
                    error["message"].as_str().unwrap().contains("2.1.214"),
                    "{error}"
                );
                assert_eq!(env.ok(&["leases"]).as_array().map(Vec::len), Some(0));
            }

            #[test]
            fn read_only_commands_run_as_the_agent() {
                let env = Env::new();
                let dir = env.dir("alpha");
                json_of(
                    env.agent_without_pid("sess-1", &dir)
                        .args(["leases", "--mine"]),
                );
            }
        }

        mod on_claude_pid_whose_process_is_gone {
            use super::*;

            #[test]
            fn acquire_fails_with_agent_unidentified() {
                let env = Env::new();
                seed(&env, "board", "k1");
                let dir = env.dir("alpha");
                let mut gone = std::process::Command::new("true").spawn().unwrap();
                let pid = gone.id();
                gone.wait().unwrap();
                let error = error_of(
                    env.agent_without_pid("sess-1", &dir)
                        .env(CLAUDE_PID, pid.to_string())
                        .args(["acquire", "board", "--for", "10m"]),
                    2,
                );
                assert_eq!(error["code"], "agent_unidentified", "{error}");
            }
        }

        mod on_claude_pid_set {
            use super::*;

            /// Review Focus 1: a killed Claude Code frees its board after
            /// the grace.
            #[test]
            fn the_lease_ends_after_the_grace_once_its_host_dies() {
                let env = Env::new();
                let config = env.home().join("config");
                fs::create_dir_all(&config).unwrap();
                fs::write(config.join("config.toml"), "reconnect_grace = 1\n").unwrap();
                seed(&env, "board", "k1");
                let dir = env.dir("alpha");
                let mut host = std::process::Command::new("sleep")
                    .arg("300")
                    .spawn()
                    .unwrap();
                json_of(
                    env.agent_without_pid("sess-1", &dir)
                        .env(CLAUDE_PID, host.id().to_string())
                        .args(["acquire", "board", "--for", "10m"]),
                );
                assert_eq!(env.ok(&["leases"]).as_array().map(Vec::len), Some(1));
                host.kill().unwrap();
                host.wait().unwrap();
                let until = Instant::now() + Duration::from_secs(10);
                while env.ok(&["leases"]).as_array().map(Vec::len) != Some(0) {
                    assert!(Instant::now() < until, "the lease never ended");
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }

        /// SK-H2: `/clear` while the daemon was down, so no hook said so.
        mod on_a_lost_clear {
            use super::*;

            /// A process standing for Claude Code; killed when dropped.
            struct Host(std::process::Child);

            impl Host {
                fn spawn() -> Self {
                    Self(
                        std::process::Command::new("sleep")
                            .arg("300")
                            .spawn()
                            .unwrap(),
                    )
                }

                fn pid(&self) -> String {
                    self.0.id().to_string()
                }

                fn kill(&mut self) {
                    let _ = self.0.kill();
                    let _ = self.0.wait();
                }
            }

            impl Drop for Host {
                fn drop(&mut self) {
                    self.kill();
                }
            }

            fn hosted(env: &Env, session: &str, host: &Host, dir: &Path) -> Command {
                let mut cmd = env.agent_without_pid(session, dir);
                cmd.env(CLAUDE_PID, host.pid());
                cmd
            }

            /// `old` on `host` holds `board/u1`, and `board` has a free
            /// `u2`.
            fn old_holds(env: &Env, host: &Host, dir: &Path) {
                let config = env.home().join("config");
                fs::create_dir_all(&config).unwrap();
                fs::write(config.join("config.toml"), "reconnect_grace = 3\n").unwrap();
                seed(env, "board", "k1");
                env.ok(&["unit", "add", "board", "u2", "--key", "k2"]);
                json_of(hosted(env, "old", host, dir).args(["acquire", "board", "--for", "1h"]));
            }

            fn pre_tool_use(env: &Env, session: &str, host: &Host) {
                let input = json!({
                    "session_id": session,
                    "hook_event_name": "PreToolUse",
                    "tool_name": "Bash",
                    "tool_input": {"command": "ls"},
                    "tool_use_id": "toolu_1",
                });
                env.cmd()
                    .args(["hook", "pre-tool-use"])
                    .env(CLAUDE_PID, host.pid())
                    .write_stdin(input.to_string())
                    .assert()
                    .success();
            }

            #[test]
            fn the_new_sessions_lease_ends_within_grace_after_its_process_dies() {
                let env = Env::new();
                let dir = env.dir("alpha");
                let mut host = Host::spawn();
                old_holds(&env, &host, &dir);
                env.ok(&["daemon", "stop"]);
                env.ok(&["daemon", "restart"]);
                pre_tool_use(&env, "new", &host);
                let lease = json_of(
                    hosted(&env, "new", &host, &dir).args(["acquire", "board", "--for", "1h"]),
                );
                assert_eq!(lease["outcome"], "granted", "{lease}");
                host.kill();
                let until = Instant::now() + PATIENCE;
                while env.ok(&["leases"]).as_array().map(Vec::len) != Some(0) {
                    assert!(Instant::now() < until, "a lease never ended");
                    std::thread::sleep(Duration::from_millis(200));
                }
            }

            #[test]
            fn without_any_hook_the_new_session_is_refused_session_not_hosted() {
                let env = Env::new();
                let dir = env.dir("alpha");
                let host = Host::spawn();
                old_holds(&env, &host, &dir);
                let error = error_of(
                    hosted(&env, "new", &host, &dir).args(["acquire", "board", "--for", "1h"]),
                    4,
                );
                assert_eq!(error["code"], "session_not_hosted", "{error}");
                let message = error["message"].as_str().unwrap();
                assert!(
                    message.contains("hooks are not reaching the clerk"),
                    "{error}"
                );
                assert!(message.contains("plugin installed"), "{error}");
                assert!(message.contains("daemon up"), "{error}");
                assert_eq!(env.ok(&["leases"]).as_array().map(Vec::len), Some(1));
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

        /// A git repository below the home.
        fn git_repo(env: &Env) -> PathBuf {
            let dir = env.dir("repo");
            let out = std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(&dir)
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
            dir
        }

        mod on_git_failing_inside_a_git_repo {
            use super::*;

            #[test]
            fn exits_1_with_git_failed() {
                let env = Env::new();
                let dir = git_repo(&env);
                let error = error_of(
                    env.agent("sess-1", &dir)
                        .env("PATH", env.home().join("no-bin"))
                        .args(["resource", "list"]),
                    1,
                );
                assert_eq!(error["code"], "git_failed", "{error}");
            }
        }

        mod on_git_missing_outside_any_git_repo {
            use super::*;

            #[test]
            fn uses_the_working_directory() {
                let env = Env::new();
                let dir = env.dir("alpha");
                let body = json_of(
                    env.agent("sess-1", &dir)
                        .env("PATH", env.home().join("no-bin"))
                        .args([
                            "resource", "create", "probe", "--type", "generic", "--scope",
                            "project",
                        ]),
                );
                assert_eq!(
                    body["resource"]["scope"],
                    json!({"project": canonical(&dir)})
                );
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

        mod on_another_version_with_the_same_protocol {
            use super::*;

            #[test]
            fn works() {
                let env = Env::new();
                env.cmd()
                    .env("CLERK_TEST_DAEMON_VERSION", "0.0.0-other")
                    .arg("status")
                    .assert()
                    .success();
            }
        }

        mod on_protocol_mismatch {
            use super::*;

            #[test]
            fn exits_5_saying_daemon_restart() {
                let env = Env::new();
                let error = error_of(
                    env.cmd()
                        .env("CLERK_TEST_DAEMON_PROTOCOL", "1")
                        .arg("status"),
                    5,
                );
                assert_eq!(error["code"], "protocol_mismatch");
                let message = error["message"].as_str().unwrap();
                assert!(message.contains("clerk daemon restart"), "{message}");
            }
        }
    }

    mod daemon_status {
        use super::*;

        mod on_a_socket_path_over_the_limit {
            use super::*;

            #[test]
            fn prints_the_reason_with_running_false_and_exits_0() {
                let env = Env::new();
                let long = env.home().join("a".repeat(120));
                let out = env
                    .cmd()
                    .env("CLERK_HOME", &long)
                    .args(["daemon", "status"])
                    .output()
                    .unwrap();
                assert_eq!(out.status.code(), Some(0), "{out:?}");
                let body: Value = serde_json::from_slice(&out.stdout).unwrap();
                assert_eq!(body["running"], false);
                let reason = body["reason"].as_str().unwrap();
                assert!(reason.contains("107-byte limit"), "{reason}");
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
                seed(&env, "board", "k1");
                let error = error_of(
                    env.cmd()
                        .args(["resource", "create", "board", "--type", "generic"]),
                    4,
                );
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
        fn human_wraps_descriptions_to_the_terminal_width() {
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
            assert_eq!(shown.matches('x').count(), 150, "{shown}");
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
                    "cd '{}' && {SESSION_VAR}=sess-b NO_COLOR=1 \"$CLERK\" status",
                    b.display()
                ));
                assert!(
                    shown.contains("held by agent in another project"),
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
            let shown = env.tty("NO_COLOR=1 \"$CLERK\" resources --tag esp32");
            assert!(shown.contains("tags: esp32 thread"), "{shown}");
        }

        mod on_agent_session {
            use super::*;

            #[test]
            fn omits_other_projects_resources() {
                let env = Env::new();
                let (_, b) = two_projects(&env, "a-board", "b-board");
                let body = json_of(env.agent("sess-b", &b).arg("resources"));
                assert_eq!(strings(&body, "id"), ["b-board"]);
            }
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
                    .env_remove(CLAUDE_PID)
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

        mod on_agent_session {
            use super::*;

            #[test]
            fn omits_other_projects_leases() {
                let env = Env::new();
                let (a, b) = two_projects(&env, "a-board", "b-board");
                acquire(&env, &agent_caller("sess-a", &a), "a-board");
                acquire(&env, &agent_caller("sess-b", &b), "b-board");
                let body = json_of(env.agent("sess-b", &b).arg("leases"));
                assert_eq!(strings(&body, "resource"), ["b-board"]);
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

        mod on_agent_session {
            use super::*;

            #[test]
            fn shows_only_its_own_projects_resource_of_that_id() {
                let env = Env::new();
                let (a, b) = two_projects(&env, "board", "board");
                for (session, dir) in [("sess-a", &a), ("sess-b", &b)] {
                    acquire(&env, &agent_caller(session, dir), "board");
                    acquire(&env, &agent_caller(&format!("{session}-2"), dir), "board");
                }
                let body = json_of(env.agent("sess-b", &b).args(["queue", "board"]));
                let scopes: Vec<&Value> = body
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|q| &q["scope"])
                    .collect();
                assert_eq!(scopes, [&json!({"project": canonical(&b)})]);
                assert_eq!(
                    body[0]["queue"][0]["holder"],
                    json!({"kind": "agent_session", "session_id": "sess-b-2"})
                );
            }
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
        fn since_a_span_keeps_newer_entries() {
            let env = Env::new();
            seed(&env, "board", "k1");
            assert_ne!(env.ok(&["log", "--since", "1h"]), json!([]));
        }

        mod on_since_rfc3339 {
            use super::*;

            #[test]
            fn keeps_entries_after_it() {
                let env = Env::new();
                seed(&env, "board", "k1");
                let body = env.ok(&["log", "--since", "2000-01-01T00:00:00Z"]);
                assert_ne!(body, json!([]));
            }

            #[test]
            fn drops_entries_before_it() {
                let env = Env::new();
                seed(&env, "board", "k1");
                let body = env.ok(&["log", "--since", "2999-01-01T00:00:00+02:00"]);
                assert_eq!(body, json!([]));
            }
        }

        mod on_since_bare_number {
            use super::*;

            #[test]
            fn exits_2_naming_the_accepted_forms() {
                let env = Env::new();
                let error = error_of(env.cmd().args(["log", "--since", "5"]), 2);
                assert_eq!(error["code"], "usage_error");
                let message = error["message"].as_str().unwrap();
                assert!(message.contains("<n>s|m|h|d"), "{message}");
                assert!(message.contains("RFC 3339"), "{message}");
                assert!(!env.daemon_started());
            }
        }

        mod on_since_followed_by_an_option {
            use super::*;

            #[test]
            fn exits_2_without_daemon_call() {
                let env = Env::new();
                let error = error_of(env.cmd().args(["log", "--since", "--actor", "user:x"]), 2);
                assert_eq!(error["code"], "usage_error");
                assert!(!env.daemon_started());
            }

            #[test]
            fn says_since_needs_a_value() {
                let env = Env::new();
                let error = error_of(env.cmd().args(["log", "--since", "--json"]), 2);
                let message = error["message"].as_str().unwrap();
                assert!(message.contains("--since needs a value"), "{message}");
            }
        }

        mod on_since_negative {
            use super::*;

            #[test]
            fn exits_2_naming_the_accepted_forms() {
                let env = Env::new();
                let error = error_of(env.cmd().args(["log", "--since", "-5m"]), 2);
                assert_eq!(error["code"], "usage_error");
                let message = error["message"].as_str().unwrap();
                assert!(message.contains("RFC 3339"), "{message}");
            }
        }

        mod on_agent_session_without_override {
            use super::*;

            #[test]
            fn exits_4_and_prints_nothing_from_other_projects() {
                let env = Env::new();
                promotion(&env);
                let gamma = env.dir("gamma");
                let out = env.agent("sess-c", &gamma).arg("log").output().unwrap();
                assert_eq!(out.status.code(), Some(4), "{out:?}");
                let stdout = String::from_utf8_lossy(&out.stdout);
                assert!(!stdout.contains("a-board"), "{stdout}");
                assert!(!stdout.contains("alpha"), "{stdout}");
                let body: Value = serde_json::from_slice(&out.stdout).unwrap();
                assert_eq!(body["error"]["code"], "override_required");
            }
        }

        mod on_agent_session_with_override {
            use super::*;

            #[test]
            fn lists_the_log() {
                let env = Env::new();
                promotion(&env);
                let gamma = env.dir("gamma");
                let body = json_of(env.agent("sess-c", &gamma).args([
                    "log",
                    "--action",
                    "resource_created",
                    "--override",
                    "the user asked for the log",
                ]));
                assert_eq!(body.as_array().unwrap().len(), 2, "{body}");
            }
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

        mod on_terminal {
            use super::*;

            #[test]
            fn shows_the_resource_description() {
                let env = Env::new();
                promotion(&env);
                json_of(env.cmd().current_dir(env.dir("alpha")).args([
                    "resource",
                    "update",
                    "a-board",
                    "--description",
                    "The alpha devkit",
                ]));
                let shown = env.tty("\"$CLERK\" promotions");
                assert!(shown.contains("\n  The alpha devkit\n"), "{shown}");
            }
        }

        mod on_agent_session_without_override {
            use super::*;

            #[test]
            fn exits_4_and_prints_nothing_from_other_projects() {
                let env = Env::new();
                promotion(&env);
                let gamma = env.dir("gamma");
                let out = env
                    .agent("sess-c", &gamma)
                    .arg("promotions")
                    .output()
                    .unwrap();
                assert_eq!(out.status.code(), Some(4), "{out:?}");
                let stdout = String::from_utf8_lossy(&out.stdout);
                assert!(!stdout.contains("a-board"), "{stdout}");
                assert!(!stdout.contains("beta"), "{stdout}");
                let body: Value = serde_json::from_slice(&out.stdout).unwrap();
                assert_eq!(body["error"]["code"], "override_required");
            }
        }

        mod on_agent_session_with_override {
            use super::*;

            #[test]
            fn lists_pending_promotions() {
                let env = Env::new();
                promotion(&env);
                let gamma = env.dir("gamma");
                let body = json_of(env.agent("sess-c", &gamma).args([
                    "promotions",
                    "--override",
                    "the user asked what to approve",
                ]));
                assert_eq!(body[0]["resource"], "a-board");
            }
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

    // -----------------------------------------------------------------------
    // Leasing
    // -----------------------------------------------------------------------

    /// The board from [`seed`], with a usage template naming the unit's key.
    fn seed_with_usage(env: &Env) {
        seed(env, "board", "k1");
        env.ok(&[
            "resource",
            "update",
            "board",
            "--usage",
            "Flash with key {unit.identity_key}",
        ]);
    }

    /// User `who` holds board's unit; returns the lease id.
    fn hold(env: &Env, who: &str) -> String {
        let grant = acquire(env, &user(who), "board");
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

    /// Board's first unit in the status view.
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

    /// The lease id of a grant or lease view.
    fn lease_of(body: &Value) -> String {
        body["lease_id"].as_str().unwrap().to_owned()
    }

    /// A background clerk in a process group of its own. Dropping it kills
    /// the group, so the commands `clerk run` started go too, even when an
    /// assertion fails first.
    struct Bg(std::process::Child);

    impl std::ops::Deref for Bg {
        type Target = std::process::Child;
        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl std::ops::DerefMut for Bg {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.0
        }
    }

    impl Drop for Bg {
        fn drop(&mut self) {
            if let Ok(pid) = libc::pid_t::try_from(self.0.id()) {
                // SAFETY: kill(2) on the process group this test created.
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
            }
            let _ = self.0.wait();
        }
    }

    /// clerk `args` in the background as the user (or agent `session`):
    /// stdin a pipe that stays open, stdout and stderr piped.
    fn spawn(env: &Env, args: &[&str], session: Option<&str>) -> Bg {
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_clerk"));
        cmd.process_group(0);
        cmd.args(args)
            .env("CLERK", env!("CARGO_BIN_EXE_clerk"))
            .env("CLERK_HOME", env.home())
            .env_remove(SESSION_VAR)
            .env_remove(CLAUDE_PID)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(session) = session {
            cmd.arg("--json")
                .env(SESSION_VAR, session)
                .env(CLAUDE_PID, std::process::id().to_string());
        }
        Bg(cmd.spawn().unwrap())
    }

    /// The lines `child` prints on stdout, as they come.
    fn lines(child: &mut std::process::Child) -> std::sync::mpsc::Receiver<String> {
        use std::io::BufRead;
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        rx
    }

    /// Everything `child` printed on stdout, once it exited.
    fn stdout_of(child: &mut std::process::Child) -> String {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut child.stdout.take().unwrap(), &mut text).unwrap();
        text
    }

    /// Everything `child` printed on stderr, once it exited.
    fn stderr_of(child: &mut std::process::Child) -> String {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut text).unwrap();
        text
    }

    /// True when nothing answers on `env`'s socket and its daemon lock is
    /// free.
    fn no_daemon(env: &Env) -> bool {
        let paths = env.paths();
        let lock_free = fs::File::open(&paths.lock)
            .map_or(true, |f| FileExt::try_lock_exclusive(&f).unwrap_or(false));
        lock_free && std::os::unix::net::UnixStream::connect(&paths.socket).is_err()
    }

    /// Exit code of `child`, which must end within [`PATIENCE`].
    fn code_of(child: &mut std::process::Child) -> i32 {
        let status = wait_within(child, PATIENCE).expect("clerk ended in time");
        status
            .code()
            .unwrap_or_else(|| panic!("killed by a signal: {status:?}"))
    }

    fn now_ms() -> i64 {
        use leclerk::clock::Clock;
        leclerk::clock::SystemClock.now_ms()
    }

    /// Board unit `u2` keyed `k2`, beside [`seed`]'s `u1`.
    fn add_u2(env: &Env) {
        env.ok(&["unit", "add", "board", "u2", "--key", "k2"]);
    }

    /// Agent `session` (in `dir`) acquires board/u2; returns the lease id.
    fn agent_holds_u2(env: &Env, session: &str, dir: &Path) -> String {
        lease_of(&json_of(
            env.agent(session, dir)
                .args(["acquire", "board/u2", "--for", "10m"]),
        ))
    }

    /// True when agent `session`'s pending notices name `lease`.
    fn session_has_notice_for(env: &Env, session: &str, _dir: &Path, lease: &str) -> bool {
        pending_of(env, session)
            .iter()
            .any(|n| n["lease_id"] == lease)
    }

    /// The pending notices of agent session `session`, taken now (agent
    /// sessions cannot run `clerk wait`).
    fn pending_of(env: &Env, session: &str) -> Vec<Value> {
        let holder = Caller {
            holder: HolderRef::AgentSession {
                session_id: session.to_owned(),
            },
            project: None,
            override_reason: None,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let notices = runtime.block_on(async {
            let client = Client::connect(&env.paths()).await.expect("daemon runs");
            client.wait_notices(&holder, 0).await.unwrap()
        });
        notices
            .iter()
            .map(|n| serde_json::to_value(n).unwrap())
            .collect()
    }

    /// A lent user lease: an interruptible lease `paused` that yielded to a
    /// higher-priority user lease `borrower`, both made by the user's CLI.
    /// Returns (paused, borrower).
    fn lent(env: &Env) -> (String, String) {
        seed(env, "board", "k1");
        // The CLI opens leases to interruption only for agent sessions, so
        // the user-holder lease is made directly.
        let owner = user("owner");
        let paused = lease_of(
            &env.rpc(
                "acquire",
                &owner,
                json!({"target": {"resource": "board"}, "duration_ms": 600_000,
                       "interruptible": true, "purpose": "lend it"}),
            )
            .unwrap(),
        );
        let mut borrower = spawn(
            env,
            &[
                "acquire",
                "board",
                "--for",
                "5m",
                "--priority",
                "high",
                "--wait",
            ],
            None,
        );
        let deadline = Instant::now() + PATIENCE;
        while env
            .rpc(
                "confirm",
                &owner,
                json!({"lease_id": paused, "how": "yielded"}),
            )
            .is_err()
        {
            assert!(Instant::now() < deadline, "never asked to yield");
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(code_of(&mut borrower), 0);
        let body: Value = serde_json::from_str(&stdout_of(&mut borrower)).unwrap();
        (paused, lease_of(&body))
    }

    /// The user's CLI queues a detached request for board behind user
    /// `x`'s lease; returns (x's lease, the request id).
    fn queue_detached(env: &Env) -> (String, String) {
        seed(env, "board", "k1");
        let theirs = hold(env, "x");
        let out = env
            .cmd()
            .args(["acquire", "board", "--for", "10m"])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(6), "{out:?}");
        let body: Value = serde_json::from_slice(&out.stdout).unwrap();
        (theirs, body["request_id"].as_str().unwrap().to_owned())
    }

    /// [`queue_detached`], then x gives its lease back so the request is
    /// offered the unit; returns (the request id, the offered lease id).
    fn offer_detached(env: &Env) -> (String, String) {
        let (theirs, request) = queue_detached(env);
        free(env, "x", &theirs);
        let occupancy = unit_status(env)["occupancy"].clone();
        assert_eq!(occupancy["state"], "offered", "{occupancy}");
        (request, lease_of(&occupancy))
    }

    mod acquire {
        use super::*;

        mod on_stdin_not_tty {
            use super::*;

            mod on_queued {
                use super::*;

                #[test]
                fn json_names_the_request_and_how_to_claim_or_cancel() {
                    let env = Env::new();
                    seed(&env, "board", "k1");
                    hold(&env, "x");
                    let out = env
                        .cmd()
                        .args(["acquire", "board", "--for", "10m"])
                        .output()
                        .unwrap();
                    assert_eq!(out.status.code(), Some(6), "{out:?}");
                    let body: Value = serde_json::from_slice(&out.stdout).unwrap();
                    let request = body["request_id"].as_str().unwrap();
                    let next = body["next"].as_str().unwrap();
                    assert!(next.contains("clerk attach"), "{next}");
                    assert!(next.contains(&format!("clerk cancel {request}")), "{next}");
                }

                #[test]
                fn human_text_names_the_request_and_how_to_claim_or_cancel() {
                    let env = Env::new();
                    seed(&env, "board", "k1");
                    hold(&env, "x");
                    let shown = env.tty(r#""$CLERK" acquire board --for 10m < /dev/null"#);
                    let request = env.ok(&["queue", "board"])[0]["queue"][0]["request_id"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    assert!(shown.contains(&request), "{shown}");
                    assert!(shown.contains("clerk attach"), "{shown}");
                    assert!(
                        shown.contains(&format!("clerk cancel {request}")),
                        "{shown}"
                    );
                }
            }

            #[test]
            fn prints_queued_and_exits_6_without_reading_stdin() {
                let env = Env::new();
                seed(&env, "board", "k1");
                hold(&env, "x");
                let mut child = spawn(&env, &["acquire", "board", "--for", "10m"], None);
                let _open_stdin = child.stdin.take();
                assert_eq!(code_of(&mut child), 6);
                let body: Value = serde_json::from_str(&stdout_of(&mut child)).unwrap();
                assert_eq!(body["outcome"], "queued");
                assert_eq!(body["position"], 1);
            }

            #[test]
            fn prints_granted_and_exits_0_without_reading_stdin() {
                let env = Env::new();
                seed(&env, "board", "k1");
                let mut child = spawn(&env, &["acquire", "board", "--for", "10m"], None);
                let _open_stdin = child.stdin.take();
                assert_eq!(code_of(&mut child), 0);
                let body: Value = serde_json::from_str(&stdout_of(&mut child)).unwrap();
                assert_eq!(body["outcome"], "granted");
                assert_eq!(body["unit"], "u1");
            }

            #[test]
            fn lease_held_by_a_fresh_user_holder() {
                let env = Env::new();
                seed(&env, "board", "k1");
                env.ok(&["acquire", "board", "--for", "10m"]);
                let occupancy = unit_status(&env)["occupancy"].clone();
                assert_eq!(occupancy["holder"]["kind"], "user", "{occupancy}");
            }
        }

        mod on_wait_flag {
            use super::*;

            mod on_free_unit {
                use super::*;

                #[test]
                fn lease_survives_the_process() {
                    let env = Env::new();
                    seed(&env, "board", "k1");
                    let out = env
                        .cmd()
                        // assert_cmd gives the command an empty, closed stdin.
                        .args(["--json", "acquire", "board", "--for", "10m", "--wait"])
                        .output()
                        .unwrap();
                    assert!(out.status.success(), "{out:?}");
                    let body: Value = serde_json::from_slice(&out.stdout).unwrap();
                    let lease = lease_of(&body);
                    // Past the daemon's liveness probes of CLI processes.
                    std::thread::sleep(Duration::from_secs(3));
                    let leases = env.ok(&["leases"]);
                    let listed = leases
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|l| l["occupancy"].clone())
                        .find(|o| o["lease_id"] == lease.as_str())
                        .unwrap_or_else(|| panic!("{lease} not listed: {leases}"));
                    assert_eq!(listed["holder"]["kind"], "user", "{listed}");
                    assert_eq!(unit_status(&env)["dirty"], Value::Null);
                }
            }

            mod on_queued_then_granted {
                use super::*;

                #[test]
                fn lease_survives_the_process() {
                    let env = Env::new();
                    seed(&env, "board", "k1");
                    let theirs = hold(&env, "x");
                    let mut child =
                        spawn(&env, &["acquire", "board", "--for", "10m", "--wait"], None);
                    std::thread::sleep(Duration::from_millis(500));
                    free(&env, "x", &theirs);
                    assert_eq!(code_of(&mut child), 0);
                    let body: Value = serde_json::from_str(&stdout_of(&mut child)).unwrap();
                    let lease = lease_of(&body);
                    std::thread::sleep(Duration::from_secs(3));
                    let occupancy = unit_status(&env)["occupancy"].clone();
                    assert_eq!(occupancy["lease_id"], lease.as_str(), "{occupancy}");
                    assert_eq!(occupancy["state"], "held", "{occupancy}");
                    assert_eq!(unit_status(&env)["dirty"], Value::Null);
                }
            }

            mod on_concurrent_user_wait {
                use super::*;

                #[test]
                fn still_gets_its_grant() {
                    let env = Env::new();
                    seed(&env, "board", "k1");
                    // An agent holds the unit, so the only user holder
                    // `clerk wait` could follow is the waiting request's.
                    let dir = env.dir("alpha");
                    let theirs = lease_of(&json_of(
                        env.agent("sess-1", &dir)
                            .args(["acquire", "board", "--for", "10m"]),
                    ));
                    let mut child =
                        spawn(&env, &["acquire", "board", "--for", "10m", "--wait"], None);
                    let deadline = Instant::now() + PATIENCE;
                    while env.ok(&["queue", "board"])[0]["queue"] == json!([]) {
                        assert!(Instant::now() < deadline, "never queued");
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    let _waiter = spawn(&env, &["wait"], None);
                    std::thread::sleep(Duration::from_millis(1500));
                    json_of(env.agent("sess-1", &dir).args(["release", &theirs]));
                    assert_eq!(code_of(&mut child), 0);
                    let body: Value = serde_json::from_str(&stdout_of(&mut child)).unwrap();
                    assert_eq!(body["state"], "held", "{body}");
                }
            }

            mod on_grant {
                use super::*;

                #[test]
                fn lease_moves_to_a_user_holder() {
                    let env = Env::new();
                    seed(&env, "board", "k1");
                    let theirs = hold(&env, "x");
                    let mut child =
                        spawn(&env, &["acquire", "board", "--for", "10m", "--wait"], None);
                    std::thread::sleep(Duration::from_millis(500));
                    free(&env, "x", &theirs);
                    assert_eq!(code_of(&mut child), 0);
                    let occupancy = unit_status(&env)["occupancy"].clone();
                    assert_eq!(occupancy["holder"]["kind"], "user", "{occupancy}");
                    assert_eq!(occupancy["state"], "held", "{occupancy}");
                }
            }

            mod while_waiting {
                use super::*;

                #[test]
                fn request_is_held_by_this_cli_process() {
                    let env = Env::new();
                    seed(&env, "board", "k1");
                    hold(&env, "x");
                    let child = spawn(&env, &["acquire", "board", "--for", "10m", "--wait"], None);
                    let deadline = Instant::now() + PATIENCE;
                    let queue = loop {
                        let queue = env.ok(&["queue", "board"])[0]["queue"].clone();
                        if queue != json!([]) {
                            break queue;
                        }
                        assert!(Instant::now() < deadline, "never queued");
                        std::thread::sleep(Duration::from_millis(50));
                    };
                    assert_eq!(queue[0]["holder"]["kind"], "cli_process", "{queue}");
                    assert_eq!(queue[0]["holder"]["pid"], child.id(), "{queue}");
                }
            }

            mod on_daemon_stopped_mid_wait {
                use super::*;

                #[test]
                fn exits_5_without_a_cancel_call_or_a_daemon() {
                    let env = Env::new();
                    seed(&env, "board", "k1");
                    hold(&env, "x");
                    let mut child =
                        spawn(&env, &["acquire", "board", "--for", "10m", "--wait"], None);
                    let deadline = Instant::now() + PATIENCE;
                    while env.ok(&["queue", "board"])[0]["queue"] == json!([]) {
                        assert!(Instant::now() < deadline, "never queued");
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    env.cmd().args(["daemon", "stop"]).assert().success();
                    assert_eq!(code_of(&mut child), 5);
                    let said = stderr_of(&mut child);
                    assert!(!said.contains("cancel"), "{said}");
                    assert!(no_daemon(&env), "a daemon runs after the stop");
                }
            }

            #[test]
            fn blocks_until_granted_then_prints_and_exits_0() {
                let env = Env::new();
                seed(&env, "board", "k1");
                let theirs = hold(&env, "x");
                let mut child = spawn(&env, &["acquire", "board", "--for", "10m", "--wait"], None);
                let _open_stdin = child.stdin.take();
                std::thread::sleep(Duration::from_millis(500));
                assert!(child.try_wait().unwrap().is_none(), "returned while queued");
                free(&env, "x", &theirs);
                assert_eq!(code_of(&mut child), 0);
                let body: Value = serde_json::from_str(&stdout_of(&mut child)).unwrap();
                assert_eq!(body["state"], "held", "{body}");
                assert_ne!(lease_of(&body), theirs);
            }
        }

        mod on_interruptible_without_session {
            use super::*;

            #[test]
            fn exits_2_without_daemon_call() {
                let env = Env::new();
                let error = error_of(
                    env.cmd()
                        .args(["acquire", "board", "--for", "10m", "--interruptible"]),
                    2,
                );
                assert_eq!(error["code"], "usage_error");
                let message = error["message"].as_str().unwrap();
                assert!(message.contains("--interruptible"), "{message}");
                assert!(!env.daemon_started());
            }

            #[test]
            fn run_exits_2_without_daemon_call() {
                let env = Env::new();
                let error = error_of(
                    env.cmd().args([
                        "run",
                        "board",
                        "--for",
                        "10m",
                        "--interruptible",
                        "--",
                        "true",
                    ]),
                    2,
                );
                assert_eq!(error["code"], "usage_error");
                assert!(!env.daemon_started());
            }
        }

        mod on_interruptible_in_agent_session {
            use super::*;

            #[test]
            fn opens_the_lease_to_interruption() {
                let env = Env::new();
                seed(&env, "board", "k1");
                let dir = env.dir("alpha");
                let grant = json_of(env.agent("sess-1", &dir).args([
                    "acquire",
                    "board",
                    "--for",
                    "10m",
                    "--interruptible",
                ]));
                let lease = json_of(
                    env.agent("sess-1", &dir)
                        .args(["attach", &lease_of(&grant)]),
                );
                assert_eq!(lease["interruptible"], true, "{lease}");
            }
        }

        mod on_invalid_for {
            use super::*;

            #[test]
            fn exits_2_for_each_of_0_neg_abc_100000d() {
                let env = Env::new();
                for bad in ["0", "-5m", "abc", "100000d"] {
                    let error = error_of(env.cmd().args(["acquire", "board", "--for", bad]), 2);
                    assert_eq!(error["code"], "usage_error", "{bad}");
                    let message = error["message"].as_str().unwrap();
                    assert!(message.contains("<n>s|m|h|d"), "{bad}: {message}");
                }
                assert!(!env.daemon_started());
            }
        }

        mod on_no_units {
            use super::*;

            #[test]
            fn exits_4_code_no_units() {
                let env = Env::new();
                env.ok(&["resource", "create", "empty", "--type", "generic"]);
                let error = error_of(env.cmd().args(["acquire", "empty", "--for", "10m"]), 4);
                assert_eq!(error["code"], "no_units");
                assert!(
                    error["message"].as_str().unwrap().contains("add"),
                    "{error}"
                );
            }
        }

        mod on_grant {
            use super::*;

            #[test]
            fn prints_brief() {
                let env = Env::new();
                seed_with_usage(&env);
                let body = env.ok(&["acquire", "board", "--for", "10m"]);
                assert_eq!(body["resource_description"], "A test board");
                assert_eq!(body["unit_description"], "The left one");
                assert_eq!(body["resource_tags"], json!(["esp32", "thread"]));
                assert_eq!(body["usage"], "Flash with key k1");
            }
        }

        mod on_session_env {
            use super::*;

            #[test]
            fn lease_owned_by_agent_session() {
                let env = Env::new();
                seed(&env, "board", "k1");
                let dir = env.dir("alpha");
                json_of(
                    env.agent("sess-1", &dir)
                        .args(["acquire", "board", "--for", "10m"]),
                );
                assert_eq!(
                    unit_status(&env)["occupancy"]["holder"],
                    json!({"kind": "agent_session", "session_id": "sess-1"})
                );
            }
        }
    }

    mod acquire_override {
        use super::*;

        mod on_empty_reason {
            use super::*;

            #[test]
            fn refused_exit_2() {
                let env = Env::new();
                let dir = env.dir("alpha");
                let error = error_of(
                    env.agent("sess-1", &dir).args([
                        "acquire",
                        "board",
                        "--for",
                        "10m",
                        "--override",
                        "",
                    ]),
                    2,
                );
                assert_eq!(error["code"], "usage_error");
                assert!(!env.daemon_started());
            }
        }
    }

    mod attach {
        use super::*;

        mod on_lent_lease {
            use super::*;

            #[test]
            fn prints_its_brief() {
                let env = Env::new();
                let (paused, _) = lent(&env);
                let body = env.ok(&["attach", &paused]);
                assert_eq!(lease_of(&body), paused);
                assert_eq!(body["state"], "paused");
            }
        }

        #[test]
        fn prints_the_brief_from_get_lease() {
            let env = Env::new();
            seed_with_usage(&env);
            let lease = lease_of(&env.ok(&["acquire", "board", "--for", "10m"]));
            let body = env.ok(&["attach", &lease]);
            assert_eq!(lease_of(&body), lease);
            assert_eq!(body["resource_description"], "A test board");
            assert_eq!(body["unit_description"], "The left one");
            assert_eq!(body["resource_tags"], json!(["esp32", "thread"]));
            assert_eq!(body["usage"], "Flash with key k1");
        }

        mod on_offered_lease_without_tty {
            use super::*;

            #[test]
            fn claims_it_and_prints_the_grant() {
                let env = Env::new();
                let (_, lease) = offer_detached(&env);
                let body = env.ok(&["attach", &lease]);
                assert_eq!(lease_of(&body), lease);
                assert_eq!(body["state"], "held", "{body}");
                assert_eq!(body["resource_description"], "A test board");
            }

            #[test]
            fn lease_is_held_afterwards() {
                let env = Env::new();
                let (_, lease) = offer_detached(&env);
                env.ok(&["attach", &lease]);
                let occupancy = unit_status(&env)["occupancy"].clone();
                assert_eq!(lease_of(&occupancy), lease);
                assert_eq!(occupancy["state"], "held", "{occupancy}");
            }
        }

        mod on_ended_lease {
            use super::*;

            #[test]
            fn exits_3() {
                let env = Env::new();
                seed(&env, "board", "k1");
                let lease = lease_of(&env.ok(&["acquire", "board", "--for", "10m"]));
                env.ok(&["release", &lease]);
                let error = error_of(env.cmd().args(["attach", &lease]), 3);
                assert_eq!(error["code"], "not_found");
            }
        }

        mod on_unknown_lease {
            use super::*;

            #[test]
            fn exits_3() {
                let env = Env::new();
                seed(&env, "board", "k1");
                let ghost = leclerk::model::LeaseId::new();
                let error = error_of(env.cmd().args(["attach", ghost.as_str()]), 3);
                assert_eq!(error["code"], "not_found");
            }
        }

        mod on_someone_elses_lease {
            use super::*;

            #[test]
            fn exits_3_and_nothing_changes() {
                let env = Env::new();
                seed(&env, "board", "k1");
                let dir = env.dir("alpha");
                let lease = lease_of(&json_of(
                    env.agent("sess-1", &dir)
                        .args(["acquire", "board", "--for", "10m"]),
                ));
                let before = unit_status(&env);
                let error = error_of(env.cmd().args(["attach", &lease]), 3);
                assert_eq!(error["code"], "not_found");
                assert_eq!(unit_status(&env), before);
            }
        }
    }

    mod cancel {
        use super::*;

        mod on_queued_request {
            use super::*;

            #[test]
            fn withdraws_it_and_exits_0() {
                let env = Env::new();
                let (_, request) = queue_detached(&env);
                let body = env.ok(&["cancel", &request]);
                assert_eq!(body["cancelled"], request.as_str());
                assert_eq!(env.ok(&["queue", "board"])[0]["queue"], json!([]));
            }
        }

        mod on_pending_offer {
            use super::*;

            #[test]
            fn withdraws_the_offer_unclaimed() {
                let env = Env::new();
                let (request, lease) = offer_detached(&env);
                env.ok(&["cancel", &request]);
                let ended = &audit(&env, "lease_ended")[0]["detail"];
                assert_eq!(ended["lease"], lease.as_str());
                assert_eq!(ended["cause"], "unclaimed", "{ended}");
            }

            #[test]
            fn leaves_the_unit_free() {
                let env = Env::new();
                let (request, _) = offer_detached(&env);
                env.ok(&["cancel", &request]);
                assert_eq!(unit_status(&env)["occupancy"]["kind"], "free");
            }
        }

        mod on_unknown_request {
            use super::*;

            #[test]
            fn exits_3() {
                let env = Env::new();
                seed(&env, "board", "k1");
                let ghost = leclerk::model::RequestId::new();
                let error = error_of(env.cmd().args(["cancel", ghost.as_str()]), 3);
                assert_eq!(error["code"], "not_found");
            }
        }

        mod on_agent_sessions_request {
            use super::*;

            #[test]
            fn exits_3_and_nothing_changes() {
                let env = Env::new();
                seed(&env, "board", "k1");
                hold(&env, "x");
                let dir = env.dir("alpha");
                let out = env
                    .agent("sess-1", &dir)
                    .args(["acquire", "board", "--for", "10m"])
                    .output()
                    .unwrap();
                assert_eq!(out.status.code(), Some(6), "{out:?}");
                let body: Value = serde_json::from_slice(&out.stdout).unwrap();
                let request = body["request_id"].as_str().unwrap();
                let error = error_of(env.cmd().args(["cancel", request]), 3);
                assert_eq!(error["code"], "not_found");
                assert_eq!(
                    env.ok(&["queue", "board"])[0]["queue"][0]["request_id"],
                    request
                );
            }
        }

        mod on_agent_session {
            use super::*;

            #[test]
            fn withdraws_its_own_request() {
                let env = Env::new();
                seed(&env, "board", "k1");
                hold(&env, "x");
                let dir = env.dir("alpha");
                let out = env
                    .agent("sess-1", &dir)
                    .args(["acquire", "board", "--for", "10m"])
                    .output()
                    .unwrap();
                let body: Value = serde_json::from_slice(&out.stdout).unwrap();
                let request = body["request_id"].as_str().unwrap();
                json_of(env.agent("sess-1", &dir).args(["cancel", request]));
                assert_eq!(env.ok(&["queue", "board"])[0]["queue"], json!([]));
            }
        }
    }

    mod release {
        use super::*;

        mod on_lent_lease {
            use super::*;

            #[test]
            fn ends_it() {
                let env = Env::new();
                let (paused, _) = lent(&env);
                let body = env.ok(&["release", &paused]);
                assert_eq!(body["state"], "ended", "{body}");
            }
        }

        #[test]
        fn starts_cleanup_and_confirms_clean() {
            let env = Env::new();
            seed(&env, "board", "k1");
            let lease = lease_of(&env.ok(&["acquire", "board", "--for", "10m"]));
            let body = env.ok(&["release", &lease]);
            assert_eq!(body["state"], "ended", "{body}");
            assert_eq!(body["end_cause"], "released");
            let ended = &audit(&env, "lease_ended")[0]["detail"];
            assert_eq!(ended["lease"], lease.as_str());
            assert_eq!(ended["cleanup_confirmed"], true);
            assert_eq!(unit_status(&env)["dirty"], Value::Null);
        }
    }

    mod extend {
        use super::*;

        mod on_borrowers_lease {
            use super::*;

            #[test]
            fn moves_its_end() {
                let env = Env::new();
                let (_, borrower) = lent(&env);
                let before = now_ms();
                let body = env.ok(&["extend", &borrower, "--for", "1h"]);
                assert!(
                    body["ends_at"].as_i64().unwrap() >= before + 3_600_000,
                    "{body}"
                );
            }
        }

        #[test]
        fn new_end_is_now_plus_duration() {
            let env = Env::new();
            seed(&env, "board", "k1");
            let lease = lease_of(&env.ok(&["acquire", "board", "--for", "10m"]));
            let before = now_ms();
            let body = env.ok(&["extend", &lease, "--for", "1h"]);
            let after = now_ms();
            let ends = body["ends_at"].as_i64().unwrap();
            assert!(
                (before + 3_600_000..=after + 3_600_000).contains(&ends),
                "{before} {ends} {after}"
            );
        }

        mod on_higher_priority_waiter {
            use super::*;

            #[test]
            fn exits_4_showing_the_waiting_purpose() {
                let env = Env::new();
                seed(&env, "board", "k1");
                let lease = lease_of(&env.ok(&["acquire", "board", "--for", "10m"]));
                env.rpc(
                    "acquire",
                    &user("w"),
                    json!({
                        "target": {"resource": "board"},
                        "duration_ms": 60_000,
                        "priority": "high",
                        "purpose": "urgent flash",
                    }),
                )
                .unwrap();
                let error = error_of(env.cmd().args(["extend", &lease, "--for", "1h"]), 4);
                assert_eq!(error["code"], "extend_refused");
                assert_eq!(error["details"]["waiting_purpose"], "urgent flash");
            }
        }
    }

    mod run {
        use super::*;

        mod on_agent_session_with_another_lease {
            use super::*;

            #[test]
            fn leaves_that_leases_notices_for_the_session() {
                let env = Env::new();
                seed(&env, "board", "k1");
                add_u2(&env);
                let dir = env.dir("alpha");
                let other = agent_holds_u2(&env, "sess-1", &dir);
                env.agent("sess-1", &dir)
                    .env("CLERK", env!("CARGO_BIN_EXE_clerk"))
                    .env("OTHER", &other)
                    .args([
                        "run",
                        "board/u1",
                        "--for",
                        "10m",
                        "--",
                        "sh",
                        "-c",
                        "\"$CLERK\" release \"$OTHER\" >/dev/null && sleep 1",
                    ])
                    .assert()
                    .code(0);
                assert!(session_has_notice_for(&env, "sess-1", &dir, &other));
            }
        }

        mod on_cmd {
            use super::*;

            #[test]
            fn receives_clerk_unit_env() {
                let env = Env::new();
                seed(&env, "board", "k1");
                let out = env
                    .cmd()
                    .args([
                        "run",
                        "board",
                        "--for",
                        "10m",
                        "--",
                        "sh",
                        "-c",
                        "echo \"$CLERK_LEASE_ID|$CLERK_RESOURCE|$CLERK_UNIT|$CLERK_UNIT_IDENTITY_KEY\"",
                    ])
                    .output()
                    .unwrap();
                assert!(out.status.success(), "{out:?}");
                let text = String::from_utf8(out.stdout).unwrap();
                let fields: Vec<&str> = text.trim().split('|').collect();
                assert_eq!(fields.len(), 4, "{text}");
                assert!(!fields[0].is_empty(), "{text}");
                assert_eq!(&fields[1..], ["board", "u1", "k1"]);
            }

            #[test]
            fn lease_held_by_this_cli_process() {
                let env = Env::new();
                seed(&env, "board", "k1");
                let out = env
                    .cmd()
                    .env("CLERK", env!("CARGO_BIN_EXE_clerk"))
                    .args([
                        "run",
                        "board",
                        "--for",
                        "10m",
                        "--",
                        "sh",
                        "-c",
                        "\"$CLERK\" status --json",
                    ])
                    .output()
                    .unwrap();
                assert!(out.status.success(), "{out:?}");
                let status: Value = serde_json::from_slice(&out.stdout).unwrap();
                let holder = &status["resources"][0]["units"][0]["occupancy"]["holder"];
                assert_eq!(holder["kind"], "cli_process", "{status}");
            }
        }

        mod on_queued {
            use super::*;

            #[test]
            fn waits_and_claims_then_runs_cmd() {
                let env = Env::new();
                seed(&env, "board", "k1");
                let theirs = hold(&env, "x");
                let mut child = spawn(
                    &env,
                    &["run", "board", "--for", "10m", "--", "echo", "ran"],
                    None,
                );
                std::thread::sleep(Duration::from_millis(500));
                assert!(child.try_wait().unwrap().is_none(), "ran while queued");
                free(&env, "x", &theirs);
                assert_eq!(code_of(&mut child), 0);
                assert_eq!(stdout_of(&mut child).trim(), "ran");
            }
        }

        mod on_cmd_exit_0 {
            use super::*;

            #[test]
            fn releases_with_confirm_clean_and_exits_0() {
                let env = Env::new();
                seed(&env, "board", "k1");
                env.cmd()
                    .args(["run", "board", "--for", "10m", "--", "true"])
                    .assert()
                    .code(0);
                let ended = &audit(&env, "lease_ended")[0]["detail"];
                assert_eq!(ended["cause"], "released");
                assert_eq!(ended["cleanup_confirmed"], true);
                assert_eq!(unit_status(&env)["dirty"], Value::Null);
            }
        }

        mod on_cmd_exit_3 {
            use super::*;

            #[test]
            fn unit_dirty_command_failed_and_exit_3() {
                let env = Env::new();
                seed(&env, "board", "k1");
                env.cmd()
                    .args(["run", "board", "--for", "10m", "--", "sh", "-c", "exit 3"])
                    .assert()
                    .code(3);
                let unit = unit_status(&env);
                assert_eq!(unit["dirty"]["reason"], "command_failed", "{unit}");
                assert_eq!(unit["occupancy"]["kind"], "free", "{unit}");
            }
        }

        /// A command that says "started", then on SIGTERM says
        /// "terminated" and exits 7.
        const PATIENT: &str =
            "trap 'echo terminated; exit 7' TERM; echo started; while :; do sleep 0.1; done";

        mod on_override {
            use super::*;

            #[test]
            fn sigterms_cmd_at_the_cleanup_deadline() {
                let env = Env::new();
                let config = env.paths().config;
                fs::create_dir_all(config.parent().unwrap()).unwrap();
                fs::write(&config, "cleanup_window = 2\n").unwrap();
                seed(&env, "board", "k1");
                let mut child = spawn(
                    &env,
                    &["run", "board", "--for", "10m", "--", "sh", "-c", PATIENT],
                    None,
                );
                let out = lines(&mut child);
                assert_eq!(out.recv_timeout(PATIENCE).unwrap(), "started");
                let dir = env.dir("alpha");
                let mut agent = agent_caller("sess-1", &dir);
                agent.override_reason = Some("the user asked".to_owned());
                let overridden = Instant::now();
                env.rpc(
                    "acquire",
                    &agent,
                    json!({
                        "target": {"resource": "board"},
                        "duration_ms": 60_000,
                        "purpose": "take it",
                        "override_reason": "the user asked",
                    }),
                )
                .unwrap();
                assert_eq!(out.recv_timeout(PATIENCE).unwrap(), "terminated");
                assert!(
                    overridden.elapsed() >= Duration::from_millis(1500),
                    "{:?}",
                    overridden.elapsed()
                );
                assert_eq!(code_of(&mut child), 7);
            }
        }

        mod on_admin_release {
            use super::*;

            #[test]
            fn sigterms_cmd() {
                let env = Env::new();
                seed(&env, "board", "k1");
                let mut child = spawn(
                    &env,
                    &["run", "board", "--for", "10m", "--", "sh", "-c", PATIENT],
                    None,
                );
                let out = lines(&mut child);
                assert_eq!(out.recv_timeout(PATIENCE).unwrap(), "started");
                let lease = unit_status(&env)["occupancy"]["lease_id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                env.rpc(
                    "admin_release",
                    &user("admin"),
                    json!({ "lease_id": lease }),
                )
                .unwrap();
                assert_eq!(out.recv_timeout(PATIENCE).unwrap(), "terminated");
                assert_eq!(code_of(&mut child), 7);
            }
        }

        mod on_sigint {
            use super::*;

            #[test]
            fn forwards_it_to_cmd() {
                let env = Env::new();
                seed(&env, "board", "k1");
                let mut child = spawn(
                    &env,
                    &[
                        "run",
                        "board",
                        "--for",
                        "10m",
                        "--",
                        "sh",
                        "-c",
                        "trap 'echo interrupted; exit 9' INT; echo started; while :; do sleep 0.1; done",
                    ],
                    None,
                );
                let out = lines(&mut child);
                assert_eq!(out.recv_timeout(PATIENCE).unwrap(), "started");
                let pid = libc::pid_t::try_from(child.id()).unwrap();
                // SAFETY: plain kill(2) on our own child.
                assert_eq!(unsafe { libc::kill(pid, libc::SIGINT) }, 0);
                assert_eq!(out.recv_timeout(PATIENCE).unwrap(), "interrupted");
                assert_eq!(code_of(&mut child), 9);
                let mut stderr = String::new();
                std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr)
                    .unwrap();
                assert!(stderr.contains("Passing SIGINT on"), "{stderr}");
            }
        }
    }

    mod wait {
        use super::*;

        #[test]
        fn prints_notices_as_they_arrive() {
            let env = Env::new();
            seed(&env, "board", "k1");
            let theirs = hold(&env, "x");
            env.cmd()
                .args(["acquire", "board", "--for", "10m"])
                .assert()
                .code(6);
            let mut child = spawn(&env, &["wait"], None);
            let out = lines(&mut child);
            free(&env, "x", &theirs);
            let line = out.recv_timeout(PATIENCE).unwrap();
            let _ = child.kill();
            let _ = child.wait();
            let notice: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(notice["kind"], "offered", "{notice}");
        }

        mod on_once {
            use super::*;

            #[test]
            fn returns_after_the_first_batch() {
                let env = Env::new();
                seed(&env, "board", "k1");
                let theirs = hold(&env, "x");
                env.cmd()
                    .args(["acquire", "board", "--for", "10m"])
                    .assert()
                    .code(6);
                free(&env, "x", &theirs);
                let body = env.ok(&["wait", "--once"]);
                let kinds = strings(&body, "kind");
                assert!(kinds.contains(&"offered".to_owned()), "{body}");
            }
        }

        /// `sess-1` in `dir` queued for `board` behind user `x`; returns
        /// x's lease.
        fn agent_queued(env: &Env, dir: &Path) -> String {
            seed(env, "board", "k1");
            let theirs = hold(env, "x");
            env.agent("sess-1", dir)
                .args(["acquire", "board", "--for", "10m"])
                .assert()
                .code(6);
            theirs
        }

        mod on_an_agent_session {
            use super::*;

            mod on_pending_notices {
                use super::*;

                #[test]
                fn prints_one_batch_with_summary_and_next_and_exits_0() {
                    let env = Env::new();
                    let dir = env.dir("alpha");
                    let theirs = agent_queued(&env, &dir);
                    free(&env, "x", &theirs);
                    let body = json_of(env.agent("sess-1", &dir).args(["wait"]));
                    let kinds = strings(&body["notices"], "kind");
                    assert!(kinds.contains(&"offered".to_owned()), "{body}");
                    assert!(body["summary"].is_array(), "{body}");
                    assert!(
                        body["next"].as_str().unwrap().contains("clerk wait"),
                        "{body}"
                    );
                }

                #[test]
                fn claims_an_offered_lease_and_carries_the_grant() {
                    let env = Env::new();
                    let dir = env.dir("alpha");
                    let theirs = agent_queued(&env, &dir);
                    free(&env, "x", &theirs);
                    let body = json_of(env.agent("sess-1", &dir).args(["wait"]));
                    let claimed = body["claimed"].as_array().unwrap();
                    assert_eq!(claimed.len(), 1, "{body}");
                    assert_eq!(claimed[0]["state"], "held", "{body}");
                    let mine = json_of(env.agent("sess-1", &dir).args(["leases", "--mine"]));
                    assert_eq!(mine["leases"][0]["state"], "held", "{mine}");
                }

                /// Review finding 3: an offer this session already holds
                /// (claimed by hand, or by a wait that died before acking)
                /// is dropped as superseded, not a failed claim.
                #[test]
                fn an_offer_already_claimed_is_dropped_not_a_failed_claim() {
                    let env = Env::new();
                    let dir = env.dir("alpha");
                    let theirs = agent_queued(&env, &dir);
                    free(&env, "x", &theirs);
                    let lease = lease_of(&unit_status(&env)["occupancy"]);
                    json_of(env.agent("sess-1", &dir).args(["claim", &lease]));
                    let body = json_of(env.agent("sess-1", &dir).args(["wait", "--timeout", "1s"]));
                    // The offer is superseded by the claim: nothing to report,
                    // and no failed claim.
                    assert_eq!(body["notices"], json!([]), "{body}");
                    assert!(
                        !body["summary"].to_string().contains("Could not claim"),
                        "{body}"
                    );
                }

                #[test]
                fn marks_them_delivered_once_printed() {
                    let env = Env::new();
                    let dir = env.dir("alpha");
                    let theirs = agent_queued(&env, &dir);
                    free(&env, "x", &theirs);
                    json_of(env.agent("sess-1", &dir).args(["wait"]));
                    let again =
                        json_of(env.agent("sess-1", &dir).args(["wait", "--timeout", "1s"]));
                    assert_eq!(again["notices"], json!([]), "{again}");
                }
            }

            mod on_timeout {
                use super::*;

                #[test]
                fn exits_0_with_an_empty_batch_saying_to_start_it_again() {
                    let env = Env::new();
                    let dir = env.dir("alpha");
                    let body = json_of(env.agent("sess-1", &dir).args(["wait", "--timeout", "1s"]));
                    assert_eq!(body["notices"], json!([]), "{body}");
                    assert!(
                        body["next"].as_str().unwrap().contains("clerk wait"),
                        "{body}"
                    );
                }

                /// Peer report 3: a wait killed outright (no chance to
                /// disarm) holds the arm a few seconds; the next wait waits
                /// that out instead of exiting "already armed".
                #[test]
                fn a_wait_started_right_after_a_killed_one_arms() {
                    let env = Env::new();
                    let dir = env.dir("alpha");
                    let mut child = spawn(&env, &["wait"], Some("sess-1"));
                    std::thread::sleep(Duration::from_millis(800));
                    let pid = i32::try_from(child.0.id()).unwrap();
                    // SAFETY: plain kill(2) on our own child.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                    let _ = child.0.wait();
                    let body = json_of(env.agent("sess-1", &dir).args(["wait", "--timeout", "8s"]));
                    assert_ne!(body["armed"], true, "{body}");
                }

                /// Peer report 2: a `--follow` wait that ends says why.
                #[test]
                fn a_follow_wait_that_ends_says_why() {
                    let env = Env::new();
                    let dir = env.dir("alpha");
                    let out = env
                        .agent_md("sess-1", &dir)
                        .args(["wait", "--follow", "--timeout", "1s"])
                        .output()
                        .unwrap();
                    let text = String::from_utf8(out.stdout).unwrap();
                    assert!(text.contains("Next: "), "{text:?}");
                }

                /// Research finding 1: re-arming right after a Monitor
                /// expiry (SIGTERM) arms, instead of "already armed".
                #[test]
                fn a_wait_started_right_after_arms() {
                    let env = Env::new();
                    let dir = env.dir("alpha");
                    let child = spawn(&env, &["wait"], Some("sess-1"));
                    std::thread::sleep(Duration::from_millis(800));
                    let pid = i32::try_from(child.0.id()).unwrap();
                    // SAFETY: plain kill(2) on our own child.
                    unsafe { libc::kill(pid, libc::SIGTERM) };
                    let mut child = child;
                    assert!(child.0.wait().unwrap().success());
                    let body = json_of(env.agent("sess-1", &dir).args(["wait", "--timeout", "1s"]));
                    assert_ne!(body["armed"], true, "{body}");
                }
            }

            mod on_another_wait_already_armed {
                use super::*;

                #[test]
                fn exits_0_at_once_saying_one_is_armed_and_takes_nothing() {
                    let env = Env::new();
                    let dir = env.dir("alpha");
                    let theirs = agent_queued(&env, &dir);
                    let mut first = spawn(&env, &["wait"], Some("sess-1"));
                    std::thread::sleep(Duration::from_millis(800));
                    let started = Instant::now();
                    let body = json_of(env.agent("sess-1", &dir).args(["wait"]));
                    // It waits out the other's arm (ARM_GRACE, 6 s) before giving way.
                    assert!(started.elapsed() < Duration::from_secs(12));
                    assert_eq!(body["armed"], true, "{body}");
                    assert!(
                        body["summary"][0]
                            .as_str()
                            .unwrap()
                            .contains("already armed"),
                        "{body}"
                    );
                    free(&env, "x", &theirs);
                    assert_eq!(code_of(&mut first), 0);
                }
            }

            mod on_the_session_moved_on {
                use super::*;

                #[test]
                fn exits_0_with_an_empty_batch() {
                    // `/clear`: the same process now runs another session.
                    let env = Env::new();
                    let dir = env.dir("alpha");
                    agent_queued(&env, &dir);
                    let mut old = spawn(&env, &["wait"], Some("sess-1"));
                    std::thread::sleep(Duration::from_millis(800));
                    // The SessionStart hook of the new session moves the process.
                    let input = json!({"session_id": "sess-2", "hook_event_name": "SessionStart",
                                       "source": "clear"});
                    env.cmd()
                        .args(["hook", "session-start"])
                        .env(CLAUDE_PID, std::process::id().to_string())
                        .write_stdin(input.to_string())
                        .assert()
                        .success();
                    assert_eq!(code_of(&mut old), 0);
                }
            }

            mod on_the_host_process_exiting {
                use super::*;

                #[test]
                fn exits_0_within_a_few_seconds() {
                    let env = Env::new();
                    let dir = env.dir("alpha");
                    let mut host = std::process::Command::new("sleep")
                        .arg("300")
                        .spawn()
                        .unwrap();
                    let mut wait = std::process::Command::new(env!("CARGO_BIN_EXE_clerk"))
                        .arg("wait")
                        .env("CLERK_HOME", env.home())
                        .env(SESSION_VAR, "sess-1")
                        .env(CLAUDE_PID, host.id().to_string())
                        .current_dir(&dir)
                        .stdin(Stdio::null())
                        .stdout(Stdio::piped())
                        .spawn()
                        .unwrap();
                    std::thread::sleep(Duration::from_millis(800));
                    host.kill().unwrap();
                    host.wait().unwrap();
                    let killed = Instant::now();
                    loop {
                        if let Some(status) = wait.try_wait().unwrap() {
                            assert!(status.success());
                            break;
                        }
                        assert!(killed.elapsed() < Duration::from_secs(5), "wait still runs");
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            }

            mod on_sigterm {
                use super::*;

                #[test]
                fn prints_an_empty_batch_saying_to_start_it_again_and_exits_0() {
                    let env = Env::new();
                    let mut child = spawn(&env, &["wait"], Some("sess-1"));
                    std::thread::sleep(Duration::from_millis(800));
                    let pid = i32::try_from(child.0.id()).unwrap();
                    // SAFETY: plain kill(2) on our own child.
                    unsafe { libc::kill(pid, libc::SIGTERM) };
                    let mut stdout = Vec::new();
                    std::io::Read::read_to_end(&mut child.0.stdout.take().unwrap(), &mut stdout)
                        .unwrap();
                    assert!(child.0.wait().unwrap().success());
                    let body: Value = serde_json::from_slice(&stdout).unwrap();
                    assert_eq!(body["notices"], json!([]), "{body}");
                    assert!(
                        body["next"].as_str().unwrap().contains("clerk wait"),
                        "{body}"
                    );
                }
            }

            mod on_follow {
                use super::*;

                #[test]
                fn prints_one_line_per_notice_and_keeps_running() {
                    let env = Env::new();
                    let dir = env.dir("alpha");
                    let theirs = agent_queued(&env, &dir);
                    let mut child = spawn(&env, &["wait", "--follow"], Some("sess-1"));
                    let out = lines(&mut child.0);
                    free(&env, "x", &theirs);
                    let first: Value =
                        serde_json::from_str(&out.recv_timeout(PATIENCE).unwrap()).unwrap();
                    assert_eq!(first["notice"]["kind"], "offered", "{first}");
                    assert!(first["claimed"]["lease_id"].is_string(), "{first}");
                    assert!(first["next"].is_string(), "{first}");
                    let lease = first["claimed"]["lease_id"].as_str().unwrap().to_owned();
                    env.rpc(
                        "admin_release",
                        &user("admin"),
                        json!({ "lease_id": lease }),
                    )
                    .unwrap();
                    let second: Value =
                        serde_json::from_str(&out.recv_timeout(PATIENCE).unwrap()).unwrap();
                    assert_eq!(second["notice"]["kind"], "lease_ended", "{second}");
                    assert!(child.0.try_wait().unwrap().is_none(), "still running");
                }
            }
        }

        /// Research finding 2: Monitor cuts events at ~500 characters, so a
        /// `--follow` event stays short even for a resource with long usage.
        mod on_follow_with_long_usage {
            use super::*;

            fn first_event(env: &Env, dir: &Path, json: bool) -> String {
                env.ok(&[
                    "resource",
                    "create",
                    "board",
                    "--type",
                    "generic",
                    "--usage",
                    &"Flash it with the vendor tool; hold BOOT while plugging in. ".repeat(20),
                ]);
                env.ok(&["unit", "add", "board", "u1", "--key", "k1"]);
                let theirs = hold(env, "x");
                env.agent("sess-1", dir)
                    .args(["acquire", "board", "--for", "10m"])
                    .assert()
                    .code(6);
                let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_clerk"));
                if json {
                    cmd.arg("--json");
                }
                let mut child = cmd
                    .args(["wait", "--follow"])
                    .env("CLERK_HOME", env.home())
                    .env(SESSION_VAR, "sess-1")
                    .env(CLAUDE_PID, std::process::id().to_string())
                    .current_dir(dir)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap();
                let out = lines(&mut child);
                free(env, "x", &theirs);
                let mut event = Vec::new();
                while let Ok(line) = out.recv_timeout(Duration::from_secs(3)) {
                    let last = json || line.starts_with("Next: ");
                    event.push(line);
                    if last {
                        break;
                    }
                }
                let _ = child.kill();
                let _ = child.wait();
                event.join("\n")
            }

            #[test]
            fn a_markdown_event_is_under_500_characters_with_notice_lease_and_next() {
                let env = Env::new();
                let dir = env.dir("alpha");
                let event = first_event(&env, &dir, false);
                assert!(
                    event.chars().count() < 500,
                    "{} chars:\n{event}",
                    event.chars().count()
                );
                assert!(event.starts_with("## Notice: offered"), "{event}");
                assert!(event.contains("lse_"), "{event}");
                assert!(
                    event.lines().last().unwrap().starts_with("Next: "),
                    "{event}"
                );
            }

            #[test]
            fn a_json_event_is_under_500_characters_with_notice_lease_and_next() {
                let env = Env::new();
                let dir = env.dir("alpha");
                let event = first_event(&env, &dir, true);
                assert!(
                    event.chars().count() < 500,
                    "{} chars:\n{event}",
                    event.chars().count()
                );
                let v: Value = serde_json::from_str(&event).unwrap();
                assert_eq!(v["notice"]["kind"], "offered", "{v}");
                assert!(v["claimed"]["lease_id"].is_string(), "{v}");
                assert!(v["next"].is_string(), "{v}");
            }
        }

        mod on_an_agent_session_in_markdown {
            use super::*;

            fn text_of(cmd: &mut Command) -> String {
                String::from_utf8(cmd.output().unwrap().stdout).unwrap()
            }

            #[test]
            fn one_batch_is_notice_blocks_the_claimed_lease_and_next_last() {
                let env = Env::new();
                let dir = env.dir("alpha");
                let theirs = agent_queued(&env, &dir);
                free(&env, "x", &theirs);
                let text = text_of(env.agent_md("sess-1", &dir).arg("wait"));
                assert!(text.contains("## Claimed: lease lse_"), "{text}");
                assert!(
                    text.trim_end()
                        .lines()
                        .last()
                        .unwrap()
                        .starts_with("Next: "),
                    "{text}"
                );
                assert!(text.contains("clerk wait"), "{text}");
                assert!(!text.contains('{'), "{text}");
            }

            /// Notices that need no action must not end a wait: each wake
            /// re-reads the agent's whole conversation.
            #[test]
            fn a_cleanup_notice_of_a_finished_lease_does_not_end_the_wait() {
                let env = Env::new();
                let dir = env.dir("alpha");
                seed(&env, "board", "k1");
                let lease = lease_of(&json_of(
                    env.agent("sess-1", &dir)
                        .args(["acquire", "board", "--for", "10m"]),
                ));
                json_of(env.agent("sess-1", &dir).args(["release", &lease]));
                let text = text_of(
                    env.agent_md("sess-1", &dir)
                        .args(["wait", "--timeout", "1s"]),
                );
                assert!(!text.contains("## Notice"), "{text}");
            }

            mod on_an_offer_claimed_by_the_wait {
                use super::*;

                fn claimed_batch() -> String {
                    let env = Env::new();
                    let dir = env.dir("alpha");
                    let theirs = agent_queued(&env, &dir);
                    free(&env, "x", &theirs);
                    text_of(env.agent_md("sess-1", &dir).arg("wait"))
                }

                #[test]
                fn prints_a_claimed_heading_and_no_claim_before_the_deadline_text() {
                    let text = claimed_batch();
                    assert!(text.contains("## Claimed: lease lse_"), "{text}");
                    assert!(!text.contains("before the deadline"), "{text}");
                    assert!(!text.contains("## Notice: offered"), "{text}");
                }

                #[test]
                fn next_does_not_ask_to_claim() {
                    let text = claimed_batch();
                    let next = text.lines().find(|l| l.starts_with("Next: ")).unwrap();
                    assert!(next.contains("It is yours (claimed)"), "{text}");
                    assert!(!next.contains("claim lease"), "{text}");
                    assert!(!next.contains("Act on each notice"), "{text}");
                }
            }

            mod on_a_follow_wait_killed_after_notices_were_printed {
                use super::*;

                #[test]
                fn the_last_line_does_not_say_before_any_notice_arrived() {
                    let env = Env::new();
                    let dir = env.dir("alpha");
                    let theirs = agent_queued(&env, &dir);
                    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_clerk"))
                        .args(["wait", "--follow"])
                        .env("CLERK_HOME", env.home())
                        .env(SESSION_VAR, "sess-1")
                        .env(CLAUDE_PID, std::process::id().to_string())
                        .current_dir(&dir)
                        .stdin(Stdio::null())
                        .stdout(Stdio::piped())
                        .spawn()
                        .unwrap();
                    let out = lines(&mut child);
                    free(&env, "x", &theirs);
                    let mut seen = Vec::new();
                    while let Ok(line) = out.recv_timeout(PATIENCE) {
                        let next = line.starts_with("Next: ");
                        seen.push(line);
                        if next {
                            break;
                        }
                    }
                    let pid = i32::try_from(child.id()).unwrap();
                    // SAFETY: plain kill(2) on our own child.
                    unsafe { libc::kill(pid, libc::SIGTERM) };
                    while let Ok(line) = out.recv_timeout(PATIENCE) {
                        seen.push(line);
                    }
                    let _ = child.wait();
                    let text = seen.join("\n");
                    assert!(text.contains("## Notice: offered"), "{text}");
                    assert!(
                        text.contains("The wait was stopped (time limit or signal)."),
                        "{text}"
                    );
                    assert!(!text.contains("before any notice arrived"), "{text}");
                }
            }

            #[test]
            fn a_position_change_alone_does_not_end_the_wait() {
                let env = Env::new();
                let dir = env.dir("alpha");
                seed(&env, "board", "k1");
                hold(&env, "x");
                let ahead = env
                    .rpc(
                        "acquire",
                        &user("ahead"),
                        json!({"target": {"resource": "board"}, "duration_ms": 600_000,
                               "purpose": "first in line"}),
                    )
                    .unwrap();
                env.agent("sess-1", &dir)
                    .args(["acquire", "board", "--for", "10m"])
                    .assert()
                    .code(6);
                env.rpc(
                    "cancel",
                    &user("ahead"),
                    json!({"request_id": ahead["request_id"]}),
                )
                .unwrap();
                let text = text_of(
                    env.agent_md("sess-1", &dir)
                        .args(["wait", "--timeout", "1s"]),
                );
                assert!(!text.contains("## Notice"), "{text}");
            }

            #[test]
            fn a_timeout_is_one_sentence_and_next() {
                let env = Env::new();
                let dir = env.dir("alpha");
                let text = text_of(
                    env.agent_md("sess-1", &dir)
                        .args(["wait", "--timeout", "1s"]),
                );
                let lines: Vec<&str> = text.trim().lines().filter(|l| !l.is_empty()).collect();
                assert_eq!(lines.len(), 2, "{text}");
                assert!(lines[1].starts_with("Next: "), "{text}");
            }

            #[test]
            fn follow_prints_one_block_per_notice_each_with_next() {
                let env = Env::new();
                let dir = env.dir("alpha");
                let theirs = agent_queued(&env, &dir);
                let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_clerk"))
                    .args(["wait", "--follow"])
                    .env("CLERK_HOME", env.home())
                    .env(SESSION_VAR, "sess-1")
                    .env(CLAUDE_PID, std::process::id().to_string())
                    .current_dir(&dir)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap();
                let out = lines(&mut child);
                free(&env, "x", &theirs);
                let mut seen = Vec::new();
                while let Ok(line) = out.recv_timeout(Duration::from_secs(3)) {
                    seen.push(line);
                    if seen.last().is_some_and(|l| l.starts_with("Next: ")) {
                        break;
                    }
                }
                let _ = child.kill();
                let _ = child.wait();
                let text = seen.join("\n");
                assert!(text.starts_with("## Notice: offered"), "{text}");
                assert!(text.contains("- claimed: lse_"), "{text}");
                assert!(text.ends_with(&seen.last().unwrap().clone()), "{text}");
                assert!(seen.last().unwrap().starts_with("Next: "), "{text}");
            }

            #[test]
            fn an_error_in_follow_reaches_stdout() {
                let env = Env::new();
                let dir = env.dir("alpha");
                let text = text_of(
                    env.cmd()
                        .args(["wait", "--follow"])
                        .env(SESSION_VAR, "sess-1")
                        .current_dir(&dir),
                );
                assert!(text.starts_with("## Error: agent_unidentified"), "{text}");
            }
        }

        mod on_user {
            use super::*;

            #[test]
            fn sees_the_notices_of_its_detached_requests() {
                let env = Env::new();
                seed(&env, "board", "k1");
                let theirs = hold(&env, "x");
                env.cmd()
                    .args(["acquire", "board", "--for", "10m"])
                    .assert()
                    .code(6);
                free(&env, "x", &theirs);
                let body = env.ok(&["wait", "--once"]);
                let kinds = strings(&body, "kind");
                assert!(kinds.contains(&"offered".to_owned()), "{body}");
            }
        }
    }

    mod agent_commands {
        use super::*;

        /// `sess-1` in a fresh project holding `board`; its lease id.
        fn agent_holds(env: &Env) -> (PathBuf, String) {
            seed(env, "board", "k1");
            let dir = env.dir("alpha");
            let body = json_of(
                env.agent("sess-1", &dir)
                    .args(["acquire", "board", "--for", "10m"]),
            );
            (dir, lease_of(&body))
        }

        mod acquire {
            use super::*;

            #[test]
            fn on_a_free_unit_the_grant_carries_summary_and_next_naming_release() {
                let env = Env::new();
                seed(&env, "board", "k1");
                let dir = env.dir("alpha");
                let body = json_of(
                    env.agent("sess-1", &dir)
                        .args(["acquire", "board", "--for", "10m"]),
                );
                assert!(!body["summary"].as_array().unwrap().is_empty(), "{body}");
                assert!(
                    body["next"].as_str().unwrap().contains("clerk release"),
                    "{body}"
                );
            }

            #[test]
            fn on_queued_exits_6_and_next_names_the_wait() {
                let env = Env::new();
                seed(&env, "board", "k1");
                hold(&env, "x");
                let dir = env.dir("alpha");
                let out = env
                    .agent("sess-1", &dir)
                    .args(["acquire", "board", "--for", "10m"])
                    .output()
                    .unwrap();
                assert_eq!(out.status.code(), Some(6), "{out:?}");
                let body: Value = serde_json::from_slice(&out.stdout).unwrap();
                assert!(
                    body["next"].as_str().unwrap().contains("run_in_background"),
                    "{body}"
                );
            }

            /// Research finding 3: no `?` in the summary.
            #[test]
            fn on_queued_the_summary_names_the_resource_and_cancel_names_the_request() {
                let env = Env::new();
                seed(&env, "board", "k1");
                hold(&env, "x");
                let dir = env.dir("alpha");
                let out = env
                    .agent("sess-1", &dir)
                    .args(["acquire", "board/u1", "--for", "10m"])
                    .output()
                    .unwrap();
                let body: Value = serde_json::from_slice(&out.stdout).unwrap();
                let summary = body["summary"][0].as_str().unwrap();
                assert!(
                    summary.starts_with("Queued for board/u1 at position 1"),
                    "{body}"
                );
                let request = body["request_id"].as_str().unwrap();
                let cancelled = json_of(env.agent("sess-1", &dir).args(["cancel", request]));
                assert_eq!(
                    cancelled["summary"][0],
                    format!("Request {request} is withdrawn."),
                    "{cancelled}"
                );
            }

            #[test]
            fn with_wait_is_refused_pointing_to_clerk_wait() {
                let env = Env::new();
                seed(&env, "board", "k1");
                let dir = env.dir("alpha");
                let error = error_of(
                    env.agent("sess-1", &dir)
                        .args(["acquire", "board", "--for", "10m", "--wait"]),
                    2,
                );
                assert!(
                    error["message"].as_str().unwrap().contains("clerk wait"),
                    "{error}"
                );
            }
        }

        mod types {
            use super::*;

            #[test]
            fn lists_the_three_types_with_summary_and_next() {
                let env = Env::new();
                let dir = env.dir("alpha");
                let body = json_of(env.agent("sess-1", &dir).args(["types"]));
                let names = strings(&body["types"], "type");
                assert_eq!(names, ["device", "service", "generic"], "{body}");
                assert!(
                    body["next"]
                        .as_str()
                        .unwrap()
                        .contains("clerk resource create")
                );
            }
        }

        mod release_with_cleanup {
            use super::*;

            #[test]
            fn starts_the_cleanup_window_and_confirm_clean_ends_it() {
                let env = Env::new();
                let (dir, lease) = agent_holds(&env);
                let body =
                    json_of(
                        env.agent("sess-1", &dir)
                            .args(["release", &lease, "--cleanup"]),
                    );
                assert_eq!(body["state"], "cleaning", "{body}");
                assert!(
                    body["next"].as_str().unwrap().contains("clerk confirm"),
                    "{body}"
                );
                let body = json_of(env.agent("sess-1", &dir).args(["confirm", &lease, "clean"]));
                assert_eq!(body["state"], "ended", "{body}");
            }
        }

        mod interruptible {
            use super::*;

            #[test]
            fn on_turns_the_lease_interruptible() {
                let env = Env::new();
                let (dir, lease) = agent_holds(&env);
                let body = json_of(
                    env.agent("sess-1", &dir)
                        .args(["interruptible", &lease, "on"]),
                );
                assert_eq!(body["interruptible"], true, "{body}");
                assert!(body["next"].is_string(), "{body}");
            }
        }

        mod yield_refuse {
            use super::*;

            #[test]
            fn without_a_yield_request_fails_with_a_next_step() {
                let env = Env::new();
                let (dir, lease) = agent_holds(&env);
                let out = env
                    .agent("sess-1", &dir)
                    .args(["yield", &lease, "--refuse"])
                    .output()
                    .unwrap();
                assert!(!out.status.success(), "{out:?}");
                let body: Value = serde_json::from_slice(&out.stdout).unwrap();
                assert!(
                    body["error"]["message"]
                        .as_str()
                        .unwrap()
                        .contains("clerk leases --mine"),
                    "{body}"
                );
            }
        }

        mod reclaim_and_claim {
            use super::*;

            #[test]
            fn exist_and_fail_cleanly_on_a_lease_with_nothing_to_do() {
                let env = Env::new();
                let (dir, lease) = agent_holds(&env);
                for cmd in ["reclaim", "claim"] {
                    let out = env
                        .agent("sess-1", &dir)
                        .args([cmd, &lease])
                        .output()
                        .unwrap();
                    let body: Value = serde_json::from_slice(&out.stdout)
                        .unwrap_or_else(|e| panic!("{cmd}: {e}: {out:?}"));
                    assert_ne!(body["error"]["code"], "usage_error", "{cmd}: {body}");
                    assert!(
                        body.get("error").is_some() || body["next"].is_string(),
                        "{cmd}: {body}"
                    );
                }
            }
        }

        mod leases_mine {
            use super::*;

            #[test]
            fn says_whether_a_wait_is_armed() {
                let env = Env::new();
                let (dir, _) = agent_holds(&env);
                let body = json_of(env.agent("sess-1", &dir).args(["leases", "--mine"]));
                assert_eq!(body["armed"], false, "{body}");
                let _wait = spawn(&env, &["wait"], Some("sess-1"));
                std::thread::sleep(Duration::from_millis(800));
                let body = json_of(env.agent("sess-1", &dir).args(["leases", "--mine"]));
                assert_eq!(body["armed"], true, "{body}");
            }
        }

        mod errors {
            use super::*;

            #[test]
            fn not_found_ends_with_the_next_step() {
                let env = Env::new();
                let dir = env.dir("alpha");
                let error = error_of(
                    env.agent("sess-1", &dir)
                        .args(["release", "lse_01ARZ3NDEKTSV4RRFFQ69G5FAV"]),
                    3,
                );
                assert!(
                    error["message"]
                        .as_str()
                        .unwrap()
                        .ends_with("to see where you stand."),
                    "{error}"
                );
            }
        }
    }

    /// Graceful restarts: a restart is announced and waiting clients
    /// reconnect; a stop ends them cleanly.
    mod daemon_restart_under_waiting_clients {
        use super::*;

        #[test]
        fn an_agents_wait_survives_a_restart_and_gets_the_offer() {
            let env = Env::new();
            seed(&env, "board", "k1");
            let theirs = hold(&env, "x");
            let dir = env.dir("alpha");
            env.agent("sess-1", &dir)
                .args(["acquire", "board", "--for", "10m"])
                .assert()
                .code(6);
            let mut wait = spawn(&env, &["wait"], Some("sess-1"));
            std::thread::sleep(Duration::from_millis(800));
            env.ok(&["daemon", "restart"]);
            std::thread::sleep(Duration::from_millis(800));
            assert!(
                wait.try_wait().unwrap().is_none(),
                "the wait died on the restart"
            );
            free(&env, "x", &theirs);
            assert_eq!(code_of(&mut wait), 0);
            let mut out = String::new();
            std::io::Read::read_to_string(&mut wait.0.stdout.take().unwrap(), &mut out).unwrap();
            let body: Value = serde_json::from_str(out.trim()).unwrap();
            assert_eq!(body["notices"][0]["kind"], "offered", "{body}");
        }

        #[test]
        fn an_agents_wait_ends_cleanly_on_a_stop() {
            let env = Env::new();
            let dir = env.dir("alpha");
            env.agent("sess-1", &dir)
                .args(["leases", "--mine"])
                .assert()
                .success();
            let mut wait = spawn(&env, &["wait"], Some("sess-1"));
            std::thread::sleep(Duration::from_millis(800));
            env.ok(&["daemon", "stop"]);
            assert_eq!(code_of(&mut wait), 0);
            let mut out = String::new();
            std::io::Read::read_to_string(&mut wait.0.stdout.take().unwrap(), &mut out).unwrap();
            let body: Value = serde_json::from_str(out.trim()).unwrap();
            assert!(
                body["summary"][0].as_str().unwrap().contains("stopped"),
                "{body}"
            );
        }

        #[test]
        fn an_agents_wait_ends_cleanly_when_the_new_daemon_speaks_another_protocol() {
            let env = Env::new();
            let dir = env.dir("alpha");
            env.agent("sess-1", &dir)
                .args(["leases", "--mine"])
                .assert()
                .success();
            let mut wait = spawn(&env, &["wait"], Some("sess-1"));
            std::thread::sleep(Duration::from_millis(800));
            let _ = env
                .cmd()
                .env("CLERK_TEST_DAEMON_PROTOCOL", "1")
                .args(["daemon", "restart"])
                .output()
                .unwrap();
            assert_eq!(code_of(&mut wait), 0);
            let mut out = String::new();
            std::io::Read::read_to_string(&mut wait.0.stdout.take().unwrap(), &mut out).unwrap();
            let body: Value = serde_json::from_str(out.trim()).unwrap();
            assert!(
                body["summary"][0].as_str().unwrap().contains("upgraded"),
                "{body}"
            );
        }

        #[test]
        fn acquire_wait_survives_a_restart_and_gets_the_grant() {
            let env = Env::new();
            seed(&env, "board", "k1");
            let theirs = hold(&env, "x");
            let mut child = spawn(&env, &["acquire", "board", "--for", "10m", "--wait"], None);
            std::thread::sleep(Duration::from_millis(800));
            env.ok(&["daemon", "restart"]);
            std::thread::sleep(Duration::from_millis(800));
            assert!(
                child.try_wait().unwrap().is_none(),
                "acquire --wait died on the restart"
            );
            free(&env, "x", &theirs);
            assert_eq!(code_of(&mut child), 0);
        }
    }

    /// A deliberate stop stays stopped while clients wait: none of them
    /// starts a daemon again, by RPC or by signal.
    mod daemon_stop_with_waiters {
        use super::*;

        /// How long a stopped daemon is watched for coming back: past
        /// `RECONNECT_START_AFTER` and a whole wait step.
        const STAYS_DOWN: Duration = Duration::from_secs(10);

        /// An environment whose clerk is a private copy of the binary, so
        /// whatever daemon its waiters could start runs from that copy.
        struct Stopping {
            env: Env,
            clerk: PathBuf,
        }

        impl Stopping {
            /// With its daemon running, started from the private copy.
            fn new() -> Self {
                let env = Env::new();
                let clerk = env.home().join("clerk-private");
                fs::copy(env!("CARGO_BIN_EXE_clerk"), &clerk).unwrap();
                let this = Self { env, clerk };
                let mut status = std::process::Command::new(&this.clerk);
                status
                    .arg("status")
                    .env("CLERK_HOME", this.env.home())
                    .env_remove(SESSION_VAR)
                    .env_remove(CLAUDE_PID)
                    .stdin(Stdio::null());
                // Another test's fork may hold the copy's write descriptor
                // until that child execs: the copy is "busy" until then.
                let deadline = Instant::now() + PATIENCE;
                let out = loop {
                    match status.output() {
                        Err(e)
                            if e.raw_os_error() == Some(libc::ETXTBSY)
                                && Instant::now() < deadline =>
                        {
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        got => break got.unwrap(),
                    }
                };
                assert!(out.status.success(), "{out:?}");
                this
            }

            /// [`spawn`], from the private copy.
            fn spawn(&self, args: &[&str], session: Option<&str>) -> Bg {
                use std::os::unix::process::CommandExt;
                let mut cmd = std::process::Command::new(&self.clerk);
                cmd.process_group(0);
                cmd.args(args)
                    .env("CLERK_HOME", self.env.home())
                    .env_remove(SESSION_VAR)
                    .env_remove(CLAUDE_PID)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                if let Some(session) = session {
                    cmd.arg("--json")
                        .env(SESSION_VAR, session)
                        .env(CLAUDE_PID, std::process::id().to_string());
                }
                Bg(cmd.spawn().unwrap())
            }

            /// The daemon's pid, from its lock file (`None` while a new
            /// daemon is still writing it).
            fn daemon_pid(&self) -> Option<libc::pid_t> {
                let text = fs::read_to_string(self.env.paths().lock).ok()?;
                text.trim().parse().ok()
            }

            fn signal_daemon(&self, signal: libc::c_int) {
                let pid = self.daemon_pid().expect("the daemon recorded its pid");
                // SAFETY: kill(2) on the daemon this test's clerk started.
                assert_eq!(unsafe { libc::kill(pid, signal) }, 0);
            }

            fn stop_by_rpc(&self) {
                self.env.cmd().args(["daemon", "stop"]).assert().success();
            }

            fn stop_by_sigterm(&self) {
                self.signal_daemon(libc::SIGTERM);
                poll_until("the daemon is gone", || no_daemon(&self.env));
            }

            /// Assert no daemon runs, now and for all of [`STAYS_DOWN`].
            fn assert_stays_down(&self) {
                let until = Instant::now() + STAYS_DOWN;
                while Instant::now() < until {
                    assert!(no_daemon(&self.env), "a daemon came back after the stop");
                    std::thread::sleep(Duration::from_millis(50));
                }
            }

            /// An agent wait of `sess-1` (with `extra` arguments), armed.
            fn armed_wait(&self, extra: &[&str]) -> Bg {
                let dir = self.env.dir("alpha");
                self.env
                    .agent("sess-1", &dir)
                    .args(["leases", "--mine"])
                    .assert()
                    .success();
                let mut args = vec!["wait"];
                args.extend_from_slice(extra);
                let wait = self.spawn(&args, Some("sess-1"));
                poll_until("the wait is armed", || {
                    self.env
                        .rpc("session_brief", &user("x"), json!({"session_id": "sess-1"}))
                        .is_ok_and(|brief| brief["open_waits"].as_u64() > Some(0))
                });
                wait
            }

            /// A user's `acquire --wait`, queued behind another holder.
            fn queued_acquire(&self) -> Bg {
                seed(&self.env, "board", "k1");
                hold(&self.env, "x");
                let child = self.spawn(&["acquire", "board", "--for", "10m", "--wait"], None);
                poll_until("the request is queued", || {
                    self.env.ok(&["queue", "board"])[0]["queue"] != json!([])
                });
                child
            }

            /// `clerk run` supervising a shell that ends once the `go` file
            /// appears; returns it and that file's path.
            fn supervising_run(&self) -> (Bg, PathBuf) {
                seed(&self.env, "board", "k1");
                let started = self.env.home().join("started");
                let go = self.env.home().join("go");
                let script = format!(
                    "touch {}; while [ ! -e {} ]; do sleep 0.05; done",
                    started.display(),
                    go.display()
                );
                let child = self.spawn(
                    &["run", "board", "--for", "10m", "--", "sh", "-c", &script],
                    None,
                );
                poll_until("the command runs", || started.exists());
                (child, go)
            }

            /// A live view holding the board, on a terminal.
            fn live_view(&self) -> common::Tty {
                seed(&self.env, "board", "k1");
                let tty = common::Tty::spawn_in(
                    self.env.home(),
                    "\"$CLERK\" acquire board --for 10m; echo EXIT=$?",
                    &[("CLERK", self.clerk.to_str().unwrap())],
                );
                tty.expect("Granted");
                tty
            }
        }

        /// Poll `done` until it holds; panic after [`PATIENCE`].
        fn poll_until(what: &str, mut done: impl FnMut() -> bool) {
            let deadline = Instant::now() + PATIENCE;
            while !done() {
                assert!(Instant::now() < deadline, "timed out waiting until {what}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        mod on_daemon_stop_with_an_agent_wait_follow {
            use super::*;

            #[test]
            fn no_daemon_runs_10_s_later() {
                let s = Stopping::new();
                let _wait = s.armed_wait(&["--follow"]);
                s.stop_by_rpc();
                s.assert_stays_down();
            }

            #[test]
            fn the_wait_prints_the_stopped_notice_and_exits_5() {
                let s = Stopping::new();
                let mut wait = s.armed_wait(&["--follow"]);
                s.stop_by_rpc();
                assert_eq!(code_of(&mut wait), 5);
                let out = stdout_of(&mut wait);
                let body: Value = serde_json::from_str(out.trim()).unwrap();
                assert!(
                    body["summary"][0].as_str().unwrap().contains("stopped"),
                    "{body}"
                );
            }
        }

        mod on_sigterm_with_an_agent_wait_follow {
            use super::*;

            #[test]
            fn no_daemon_runs_10_s_later() {
                let s = Stopping::new();
                let _wait = s.armed_wait(&["--follow"]);
                s.stop_by_sigterm();
                s.assert_stays_down();
            }
        }

        mod on_daemon_stop_with_an_agent_wait {
            use super::*;

            #[test]
            fn no_daemon_runs_10_s_later() {
                let s = Stopping::new();
                let _wait = s.armed_wait(&[]);
                s.stop_by_rpc();
                s.assert_stays_down();
            }
        }

        mod on_sigterm_with_an_agent_wait {
            use super::*;

            #[test]
            fn no_daemon_runs_10_s_later() {
                let s = Stopping::new();
                let _wait = s.armed_wait(&[]);
                s.stop_by_sigterm();
                s.assert_stays_down();
            }
        }

        /// CQ-L3: `--timeout` also bounds the time a step spends
        /// reconnecting to a daemon that does not come back.
        mod on_timeout_5s_while_the_daemon_is_unreachable {
            use super::*;

            #[test]
            fn ends_within_5_s_plus_one_step() {
                let s = Stopping::new();
                let mut wait = s.armed_wait(&["--timeout", "5s"]);
                let started = Instant::now();
                // Killed, and no daemon can start again: the private copy
                // is gone, so the waiter's relaunch fails.
                s.signal_daemon(libc::SIGKILL);
                fs::remove_file(&s.clerk).unwrap();
                let status = wait_within(&mut wait, Duration::from_secs(12));
                assert!(status.is_some(), "the wait outlived --timeout by far");
                let took = started.elapsed();
                assert!(took < Duration::from_secs(9), "took {took:?}");
                let out = stdout_of(&mut wait);
                assert!(out.contains("--timeout"), "{out}");
            }
        }

        /// A wait without --timeout still gives up on a daemon that never
        /// comes back, as the reconnect budget says.
        mod on_no_timeout_while_the_daemon_cannot_come_back {
            use super::*;

            #[test]
            fn ends_with_daemon_unavailable_after_about_the_reconnect_budget() {
                let s = Stopping::new();
                let mut wait = s.armed_wait(&[]);
                let started = Instant::now();
                s.signal_daemon(libc::SIGKILL);
                fs::remove_file(&s.clerk).unwrap();
                let status = wait_within(&mut wait, Duration::from_secs(50));
                let status = status.expect("the wait never gave up");
                let took = started.elapsed();
                assert!(!status.success(), "{status:?}");
                assert!(took >= Duration::from_secs(25), "took {took:?}");
                let out = stdout_of(&mut wait) + &stderr_of(&mut wait);
                assert!(out.contains("daemon_unavailable"), "{out}");
            }
        }

        mod on_an_agent_wait_started_after_a_stop {
            use super::*;

            #[test]
            fn says_stopped_without_starting_a_daemon() {
                let s = Stopping::new();
                s.stop_by_rpc();
                let mut wait = s.spawn(&["wait", "--follow"], Some("sess-1"));
                assert_eq!(code_of(&mut wait), 5);
                let out = stdout_of(&mut wait);
                assert!(out.contains("stopped"), "{out}");
                assert!(no_daemon(&s.env), "the wait started a daemon");
            }
        }

        mod on_daemon_stop_with_acquire_wait_queued {
            use super::*;

            #[test]
            fn no_daemon_runs_10_s_later() {
                let s = Stopping::new();
                let _child = s.queued_acquire();
                s.stop_by_rpc();
                s.assert_stays_down();
            }

            #[test]
            fn the_waiter_says_its_request_ends_with_the_daemon() {
                let s = Stopping::new();
                let mut child = s.queued_acquire();
                s.stop_by_rpc();
                assert_eq!(code_of(&mut child), 5);
                let said = stderr_of(&mut child);
                assert!(said.contains("ends with the daemon"), "{said}");
                assert!(!said.contains("cancel"), "{said}");
            }
        }

        mod on_daemon_stop_with_an_agents_clerk_run_queued {
            use super::*;

            /// An agent session's `clerk run`, queued behind another
            /// holder; returns it and the request id.
            fn queued_run(s: &Stopping) -> (Bg, String) {
                seed(&s.env, "board", "k1");
                hold(&s.env, "x");
                let child = s.spawn(
                    &["run", "board", "--for", "10m", "--", "true"],
                    Some("sess-1"),
                );
                let mut request = None;
                poll_until("the request is queued", || {
                    let queue = s.env.ok(&["queue", "board"])[0]["queue"].clone();
                    request = queue[0]["request_id"].as_str().map(str::to_owned);
                    request.is_some()
                });
                (child, request.unwrap())
            }

            #[test]
            fn the_waiter_says_the_request_stays_queued_and_how_to_cancel_it() {
                let s = Stopping::new();
                let (mut child, request) = queued_run(&s);
                s.stop_by_rpc();
                assert_eq!(code_of(&mut child), 5);
                let said = stderr_of(&mut child);
                assert!(said.contains("stays queued"), "{said}");
                assert!(said.contains(&format!("clerk cancel {request}")), "{said}");
                assert!(!said.contains("ends with the daemon"), "{said}");
            }

            #[test]
            fn the_request_is_still_queued_after_the_next_start() {
                let s = Stopping::new();
                let (mut child, request) = queued_run(&s);
                s.stop_by_rpc();
                assert_eq!(code_of(&mut child), 5);
                let queue = s.env.ok(&["queue", "board"])[0]["queue"].clone();
                assert_eq!(queue[0]["request_id"], request.as_str(), "{queue}");
            }
        }

        mod on_daemon_stop_with_an_agents_live_view_holding {
            use super::*;

            #[test]
            fn the_view_says_the_lease_stays_and_how_to_release_it() {
                let s = Stopping::new();
                seed(&s.env, "board", "k1");
                let pid = std::process::id().to_string();
                let tty = common::Tty::spawn_in(
                    s.env.home(),
                    "\"$CLERK\" acquire board --for 10m; echo EXIT=$?",
                    &[
                        ("CLERK", s.clerk.to_str().unwrap()),
                        (SESSION_VAR, "sess-1"),
                        (CLAUDE_PID, &pid),
                    ],
                );
                let mut lease = None;
                poll_until("the session holds the board", || {
                    let occupancy = unit_status(&s.env)["occupancy"].clone();
                    lease = occupancy["lease_id"].as_str().map(str::to_owned);
                    lease.is_some()
                });
                s.stop_by_rpc();
                tty.expect("stays yours");
                tty.expect(&format!("clerk release {}", lease.unwrap()));
                tty.expect("EXIT=5");
            }
        }

        mod on_sigterm_with_acquire_wait_queued {
            use super::*;

            #[test]
            fn no_daemon_runs_10_s_later() {
                let s = Stopping::new();
                let _child = s.queued_acquire();
                s.stop_by_sigterm();
                s.assert_stays_down();
            }
        }

        mod on_daemon_stop_with_clerk_run_supervising_a_child {
            use super::*;

            #[test]
            fn no_daemon_runs_10_s_later() {
                let s = Stopping::new();
                let (_child, _go) = s.supervising_run();
                s.stop_by_rpc();
                s.assert_stays_down();
            }

            #[test]
            fn run_keeps_supervising_the_child_without_notices_and_says_so_on_stderr() {
                let s = Stopping::new();
                let (mut child, go) = s.supervising_run();
                s.stop_by_rpc();
                assert!(child.try_wait().unwrap().is_none(), "run ended on the stop");
                fs::write(&go, "").unwrap();
                assert_eq!(code_of(&mut child), 0);
                let said = stderr_of(&mut child);
                assert!(said.contains("without notices"), "{said}");
            }
        }

        mod on_sigterm_with_clerk_run_supervising_a_child {
            use super::*;

            #[test]
            fn no_daemon_runs_10_s_later() {
                let s = Stopping::new();
                let (_child, _go) = s.supervising_run();
                s.stop_by_sigterm();
                s.assert_stays_down();
            }
        }

        mod on_daemon_stop_with_clerk_live_open {
            use super::*;

            #[test]
            fn no_daemon_runs_10_s_later() {
                let s = Stopping::new();
                let _tty = s.live_view();
                s.stop_by_rpc();
                s.assert_stays_down();
            }

            #[test]
            fn the_view_says_so_and_exits_5() {
                let s = Stopping::new();
                let tty = s.live_view();
                s.stop_by_rpc();
                tty.expect("was stopped");
                tty.expect("EXIT=5");
            }
        }

        mod on_sigterm_with_clerk_live_open {
            use super::*;

            #[test]
            fn no_daemon_runs_10_s_later() {
                let s = Stopping::new();
                let _tty = s.live_view();
                s.stop_by_sigterm();
                s.assert_stays_down();
            }
        }

        mod on_a_one_shot_command_after_a_stop {
            use super::*;

            #[test]
            fn starts_the_daemon_and_removes_the_marker() {
                let s = Stopping::new();
                s.stop_by_rpc();
                assert!(
                    s.env.paths().stop_marker().exists(),
                    "no marker after a stop"
                );
                s.env.ok(&["status"]);
                assert!(!no_daemon(&s.env), "the command started no daemon");
                assert!(!s.env.paths().stop_marker().exists());
            }
        }

        mod on_a_crash_with_an_agent_wait_running {
            use super::*;

            #[test]
            fn the_wait_starts_a_daemon_again() {
                let s = Stopping::new();
                let mut wait = s.armed_wait(&["--follow"]);
                let old = s.daemon_pid();
                s.signal_daemon(libc::SIGKILL);
                let socket = s.env.paths().socket;
                // No lock probe here: it could make the new daemon's own
                // try-lock fail.
                poll_until("a new daemon answers", || {
                    s.daemon_pid().is_some_and(|pid| Some(pid) != old)
                        && std::os::unix::net::UnixStream::connect(&socket).is_ok()
                });
                assert!(wait.try_wait().unwrap().is_none(), "the wait ended");
            }
        }
    }

    mod next_after_daemon_stop {
        use super::*;

        mod on_agent_daemon_stop {
            use super::*;

            #[test]
            fn next_says_the_daemon_stays_stopped_until_the_next_clerk_command() {
                let env = Env::new();
                let dir = env.dir("alpha");
                env.agent("sess-1", &dir)
                    .args(["leases", "--mine"])
                    .assert()
                    .success();
                let body = json_of(env.agent("sess-1", &dir).args([
                    "daemon",
                    "stop",
                    "--override",
                    "the user asked",
                ]));
                let next = body["next"].as_str().unwrap();
                assert!(
                    next.contains("stays stopped until the next clerk command"),
                    "{body}"
                );
            }
        }
    }

    mod agent_md {
        use super::*;

        fn stdout(cmd: &mut Command) -> (String, Option<i32>) {
            let out = cmd.output().unwrap();
            (String::from_utf8(out.stdout).unwrap(), out.status.code())
        }

        #[test]
        fn a_grant_is_markdown_with_its_heading_and_next_last() {
            let env = Env::new();
            seed(&env, "board", "k1");
            let dir = env.dir("alpha");
            let (text, code) = stdout(
                env.agent_md("sess-1", &dir)
                    .args(["acquire", "board", "--for", "10m"]),
            );
            assert_eq!(code, Some(0), "{text}");
            assert!(text.starts_with("## Granted: board/u1\n"), "{text}");
            assert!(text.contains("- lease_id: lse_"), "{text}");
            assert!(
                text.trim_end()
                    .lines()
                    .last()
                    .unwrap()
                    .starts_with("Next: "),
                "{text}"
            );
            assert!(!text.contains('{') && !text.contains('\u{1b}'), "{text}");
        }

        #[test]
        fn queued_is_markdown_and_next_names_the_wait() {
            let env = Env::new();
            seed(&env, "board", "k1");
            hold(&env, "x");
            let dir = env.dir("alpha");
            let (text, code) = stdout(
                env.agent_md("sess-1", &dir)
                    .args(["acquire", "board", "--for", "10m"]),
            );
            assert_eq!(code, Some(6), "{text}");
            assert!(text.starts_with("## Queued: board\n"), "{text}");
            assert!(text.contains("- request_id: req_"), "{text}");
            assert!(text.contains("run_in_background"), "{text}");
        }

        #[test]
        fn a_list_is_one_block_per_item_without_next() {
            let env = Env::new();
            seed(&env, "board", "k1");
            let dir = env.dir("alpha");
            let (text, _) = stdout(env.agent_md("sess-1", &dir).arg("resources"));
            assert!(text.starts_with("## board\n"), "{text}");
            assert!(!text.contains("Next:"), "{text}");
        }

        #[test]
        fn an_error_is_markdown_on_stdout_with_the_next_step() {
            let env = Env::new();
            let dir = env.dir("alpha");
            let (text, code) = stdout(
                env.agent_md("sess-1", &dir)
                    .args(["release", "lse_01ARZ3NDEKTSV4RRFFQ69G5FAV"]),
            );
            assert_eq!(code, Some(3), "{text}");
            assert!(text.starts_with("## Error: not_found\n"), "{text}");
            let message = text.lines().nth(1).unwrap();
            assert!(message.ends_with("to see where you stand."), "{text}");
            assert!(text.contains("- kind: lease"), "details follow: {text}");
        }

        #[test]
        fn a_mistyped_command_is_a_markdown_error() {
            let env = Env::new();
            let dir = env.dir("alpha");
            let (text, code) = stdout(env.agent_md("sess-1", &dir).args(["aquire", "board"]));
            assert_eq!(code, Some(2), "{text}");
            assert!(text.starts_with("## Error: usage_error\n"), "{text}");
            assert!(text.trim_end().ends_with("run it again."), "{text}");
        }

        /// Research finding 4: no epoch milliseconds in an agent's error.
        #[test]
        fn an_earlier_extension_shows_the_current_end_as_a_time() {
            let env = Env::new();
            seed(&env, "board", "k1");
            let dir = env.dir("alpha");
            let lease = lease_of(&json_of(
                env.agent("sess-1", &dir)
                    .args(["acquire", "board", "--for", "1h"]),
            ));
            let (text, code) = stdout(
                env.agent_md("sess-1", &dir)
                    .args(["extend", &lease, "--for", "1m"]),
            );
            assert_eq!(code, Some(2), "{text}");
            assert!(text.starts_with("## Error: extend_not_later"), "{text}");
            assert!(
                text.contains("- ends_at: ") && text.contains("(in "),
                "{text}"
            );
            assert!(!regex_like_epoch(&text), "{text}");
        }

        fn regex_like_epoch(text: &str) -> bool {
            text.split(|c: char| !c.is_ascii_digit())
                .any(|w| w.len() >= 13)
        }

        #[test]
        fn json_flag_prints_todays_json() {
            let env = Env::new();
            seed(&env, "board", "k1");
            let dir = env.dir("alpha");
            let body = json_of(
                env.agent_md("sess-1", &dir)
                    .args(["--json", "acquire", "board", "--for", "10m"]),
            );
            assert!(body["next"].is_string(), "{body}");
        }

        #[test]
        fn clerk_agent_format_human_prints_the_person_view_without_colour() {
            let env = Env::new();
            seed(&env, "board", "k1");
            let dir = env.dir("alpha");
            let (text, code) = stdout(
                env.agent_md("sess-1", &dir)
                    .env("CLERK_AGENT_FORMAT", "human")
                    .args(["acquire", "board", "--for", "10m"]),
            );
            assert_eq!(code, Some(0), "{text}");
            assert!(text.starts_with("Granted: board/u1"), "{text}");
            assert!(!text.contains("##") && !text.contains('{') && !text.contains('\u{1b}'));
        }

        #[test]
        fn clerk_agent_format_human_waits_in_the_person_view() {
            let env = Env::new();
            let dir = env.dir("alpha");
            let (text, code) = stdout(
                env.agent_md("sess-1", &dir)
                    .env("CLERK_AGENT_FORMAT", "human")
                    .args(["wait", "--timeout", "1s"]),
            );
            assert_eq!(code, Some(0), "{text}");
            assert!(text.contains("clerk wait") && !text.contains('{') && !text.contains("##"));
        }

        #[test]
        fn clerk_agent_format_json_prints_json() {
            let env = Env::new();
            seed(&env, "board", "k1");
            let dir = env.dir("alpha");
            let body = json_of(
                env.agent_md("sess-1", &dir)
                    .env("CLERK_AGENT_FORMAT", "json")
                    .args(["acquire", "board", "--for", "10m"]),
            );
            assert!(body["next"].is_string(), "{body}");
        }
    }

    mod cold_acquire {
        use super::*;

        /// A generic resource `job` with unit `u1` and health command `cmd`.
        fn custom_check(env: &Env, cmd: &str) {
            env.ok(&[
                "resource",
                "create",
                "job",
                "--type",
                "generic",
                "--health-command",
                cmd,
            ]);
            env.ok(&["unit", "add", "job", "u1", "--key", "k1"]);
        }

        mod on_a_device_with_local_checks {
            use super::*;

            #[test]
            fn grants_at_once_with_exit_0() {
                let env = Env::new();
                let path = env.home().join("tty-u1");
                fs::write(&path, b"").unwrap();
                env.ok(&["resource", "create", "dev", "--type", "device"]);
                env.ok(&["unit", "add", "dev", "u1", "--path", path.to_str().unwrap()]);
                let body = env.ok(&["acquire", "dev/u1", "--for", "10m"]);
                assert_eq!(body["outcome"], "granted", "{body}");
            }
        }

        mod on_a_custom_check_that_passes_in_about_1_s {
            use super::*;

            #[test]
            fn answers_granted() {
                let env = Env::new();
                custom_check(&env, "sleep 1");
                let body = env.ok(&["acquire", "job", "--for", "10m"]);
                assert_eq!(body["outcome"], "granted", "{body}");
            }

            #[test]
            fn leaves_no_offer_notice_behind() {
                let env = Env::new();
                custom_check(&env, "sleep 1");
                let body = env.ok(&["acquire", "job", "--for", "10m"]);
                let log = env.ok(&["log", "--action", "offered", "--limit", "1"]);
                assert_eq!(log, json!([]), "{body}");
            }
        }

        mod on_a_custom_check_that_fails {
            use super::*;

            #[test]
            fn answers_queued_with_blocked_by_unhealthy() {
                let env = Env::new();
                custom_check(&env, "echo broken >&2; exit 1");
                let out = env
                    .cmd()
                    .args(["acquire", "job", "--for", "10m"])
                    .output()
                    .unwrap();
                assert_eq!(out.status.code(), Some(6), "{out:?}");
                let body: Value = serde_json::from_slice(&out.stdout).unwrap();
                assert_eq!(body["blocked_by"]["kind"], "unhealthy", "{body}");
                assert_eq!(
                    body["blocked_by"]["units"][0]["message"], "broken",
                    "{body}"
                );
            }
        }

        mod on_daemon_exit_during_a_custom_check {
            use super::*;

            fn running(pid: &str) -> bool {
                fs::read_to_string(format!("/proc/{pid}/stat"))
                    .is_ok_and(|stat| !stat.rsplit(") ").next().unwrap().starts_with('Z'))
            }

            #[test]
            fn kills_the_check_and_its_process_group() {
                let env = Env::new();
                let marker = env.home().join("pids");
                // The shell and a child in its process group.
                custom_check(
                    &env,
                    &format!(
                        "sleep 300 & echo \"$$ $!\" > {}.tmp; mv {0}.tmp {0}; wait",
                        marker.display()
                    ),
                );
                let mut acquire = std::process::Command::new(env!("CARGO_BIN_EXE_clerk"))
                    .env("CLERK_HOME", env.home())
                    .env_remove(SESSION_VAR)
                    .env_remove(CLAUDE_PID)
                    .args(["acquire", "job", "--for", "10m"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(10);
                while !marker.exists() {
                    assert!(Instant::now() < deadline, "the check never started");
                    std::thread::sleep(Duration::from_millis(10));
                }
                let pids = fs::read_to_string(&marker).unwrap();
                env.ok(&["daemon", "stop"]);
                let _ = acquire.wait();
                let deadline = Instant::now() + Duration::from_secs(5);
                while pids.split_whitespace().any(running) {
                    assert!(Instant::now() < deadline, "still running: {pids}");
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }

    mod skip_checks {
        use super::*;

        /// A device resource `dev` with unit `u1` at a file `holder` keeps
        /// open (default health: exists and not open); the holder process.
        fn busy_device(env: &Env) -> std::process::Child {
            let path = env.home().join("tty-u1");
            fs::write(&path, b"").unwrap();
            env.ok(&["resource", "create", "dev", "--type", "device"]);
            env.ok(&["unit", "add", "dev", "u1", "--path", path.to_str().unwrap()]);
            let child = std::process::Command::new("sh")
                .arg("-c")
                .arg("exec 3<\"$0\"; exec sleep 300")
                .arg(&path)
                .spawn()
                .unwrap();
            std::thread::sleep(Duration::from_millis(300));
            child
        }

        struct Kill(std::process::Child);
        impl Drop for Kill {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        #[test]
        fn on_a_unit_open_by_a_process_a_normal_acquire_queues() {
            let env = Env::new();
            let _busy = Kill(busy_device(&env));
            env.cmd()
                .args(["acquire", "dev/u1", "--for", "10m"])
                .assert()
                .code(6);
        }

        #[test]
        fn grants_the_unit_without_running_its_health_checks() {
            let env = Env::new();
            let _busy = Kill(busy_device(&env));
            let body = env.ok(&["acquire", "dev/u1", "--for", "10m", "--skip-checks"]);
            assert_eq!(body["outcome"], "granted", "{body}");
        }

        #[test]
        fn is_recorded_in_the_audit_log() {
            let env = Env::new();
            let _busy = Kill(busy_device(&env));
            env.ok(&["acquire", "dev/u1", "--for", "10m", "--skip-checks"]);
            let log = env.ok(&["log", "--action", "granted", "--limit", "1"]);
            assert_eq!(log[0]["detail"]["skip_checks"], true, "{log}");
        }

        /// Peer report 4: a queued answer blocked by a health check points
        /// to --skip-checks (--override does not get past a health check).
        #[test]
        fn a_queued_agent_blocked_by_a_health_check_is_told_about_skip_checks() {
            let env = Env::new();
            let _busy = Kill(busy_device(&env));
            let dir = env.dir("alpha");
            let out = env
                .agent("sess-1", &dir)
                .args(["acquire", "dev/u1", "--for", "10m"])
                .output()
                .unwrap();
            assert_eq!(out.status.code(), Some(6), "{out:?}");
            let body: Value = serde_json::from_slice(&out.stdout).unwrap();
            assert!(
                body["next"].as_str().unwrap().contains("--skip-checks"),
                "{body}"
            );
        }

        #[test]
        fn on_a_whole_resource_is_refused() {
            let env = Env::new();
            let _busy = Kill(busy_device(&env));
            let error = error_of(
                env.cmd()
                    .args(["acquire", "dev", "--for", "10m", "--skip-checks"]),
                2,
            );
            assert!(
                error["message"].as_str().unwrap().contains("unit"),
                "{error}"
            );
        }

        #[test]
        fn never_takes_a_unit_from_its_lease_holder_and_skips_the_checks_when_offered() {
            let env = Env::new();
            let _busy = Kill(busy_device(&env));
            let theirs = env
                .rpc(
                    "acquire",
                    &user("x"),
                    json!({"target": {"resource": "dev", "unit": "u1"}, "duration_ms": 600_000,
                       "purpose": "theirs", "skip_checks": true}),
                )
                .unwrap();
            assert_eq!(theirs["outcome"], "granted", "{theirs}");
            let out = env
                .cmd()
                .args(["acquire", "dev/u1", "--for", "10m", "--skip-checks"])
                .output()
                .unwrap();
            assert_eq!(out.status.code(), Some(6), "{out:?}");
            env.rpc(
                "admin_release",
                &user("admin"),
                json!({ "lease_id": theirs["lease_id"] }),
            )
            .unwrap();
            let occupancy = unit_status(&env)["occupancy"].clone();
            assert_eq!(occupancy["state"], "offered", "{occupancy}");
        }
    }

    mod agent_policy {
        use super::*;

        /// Agent `sess` in `dir` holds `board`; returns its lease id.
        fn agent_holds(env: &Env, sess: &str, dir: &Path) -> String {
            lease_of(&json_of(
                env.agent(sess, dir)
                    .args(["acquire", "board", "--for", "10m"]),
            ))
        }

        /// Agent `session` hosted by a process of its own, so another agent
        /// session may run from this test process too.
        fn other(env: &Env, session: &str, dir: &Path, md: bool) -> Command {
            let mut cmd = if md {
                env.agent_md(session, dir)
            } else {
                env.agent(session, dir)
            };
            cmd.env(CLAUDE_PID, env.own_host(session, dir).to_string());
            cmd
        }

        mod acquire_override_on_a_unit_held_by_another_session {
            use super::*;

            fn overridden(env: &Env, md: bool) -> (String, std::process::Output) {
                seed(env, "board", "k1");
                let (a, b) = (env.dir("alpha"), env.dir("beta"));
                let theirs = agent_holds(env, "sess-1", &a);
                let args = ["acquire", "board", "--for", "10m", "--override", "asked"];
                let out = other(env, "sess-2", &b, md).args(args).output().unwrap();
                assert_eq!(out.status.code(), Some(6), "{out:?}");
                (theirs, out)
            }

            #[test]
            fn the_heading_says_the_unit_was_taken_from_that_session() {
                let env = Env::new();
                let (_, out) = overridden(&env, true);
                let md = String::from_utf8_lossy(&out.stdout).into_owned();
                let heading = md.lines().next().unwrap();
                assert!(heading.starts_with("## Queued: board"), "{md}");
                assert!(heading.contains("taken from"), "{md}");
                assert!(heading.contains("sess-1"), "{md}");
            }

            #[test]
            fn the_summary_names_the_displaced_lease() {
                let env = Env::new();
                let (theirs, out) = overridden(&env, false);
                let body: Value = serde_json::from_slice(&out.stdout).unwrap();
                let summary = body["summary"].to_string();
                assert!(summary.contains(&theirs), "{body}");
                assert!(summary.contains("overridden"), "{body}");
            }

            #[test]
            fn the_answer_names_the_displaced_holder_and_lease() {
                let env = Env::new();
                let (theirs, out) = overridden(&env, false);
                let body: Value = serde_json::from_slice(&out.stdout).unwrap();
                assert_eq!(body["displaced"]["lease_id"], theirs, "{body}");
                assert_eq!(
                    body["displaced"]["holder"],
                    json!({"kind": "agent_session", "session_id": "sess-1"}),
                    "{body}"
                );
            }

            #[test]
            fn without_an_override_nothing_is_displaced() {
                let env = Env::new();
                seed(&env, "board", "k1");
                let (a, b) = (env.dir("alpha"), env.dir("beta"));
                agent_holds(&env, "sess-1", &a);
                let out = other(&env, "sess-2", &b, false)
                    .args(["acquire", "board", "--for", "10m"])
                    .output()
                    .unwrap();
                let body: Value = serde_json::from_slice(&out.stdout).unwrap();
                assert!(body.get("displaced").is_none(), "{body}");
            }
        }

        mod gate_messages {
            use super::*;

            fn next_steps(message: &str) -> usize {
                message.matches("Ask the user").count()
            }

            #[test]
            fn log_without_override_has_exactly_one_next_step() {
                let env = Env::new();
                let dir = env.dir("alpha");
                let error = error_of(env.agent("sess-1", &dir).arg("log"), 4);
                assert_eq!(error["code"], "override_required", "{error}");
                let message = error["message"].as_str().unwrap();
                assert_eq!(next_steps(message), 1, "{message}");
            }

            #[test]
            fn daemon_stop_without_override_has_exactly_one_next_step() {
                let env = Env::new();
                let dir = env.dir("alpha");
                let error = error_of(env.agent("sess-1", &dir).args(["daemon", "stop"]), 4);
                assert_eq!(error["code"], "override_required", "{error}");
                let message = error["message"].as_str().unwrap();
                assert_eq!(next_steps(message), 1, "{message}");
                assert!(message.contains("--override"), "{message}");
            }

            /// The CLI ends a message with the next step; the daemon's must
            /// not carry one of its own.
            #[test]
            fn the_daemons_own_refusal_carries_no_next_step() {
                let env = Env::new();
                let dir = env.dir("alpha");
                seed(&env, "board", "k1");
                let caller = agent_caller("sess-1", &dir);
                let e = env
                    .rpc(
                        "admin_clean",
                        &caller,
                        json!({"resource": "board", "unit": "u1"}),
                    )
                    .unwrap_err();
                assert_eq!(e.code, "override_required");
                assert_eq!(next_steps(&e.message), 0, "{}", e.message);
            }
        }

        mod override_help {
            use super::*;

            #[test]
            fn lists_every_gated_command() {
                let env = Env::new();
                let out = env.cmd().arg("--help").output().unwrap();
                let help = String::from_utf8_lossy(&out.stdout)
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                let at = help.find("--override").expect("the flag is documented");
                let flag = &help[at..];
                for gated in [
                    "approve",
                    "admin",
                    "log",
                    "release --force",
                    "acquire",
                    "daemon stop",
                ] {
                    assert!(flag.contains(gated), "{gated} missing in {flag}");
                }
            }

            #[test]
            fn daemon_stop_help_says_agents_need_it() {
                let env = Env::new();
                let out = env.cmd().args(["daemon", "--help"]).output().unwrap();
                let help = String::from_utf8_lossy(&out.stdout)
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                assert!(help.contains("--override"), "{help}");
            }
        }

        mod daemon_stop_by_an_agent {
            use super::*;

            #[test]
            fn without_override_is_refused_and_the_daemon_stays_up() {
                let env = Env::new();
                let dir = env.dir("alpha");
                env.ok(&["status"]);
                error_of(env.agent("sess-1", &dir).args(["daemon", "stop"]), 4);
                assert_eq!(env.ok(&["daemon", "status"])["running"], true);
            }

            #[test]
            fn with_override_stops_it() {
                let env = Env::new();
                let dir = env.dir("alpha");
                env.ok(&["status"]);
                env.agent("sess-1", &dir)
                    .args(["daemon", "stop", "--override", "the user asked"])
                    .assert()
                    .success();
                assert_eq!(env.ok(&["daemon", "status"])["running"], false);
            }

            #[test]
            fn restart_needs_no_override() {
                let env = Env::new();
                let dir = env.dir("alpha");
                env.ok(&["status"]);
                env.agent("sess-1", &dir)
                    .args(["daemon", "restart"])
                    .assert()
                    .success();
                assert_eq!(env.ok(&["daemon", "status"])["running"], true);
            }
        }

        mod release_of_another_holders_lease {
            use super::*;

            #[test]
            fn by_an_agent_exits_4_with_not_yours_naming_the_kind() {
                let env = Env::new();
                seed(&env, "board", "k1");
                let (a, b) = (env.dir("alpha"), env.dir("beta"));
                let theirs = agent_holds(&env, "sess-1", &a);
                let error = error_of(
                    other(&env, "sess-2", &b, false).args(["release", &theirs]),
                    4,
                );
                assert_eq!(error["code"], "not_yours", "{error}");
                assert_eq!(
                    error["details"],
                    json!({"holder_kind": "agent_session"}),
                    "{error}"
                );
                assert_eq!(unit_status(&env)["occupancy"]["state"], "held");
            }

            #[test]
            fn by_an_agent_with_cleanup_exits_4_too() {
                let env = Env::new();
                seed(&env, "board", "k1");
                let (a, b) = (env.dir("alpha"), env.dir("beta"));
                let theirs = agent_holds(&env, "sess-1", &a);
                let error = error_of(
                    other(&env, "sess-2", &b, false).args(["release", &theirs, "--cleanup"]),
                    4,
                );
                assert_eq!(error["code"], "not_yours", "{error}");
            }

            #[test]
            fn by_an_agent_on_an_unknown_lease_is_not_found() {
                let env = Env::new();
                seed(&env, "board", "k1");
                let b = env.dir("beta");
                let ghost = leclerk::model::LeaseId::new();
                let error = error_of(
                    other(&env, "sess-2", &b, false).args(["release", ghost.as_str()]),
                    3,
                );
                assert_eq!(error["code"], "not_found", "{error}");
            }

            #[test]
            fn by_the_user_keeps_its_informative_text() {
                let env = Env::new();
                seed(&env, "board", "k1");
                let a = env.dir("alpha");
                let theirs = agent_holds(&env, "sess-1", &a);
                let error = error_of(env.cmd().args(["release", &theirs]), 3);
                let message = error["message"].as_str().unwrap();
                assert!(message.contains("belongs to an agent session"), "{message}");
                assert!(message.contains("clerk leases"), "{message}");
            }

            #[test]
            fn with_force_and_override_an_agent_ends_it() {
                let env = Env::new();
                seed(&env, "board", "k1");
                let (a, b) = (env.dir("alpha"), env.dir("beta"));
                let theirs = agent_holds(&env, "sess-1", &a);
                other(&env, "sess-2", &b, false)
                    .args([
                        "release",
                        "--force",
                        "--yes",
                        &theirs,
                        "--override",
                        "asked",
                    ])
                    .assert()
                    .success();
                assert_eq!(unit_status(&env)["occupancy"]["kind"], "free");
            }
        }

        mod launcher_spawn {
            use super::*;

            fn environ(pid: u64) -> Vec<String> {
                fs::read(format!("/proc/{pid}/environ"))
                    .unwrap()
                    .split(|b| *b == 0)
                    .filter(|e| !e.is_empty())
                    .map(|e| String::from_utf8_lossy(e).into_owned())
                    .collect()
            }

            #[test]
            fn the_daemon_environment_has_no_claude_variables() {
                let env = Env::new();
                let dir = env.dir("alpha");
                env.agent("sess-1", &dir)
                    .env("CLAUDE_LEAK_TEST", "1")
                    .env("UNRELATED_SECRET", "x")
                    .arg("status")
                    .assert()
                    .success();
                let pid = env.ok(&["daemon", "status"])["pid"].as_u64().unwrap();
                let vars = environ(pid);
                assert!(vars.iter().all(|v| !v.starts_with("CLAUDE_")), "{vars:?}");
                assert!(
                    vars.iter().all(|v| !v.starts_with("UNRELATED_SECRET")),
                    "{vars:?}"
                );
            }

            #[test]
            fn the_daemon_keeps_home_path_and_clerk_home() {
                let env = Env::new();
                env.ok(&["status"]);
                let pid = env.ok(&["daemon", "status"])["pid"].as_u64().unwrap();
                let vars = environ(pid);
                for kept in ["CLERK_HOME=", "PATH="] {
                    assert!(vars.iter().any(|v| v.starts_with(kept)), "{kept}: {vars:?}");
                }
            }
        }
    }

    mod socket_path_too_long {
        use super::*;

        /// A `CLERK_HOME` whose socket path is over the Unix limit.
        fn long_home(env: &Env) -> PathBuf {
            let dir = env.home().join("a".repeat(120));
            fs::create_dir_all(&dir).unwrap();
            dir
        }

        mod on_a_client_command {
            use super::*;

            #[test]
            fn exits_5() {
                let env = Env::new();
                let home = long_home(&env);
                let error = error_of(env.cmd().env("CLERK_HOME", &home).arg("status"), 5);
                assert_eq!(error["code"], "socket_path_too_long", "{error}");
            }

            #[test]
            fn names_the_length_and_the_fix() {
                let env = Env::new();
                let home = long_home(&env);
                let error = error_of(env.cmd().env("CLERK_HOME", &home).arg("status"), 5);
                let message = error["message"].as_str().unwrap();
                assert!(message.contains("bytes long"), "{message}");
                assert!(
                    message.contains("CLERK_HOME or XDG_RUNTIME_DIR"),
                    "{message}"
                );
            }
        }

        mod on_the_daemon {
            use super::*;

            #[test]
            fn exits_nonzero() {
                let env = Env::new();
                let home = long_home(&env);
                let out = env
                    .cmd()
                    .env("CLERK_HOME", &home)
                    .arg("daemon")
                    .output()
                    .unwrap();
                assert!(!out.status.success(), "{out:?}");
            }

            #[test]
            fn names_the_fix() {
                let env = Env::new();
                let home = long_home(&env);
                let out = env
                    .cmd()
                    .env("CLERK_HOME", &home)
                    .arg("daemon")
                    .output()
                    .unwrap();
                let text = String::from_utf8_lossy(&out.stdout);
                assert!(text.contains("CLERK_HOME or XDG_RUNTIME_DIR"), "{out:?}");
            }
        }
    }

    /// Human output on a terminal, against `docs/cli-output.md`.
    mod human_output {
        use super::*;

        use crossterm::style::Stylize;

        /// A 100-column UTF-8 terminal without colour.
        const PLAIN: &str = "stty cols 100 rows 40; export NO_COLOR=1 LC_ALL=C.UTF-8;";
        /// A 100-column UTF-8 terminal with colour.
        const COLOURED: &str = "stty cols 100 rows 40; unset NO_COLOR; export LC_ALL=C.UTF-8;";
        /// A long device path, longer than a narrow terminal.
        const LONG_PATH: &str =
            "/dev/serial/by-id/usb-Espressif_USB_JTAG_serial_debug_unit_F0:F5:BD:01:02:03-if00";

        /// `clerk <args>` on a plain terminal (stdin a terminal too).
        fn human(env: &Env, args: &str) -> String {
            env.tty(&format!("{PLAIN} \"$CLERK\" {args}"))
        }

        /// `clerk <args>` on a plain terminal, stdin not a terminal.
        fn human_detached(env: &Env, args: &str) -> String {
            env.tty(&format!("{PLAIN} \"$CLERK\" {args} < /dev/null"))
        }

        /// A local TCP port nothing listens on.
        fn closed_port() -> u16 {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        }

        /// esp32-c6 (generic: b1 described, b2 not; tags; usage) and otbr (a
        /// service on a closed port).
        fn bench(env: &Env) {
            env.ok(&[
                "resource",
                "create",
                "esp32-c6",
                "--type",
                "generic",
                "--description",
                "ESP32-C6 devkits on the bench hub",
                "--tag",
                "devboard,esp32",
                "--usage",
                "Flash with key {unit.identity_key}",
            ]);
            env.ok(&[
                "unit",
                "add",
                "esp32-c6",
                "b1",
                "--key",
                "k1",
                "--description",
                "hub port 1",
            ]);
            env.ok(&["unit", "add", "esp32-c6", "b2", "--key", "k2"]);
            env.ok(&[
                "resource",
                "create",
                "otbr",
                "--type",
                "service",
                "--description",
                "Thread border router REST API",
                "--tag",
                "otbr,thread",
            ]);
            let port = closed_port().to_string();
            env.ok(&[
                "unit",
                "add",
                "otbr",
                "main",
                "--host",
                "127.0.0.1",
                "--port",
                &port,
            ]);
        }

        /// A detached user lease on `target` for `purpose`; its id.
        fn hold_for(env: &Env, target: &str, duration: &str, purpose: &str) -> String {
            env.ok(&["acquire", target, "--for", duration, "--purpose", purpose])["lease_id"]
                .as_str()
                .unwrap()
                .to_owned()
        }

        /// A detached user request on `target` that queues (exit 6); its
        /// JSON answer.
        fn queue_on(env: &Env, target: &str) -> Value {
            let out = env
                .cmd()
                .args(["acquire", target, "--for", "10m"])
                .output()
                .unwrap();
            assert_eq!(out.status.code(), Some(6), "{out:?}");
            serde_json::from_slice(&out.stdout).unwrap()
        }

        /// The probe: a device resource whose unit has a long path.
        fn probe(env: &Env) {
            env.ok(&["resource", "create", "probe", "--type", "device"]);
            env.ok(&["unit", "add", "probe", "p1", "--path", LONG_PATH]);
        }

        /// The prefix of the escape sequence crossterm writes for `styled`
        /// (a style applied to "X").
        fn prefix(styled: &str) -> String {
            styled.split('X').next().unwrap().to_owned()
        }

        /// Every `ESC [ ... m` sequence in `text`.
        fn escapes(text: &str) -> Vec<String> {
            let mut out = Vec::new();
            let mut rest = text;
            while let Some(start) = rest.find('\x1b') {
                let tail = &rest[start..];
                let end = tail.find('m').map_or(tail.len(), |e| e + 1);
                out.push(tail[..end].to_owned());
                rest = &tail[end..];
            }
            out
        }

        mod any_list_or_detail_view {
            use super::*;

            #[test]
            fn never_shortens_lease_ids() {
                let env = Env::new();
                bench(&env);
                let lease = hold_for(&env, "esp32-c6/b1", "30m", "third");
                let shown = env.tty(
                    "stty cols 40 rows 20; export NO_COLOR=1 LC_ALL=C.UTF-8; \
                     \"$CLERK\" leases; \"$CLERK\" status",
                );
                assert_eq!(shown.matches(&lease).count(), 2, "{shown}");
            }

            #[test]
            fn never_shortens_settings() {
                let env = Env::new();
                probe(&env);
                let shown = env.tty(
                    "stty cols 40 rows 20; export NO_COLOR=1 LC_ALL=C.UTF-8; \
                     \"$CLERK\" resource show probe",
                );
                assert!(shown.contains(LONG_PATH), "{shown}");
            }

            #[test]
            fn wraps_free_text_instead_of_truncating() {
                let env = Env::new();
                let words: Vec<String> = (0..30).map(|i| format!("word{i}")).collect();
                env.ok(&[
                    "resource",
                    "create",
                    "board",
                    "--type",
                    "generic",
                    "--description",
                    &words.join(" "),
                ]);
                let shown = env.tty(
                    "stty cols 60 rows 20; export NO_COLOR=1 LC_ALL=C.UTF-8; \"$CLERK\" status",
                );
                for word in &words {
                    assert!(
                        shown.contains(&format!("{word} ")) || shown.contains(&format!("{word}\n")),
                        "{word}: {shown}"
                    );
                }
                for line in shown.lines() {
                    assert!(line.chars().count() <= 60, "{line:?} in {shown}");
                }
            }

            #[test]
            fn shows_projects_as_the_repo_folder_with_tilde_for_home() {
                let env = Env::new();
                let dir = env.dir("zigbee-fw");
                let git = std::process::Command::new("git")
                    .args(["init", "-q"])
                    .current_dir(&dir)
                    .status()
                    .unwrap();
                assert!(git.success());
                json_of(env.cmd().current_dir(&dir).args([
                    "resource", "create", "nrf52", "--type", "generic", "--scope", "project",
                ]));
                let home = env.home().canonicalize().unwrap();
                let shown = env.tty(&format!(
                    "{PLAIN} export HOME='{}'; \"$CLERK\" resources",
                    home.display()
                ));
                assert!(
                    shown.contains("• nrf52 0/0 free · generic · project ~/projects/zigbee-fw\n"),
                    "{shown}"
                );
            }

            #[test]
            fn never_shows_the_git_directory() {
                let env = Env::new();
                let dir = env.dir("zigbee-fw");
                std::process::Command::new("git")
                    .args(["init", "-q"])
                    .current_dir(&dir)
                    .status()
                    .unwrap();
                json_of(env.cmd().current_dir(&dir).args([
                    "resource", "create", "nrf52", "--type", "generic", "--scope", "project",
                ]));
                let shown = human(&env, "resources; \"$CLERK\" status");
                assert!(!shown.contains(".git"), "{shown}");
            }

            #[test]
            fn shows_a_user_lease_holder_as_cli() {
                let env = Env::new();
                bench(&env);
                let lease = hold_for(&env, "esp32-c6/b1", "30m", "third");
                let shown = human(&env, "leases");
                let line = shown.lines().find(|l| l.contains(&lease)).unwrap();
                assert!(line.ends_with(" · cli"), "{shown}");
            }

            #[test]
            fn shows_an_agent_holder_by_its_first_8_characters() {
                let env = Env::new();
                bench(&env);
                let dir = env.dir("alpha");
                acquire(&env, &agent_caller("3f2a9c1e-77aa-4c1b", &dir), "esp32-c6");
                let shown = human(&env, "leases");
                assert!(shown.contains(" · agent 3f2a9c1e\n"), "{shown}");
            }

            #[test]
            fn omits_the_unit_line_without_a_unit_description() {
                let env = Env::new();
                bench(&env);
                let shown = human_detached(&env, "acquire esp32-c6/b2 --for 10m");
                assert!(shown.contains("Granted: esp32-c6/b2 until "), "{shown}");
                assert!(!shown.contains("b2:"), "{shown}");
                assert!(!shown.contains("unit:"), "{shown}");
            }

            #[test]
            fn omits_the_tags_line_without_tags() {
                let env = Env::new();
                probe(&env);
                let shown = human(&env, "resource show probe");
                assert!(!shown.contains("tags"), "{shown}");
            }
        }

        mod style {
            use super::*;

            mod on_a_tty_without_no_color {
                use super::*;

                fn coloured_status(env: &Env) -> String {
                    bench(env);
                    queue_on(env, "otbr");
                    env.tty(&format!("{COLOURED} \"$CLERK\" status"))
                }

                #[test]
                fn tags_are_bold_cyan() {
                    let env = Env::new();
                    let shown = coloured_status(&env);
                    let style = prefix(&format!("{}", "X".cyan().bold()));
                    assert!(shown.contains(&format!("{style}esp32")), "{shown:?}");
                }

                #[test]
                fn resource_ids_are_bold() {
                    let env = Env::new();
                    let shown = coloured_status(&env);
                    let style = prefix(&format!("{}", "X".bold()));
                    assert!(shown.contains(&format!("{style}esp32-c6")), "{shown:?}");
                }

                #[test]
                fn warning_lines_are_yellow() {
                    let env = Env::new();
                    let shown = coloured_status(&env);
                    let style = prefix(&format!("{}", "X".yellow()));
                    assert!(shown.contains(&format!("{style}⚠ unhealthy")), "{shown:?}");
                }

                #[test]
                fn nothing_else_is_coloured() {
                    let env = Env::new();
                    let shown = coloured_status(&env);
                    let allowed = [
                        prefix(&format!("{}", "X".cyan().bold())),
                        prefix(&format!("{}", "X".bold())),
                        prefix(&format!("{}", "X".yellow())),
                    ];
                    let resets: Vec<String> = [
                        format!("{}", "X".cyan().bold()),
                        format!("{}", "X".bold()),
                        format!("{}", "X".yellow()),
                    ]
                    .iter()
                    .map(|s| s.split('X').nth(1).unwrap().to_owned())
                    .collect();
                    let known: Vec<String> = escapes(&allowed.concat())
                        .into_iter()
                        .chain(escapes(&resets.concat()))
                        .collect();
                    for e in escapes(&shown) {
                        assert!(known.contains(&e), "{e:?} in {shown:?}");
                    }
                }
            }

            mod on_no_color {
                use super::*;

                #[test]
                fn prints_no_escape_sequences() {
                    let env = Env::new();
                    bench(&env);
                    queue_on(&env, "otbr");
                    let shown = human(&env, "status");
                    assert!(!shown.contains('\x1b'), "{shown:?}");
                }
            }

            mod on_ascii_flag {
                use super::*;

                #[test]
                fn prints_only_ascii() {
                    let env = Env::new();
                    bench(&env);
                    hold_for(&env, "esp32-c6/b1", "30m", "third");
                    queue_on(&env, "otbr");
                    let shown = human(&env, "--ascii status");
                    assert!(shown.contains("* esp32-c6 #. 1/2 free"), "{shown}");
                    assert!(shown.is_ascii(), "{shown}");
                }
            }

            mod on_a_non_utf8_locale {
                use super::*;

                #[test]
                fn prints_only_ascii() {
                    let env = Env::new();
                    bench(&env);
                    queue_on(&env, "otbr");
                    let shown = env.tty(
                        "stty cols 100 rows 40; export NO_COLOR=1 LC_ALL=C; \"$CLERK\" status",
                    );
                    assert!(shown.contains("  - main . ! unhealthy"), "{shown}");
                    assert!(shown.is_ascii(), "{shown}");
                }
            }
        }

        mod resources {
            use super::*;

            #[test]
            fn shows_one_block_per_resource_with_its_occupancy_bar() {
                let env = Env::new();
                bench(&env);
                hold_for(&env, "esp32-c6/b1", "30m", "third");
                queue_on(&env, "otbr");
                let shown = human(&env, "resources");
                assert!(
                    shown.contains(
                        "• esp32-c6 ▮▯ 1/2 free · generic · user\n  \
                         ESP32-C6 devkits on the bench hub\n  tags: devboard esp32\n\n• otbr"
                    ),
                    "{shown}"
                );
                assert!(
                    shown.contains(
                        "• otbr ▯ 0/1 free ⚠ · service · user\n  \
                         Thread border router REST API\n  tags: otbr thread\n"
                    ),
                    "{shown}"
                );
            }
        }

        mod status {
            use super::*;

            fn shown(env: &Env) -> (String, String, String) {
                bench(env);
                let lease = hold_for(env, "esp32-c6/b1", "30m", "third");
                let request = queue_on(env, "otbr")["request_id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                (human(env, "status"), lease, request)
            }

            #[test]
            fn shows_the_header_with_the_bar_and_tags() {
                let env = Env::new();
                let (shown, ..) = shown(&env);
                assert!(
                    shown.contains(
                        "• esp32-c6 ▮▯ 1/2 free · generic · user · devboard esp32\n  \
                         ESP32-C6 devkits on the bench hub\n"
                    ),
                    "{shown}"
                );
            }

            #[test]
            fn shows_each_unit_with_its_holder_and_lease() {
                let env = Env::new();
                let (shown, lease, _) = shown(&env);
                let line = shown
                    .lines()
                    .find(|l| l.starts_with("  ◦ b1 ▯ held by cli · ends in "))
                    .unwrap_or_else(|| panic!("{shown}"));
                assert!(line.ends_with(&format!(" · {lease}")), "{shown}");
                assert!(
                    shown.contains("\n      hub port 1 · \"third\"\n"),
                    "{shown}"
                );
                assert!(shown.contains("\n  ◦ b2 ▮ free\n"), "{shown}");
            }

            #[test]
            fn shows_an_unhealthy_unit_with_its_check_message() {
                let env = Env::new();
                let (shown, ..) = shown(&env);
                assert!(
                    shown.contains("\n  ◦ main ▯ ⚠ unhealthy: connect failed (checked "),
                    "{shown}"
                );
                assert!(
                    shown.contains("• otbr ▯ 0/1 free ⚠ · service · user"),
                    "{shown}"
                );
            }

            #[test]
            fn shows_the_queue_under_its_resource() {
                let env = Env::new();
                let (shown, _, request) = shown(&env);
                assert!(
                    shown.contains(&format!(
                        "\n  queue:\n  ◦ 1. cli · normal · \"held from the command line\" · \
                         {request}\n"
                    )),
                    "{shown}"
                );
            }
        }

        mod leases {
            use super::*;

            #[test]
            fn shows_one_block_per_lease_with_its_purpose() {
                let env = Env::new();
                bench(&env);
                let lease = hold_for(&env, "esp32-c6/b1", "30m", "third");
                let shown = human(&env, "leases");
                let header = format!("• {lease}  esp32-c6/b1 · held · ends in ");
                assert!(shown.contains(&header), "{shown}");
                assert!(shown.contains(" · cli\n  \"third\""), "{shown}");
            }
        }

        mod queue {
            use super::*;

            #[test]
            fn shows_one_block_per_request() {
                let env = Env::new();
                bench(&env);
                let request = queue_on(&env, "otbr")["request_id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                let shown = human(&env, "queue otbr");
                assert!(
                    shown.contains(&format!(
                        "• 1. {request} · cli · normal\n  \"held from the command line\""
                    )),
                    "{shown}"
                );
            }
        }

        mod resource_show {
            use super::*;

            #[test]
            fn matches_the_mock_up() {
                let env = Env::new();
                bench(&env);
                let shown = human(&env, "resource show esp32-c6");
                let expected = "esp32-c6  generic · user\n  \
                                ESP32-C6 devkits on the bench hub\n  \
                                tags:   devboard esp32\n  \
                                health: type default (none)\n  \
                                units:\n  \
                                ◦ b1  key k1\n        \
                                hub port 1\n  \
                                ◦ b2  key k2\n  \
                                usage (template):\n    \
                                Flash with key {unit.identity_key}\n";
                assert!(shown.contains(expected), "{shown}");
            }
        }

        mod grant {
            use super::*;

            #[test]
            fn shows_the_brief_with_usage_indented_under_its_label() {
                let env = Env::new();
                bench(&env);
                let shown = human_detached(&env, "acquire esp32-c6/b1 --for 2h");
                let header = shown
                    .lines()
                    .find(|l| l.starts_with("Granted: esp32-c6/b1 until "))
                    .unwrap_or_else(|| panic!("{shown}"));
                assert!(header.contains(" (in 2h) · lse_"), "{shown}");
                assert!(
                    shown.contains(
                        "\n  ESP32-C6 devkits on the bench hub\n  b1: hub port 1\n  \
                         tags: devboard esp32\n  key: k1\n  usage:\n    Flash with key k1\n"
                    ),
                    "{shown}"
                );
            }
        }

        mod queued_acquire {
            use super::*;

            mod on_every_free_unit_unhealthy {
                use super::*;

                #[test]
                fn names_the_units_and_their_check_message_and_the_recheck() {
                    let env = Env::new();
                    bench(&env);
                    let shown = human_detached(&env, "acquire otbr --for 10m");
                    assert!(
                        shown.contains("Queued: position 1 on otbr · req_"),
                        "{shown}"
                    );
                    assert!(
                        shown.contains(
                            "\n  main is free but ⚠ unhealthy (connect failed); it is \
                             re-checked every 30s.\n"
                        ),
                        "{shown}"
                    );
                }

                #[test]
                fn json_carries_blocked_by() {
                    let env = Env::new();
                    bench(&env);
                    let body = queue_on(&env, "otbr");
                    let blocked = &body["blocked_by"];
                    assert_eq!(blocked["kind"], "unhealthy", "{body}");
                    assert_eq!(blocked["units"][0]["unit"], "main", "{body}");
                    assert_eq!(blocked["units"][0]["message"], "connect failed", "{body}");
                }
            }

            mod on_units_held {
                use super::*;

                #[test]
                fn names_the_holder_and_purpose_of_the_one_ending_first() {
                    let env = Env::new();
                    bench(&env);
                    hold_for(&env, "esp32-c6/b1", "30m", "third");
                    hold_for(&env, "esp32-c6/b2", "10m", "fourth");
                    let shown = human_detached(&env, "acquire esp32-c6 --for 10m");
                    assert!(
                        shown.contains("\n  b2 is held by cli for \"fourth\", ends in "),
                        "{shown}"
                    );
                    assert!(!shown.contains("third"), "{shown}");
                }

                #[test]
                fn json_carries_blocked_by() {
                    let env = Env::new();
                    bench(&env);
                    hold_for(&env, "esp32-c6/b1", "30m", "third");
                    hold_for(&env, "esp32-c6/b2", "10m", "fourth");
                    let body = queue_on(&env, "esp32-c6");
                    let blocked = &body["blocked_by"];
                    assert_eq!(blocked["kind"], "held", "{body}");
                    assert_eq!(blocked["unit"], "b2", "{body}");
                    assert_eq!(blocked["purpose"], "fourth", "{body}");
                }
            }
        }

        mod log {
            use super::*;

            #[test]
            fn shortens_ids_in_the_action_column() {
                let env = Env::new();
                bench(&env);
                let request = queue_on(&env, "otbr")["request_id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                let shown = human(&env, "log");
                let line = shown
                    .lines()
                    .find(|l| l.contains(" queued "))
                    .unwrap_or_else(|| panic!("{shown}"));
                assert!(line.starts_with("• "), "{shown}");
                assert!(line.contains("otbr by cli"), "{shown}");
                assert!(line.contains(&format!("{}…", &request[..12])), "{shown}");
                assert!(!shown.contains(&request), "{shown}");
            }

            #[test]
            fn wide_shows_ids_whole() {
                let env = Env::new();
                bench(&env);
                let request = queue_on(&env, "otbr")["request_id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                let shown = human(&env, "log --wide");
                assert!(shown.contains(&request), "{shown}");
            }
        }

        mod promotions {
            use super::*;

            #[test]
            fn shows_one_block_per_promotion() {
                let env = Env::new();
                let id = promotion(&env);
                let home = env.home().canonicalize().unwrap();
                let shown = env.tty(&format!(
                    "{PLAIN} export HOME='{}'; \"$CLERK\" promotions",
                    home.display()
                ));
                assert!(
                    shown.contains(&format!("• {id}  a-board · project ~/projects/alpha\n")),
                    "{shown}"
                );
                assert!(
                    shown.contains(&format!("Approve with `clerk approve {id}`.")),
                    "{shown}"
                );
            }
        }

        /// Review fixes: linked blockers, stderr style, wrapping, reasons.
        mod review_fixes {
            use super::*;

            /// r1/u1 and r2/u2 linked by the admin; r2 held for "other";
            /// a user request on r1 queued. Its JSON answer.
            fn linked_visible(env: &Env) -> Value {
                for (r, u, k) in [("r1", "u1", "k1"), ("r2", "u2", "k2")] {
                    env.ok(&["resource", "create", r, "--type", "generic"]);
                    env.ok(&["unit", "add", r, u, "--key", k]);
                }
                env.rpc(
                    "admin_link",
                    &user("me"),
                    json!({"a": {"resource": "r1", "unit": "u1"},
                           "b": {"resource": "r2", "unit": "u2"}}),
                )
                .unwrap();
                hold_for(env, "r2", "10m", "other");
                queue_on(env, "r1")
            }

            /// `shared/u1` (user) linked with project alpha's `a-board/u`,
            /// which an alpha agent holds; returns the beta directory.
            fn linked_elsewhere(env: &Env) -> PathBuf {
                let (a, b) = (env.dir("alpha"), env.dir("beta"));
                seed(env, "shared", "k1");
                json_of(env.agent("sess-a", &a).args([
                    "resource", "create", "a-board", "--type", "generic", "--scope", "project",
                ]));
                json_of(
                    env.agent("sess-a", &a)
                        .args(["unit", "add", "a-board", "u", "--key", "k1"]),
                );
                acquire(env, &agent_caller("sess-a", &a), "a-board");
                b
            }

            /// A beta agent's queued request on `shared`; its JSON answer.
            fn queue_from_beta(env: &Env, beta: &Path) -> Value {
                acquire(env, &agent_caller("sess-b", beta), "shared")
            }

            mod queued_acquire_on_a_busy_linked_unit {
                use super::*;

                #[test]
                fn json_names_the_linked_resource_and_unit() {
                    let env = Env::new();
                    let body = linked_visible(&env);
                    let b = &body["blocked_by"];
                    assert_eq!(b["kind"], "linked", "{body}");
                    assert_eq!(b["held_resource"], "r2", "{body}");
                    assert_eq!(b["held_unit"], "u2", "{body}");
                }

                #[test]
                fn json_names_the_holder_and_purpose() {
                    let env = Env::new();
                    let body = linked_visible(&env);
                    let b = &body["blocked_by"];
                    assert_eq!(b["holder"]["kind"], "user", "{body}");
                    assert_eq!(b["purpose"], "other", "{body}");
                }

                #[test]
                fn human_names_where_it_is_held() {
                    let env = Env::new();
                    linked_visible(&env);
                    let shown = human_detached(&env, "acquire r1 --for 10m");
                    assert!(
                        shown.contains(
                            "\n  u1 is busy: the same device is held as r2/u2 by cli for \
                             \"other\", ends in "
                        ),
                        "{shown}"
                    );
                }
            }

            mod queued_acquire_on_a_linked_unit_busy_in_another_project {
                use super::*;

                #[test]
                fn json_hides_the_resource_unit_holder_and_purpose() {
                    let env = Env::new();
                    let beta = linked_elsewhere(&env);
                    let body = queue_from_beta(&env, &beta);
                    let b = &body["blocked_by"];
                    assert_eq!(b["kind"], "linked", "{body}");
                    for key in ["held_resource", "held_unit", "holder", "purpose"] {
                        assert!(b[key].is_null(), "{key}: {body}");
                    }
                }

                #[test]
                fn json_keeps_the_unit_and_end() {
                    let env = Env::new();
                    let beta = linked_elsewhere(&env);
                    let body = queue_from_beta(&env, &beta);
                    let b = &body["blocked_by"];
                    assert_eq!(b["unit"], "u1", "{body}");
                    assert!(b["ends_at"].is_i64(), "{body}");
                }

                #[test]
                fn human_says_it_is_held_in_another_project() {
                    let env = Env::new();
                    let beta = linked_elsewhere(&env);
                    let shown = env.tty(&format!(
                        "{PLAIN} cd '{}' && {SESSION_VAR}=sess-b {CLAUDE_PID}={} \"$CLERK\" \
                         acquire shared --for 1m < /dev/null",
                        beta.display(),
                        env.own_host("sess-b", &beta)
                    ));
                    assert!(
                        shown.contains(
                            "\n  u1 is busy: the same device is held in another project, ends in "
                        ),
                        "{shown}"
                    );
                }
            }

            mod not_found_for_another_projects_unit {
                use super::*;

                #[test]
                fn json_details_name_the_resource() {
                    let env = Env::new();
                    let beta = linked_elsewhere(&env);
                    let error = error_of(
                        env.agent("sess-b", &beta)
                            .env(CLAUDE_PID, env.own_host("sess-b", &beta).to_string())
                            .args(["acquire", "a-board/u", "--for", "1m"]),
                        3,
                    );
                    assert_eq!(
                        error["details"],
                        json!({"kind": "resource", "id": "a-board"})
                    );
                }
            }

            mod queue_on_a_missing_resource {
                use super::*;

                #[test]
                fn json_details_name_it() {
                    let env = Env::new();
                    bench(&env);
                    let error = error_of(env.cmd().args(["queue", "nope"]), 3);
                    assert_eq!(error["details"], json!({"kind": "resource", "id": "nope"}));
                }

                #[test]
                fn human_names_it() {
                    let env = Env::new();
                    bench(&env);
                    let shown = human(&env, "queue nope");
                    assert!(shown.contains("not found: resource \"nope\""), "{shown}");
                }
            }

            mod stderr_style {
                use super::*;

                #[test]
                fn colours_stderr_on_a_tty_when_stdout_is_not() {
                    let env = Env::new();
                    bench(&env);
                    let shown = env.tty(&format!(
                        "{COLOURED} timeout 2 \"$CLERK\" acquire otbr --for 10m --wait \
                         < /dev/null > /dev/null"
                    ));
                    let style = prefix(&format!("{}", "X".yellow()));
                    assert!(shown.contains(&format!("{style}⚠ unhealthy")), "{shown:?}");
                }

                #[test]
                fn does_not_colour_stderr_that_is_not_a_tty() {
                    let env = Env::new();
                    bench(&env);
                    let err = env.home().join("err.txt");
                    let shown = env.tty(&format!(
                        "{COLOURED} timeout 2 \"$CLERK\" acquire otbr --for 10m --wait \
                         < /dev/null 2> '{0}'; cat '{0}'",
                        err.display()
                    ));
                    assert!(shown.contains("⚠ unhealthy"), "{shown:?}");
                    assert!(!shown.contains('\x1b'), "{shown:?}");
                }
            }

            mod wrapping {
                use super::*;

                #[test]
                fn keeps_ids_whole_on_a_narrow_coloured_terminal() {
                    let env = Env::new();
                    bench(&env);
                    let request = queue_on(&env, "otbr")["request_id"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    let shown = env.tty(
                        "stty cols 30 rows 40; unset NO_COLOR; export LC_ALL=C.UTF-8; \
                         \"$CLERK\" log --wide",
                    );
                    assert!(shown.contains(&request), "{shown:?}");
                }
            }

            mod promotion_reason {
                use super::*;

                #[test]
                fn json_does_not_name_the_requester() {
                    let env = Env::new();
                    promotion(&env);
                    let body = env.ok(&["promotions"]);
                    assert_eq!(body[0]["reason"], "a matching unit was registered there");
                }

                #[test]
                fn human_names_the_requester_once() {
                    let env = Env::new();
                    promotion(&env);
                    let home = env.home().canonicalize().unwrap();
                    let shown = env.tty(&format!(
                        "{PLAIN} export HOME='{}'; \"$CLERK\" promotions",
                        home.display()
                    ));
                    assert!(
                        shown.contains(
                            "\n  requested by ~/projects/beta: a matching unit was registered \
                             there\n"
                        ),
                        "{shown}"
                    );
                }
            }
        }

        mod errors {
            use super::*;

            mod on_not_found {
                use super::*;

                #[test]
                fn human_names_the_missing_kind_and_id() {
                    let env = Env::new();
                    bench(&env);
                    let shown = human(&env, "resource show nope");
                    assert!(shown.contains("not found: resource \"nope\""), "{shown}");
                }

                #[test]
                fn json_details_name_a_missing_resource() {
                    let env = Env::new();
                    bench(&env);
                    let error = error_of(env.cmd().args(["resource", "show", "nope"]), 3);
                    assert_eq!(error["details"], json!({"kind": "resource", "id": "nope"}));
                }

                #[test]
                fn json_details_name_a_missing_unit() {
                    let env = Env::new();
                    bench(&env);
                    let error =
                        error_of(env.cmd().args(["acquire", "esp32-c6/zz", "--for", "1m"]), 3);
                    assert_eq!(
                        error["details"],
                        json!({"kind": "unit", "id": "esp32-c6/zz"})
                    );
                }
            }

            mod on_a_clap_usage_error_in_json_mode {
                use super::*;

                #[test]
                fn message_carries_no_help_tail() {
                    let env = Env::new();
                    let error = error_of(env.cmd().args(["status", "--bogus"]), 2);
                    let message = error["message"].as_str().unwrap();
                    assert!(!message.contains("--help"), "{message}");
                    assert!(!message.contains("For more information"), "{message}");
                }
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
