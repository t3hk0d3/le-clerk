//! The Claude Code plugin under `integrations/claude-code` and the local
//! marketplace at the repo root: every JSON file and the skill checked
//! against the documented schemas (spec Appendix B), the version match with
//! the binary, the hook wiring, and the fail-open hook launcher and
//! `clerk hook` entry point. (Every `clerk ...` command in the skill parses
//! with clap: that check is a unit test in `src/cli/mod.rs`, where `Cli`
//! lives.)

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use leclerk::client::wire::NoticeKind;
use leclerk::model::ResourceType;
use leclerk::model::text::placeholder_keys;
use serde_json::Value;

/// The plugin root, relative to the repo root.
const PLUGIN_DIR: &str = "integrations/claude-code";
/// The marketplace entry's `source`.
const PLUGIN_SOURCE: &str = "./integrations/claude-code";
/// The one skill (one skill keeps agents' context small).
const SKILL: &str = "clerk";
/// The skill's files: the entry point and the references it links.
const SKILL_FILES: [&str; 3] = ["SKILL.md", "leasing.md", "registering.md"];
/// The longest `SKILL.md` may be, in lines.
const SKILL_MAX_LINES: usize = 120;
/// The removed MCP tools: no skill may name them.
const MCP_TOOLS: [&str; 6] = [
    "lease_wait",
    "lease_acquire",
    "lease_status",
    "resource_list",
    "type_list",
    "lease_confirm",
];
/// Each hook event with its `clerk hook` subcommand and the longest
/// `timeout` (seconds) it may set.
const HOOKS: [(&str, &str, f64); 4] = [
    ("SessionStart", "session-start", 5.0),
    ("Stop", "stop", 5.0),
    ("PreToolUse", "pre-tool-use", 5.0),
    // Claude Code gives SessionEnd hooks 1.5 s in total; a longer timeout
    // would raise that budget, which the hook must not rely on.
    ("SessionEnd", "session-end", 1.5),
];

/// Every [`NoticeKind`] the clerk delivers (not `Unknown`). The exhaustive
/// match makes a new kind fail to compile here until it is listed.
fn all_notice_kinds() -> Vec<NoticeKind> {
    use NoticeKind::*;
    let all = vec![
        Offered,
        QueueDropped,
        PositionChanged,
        ExpiryWarning,
        CleanupWindowStarted,
        CleanupEnding,
        YieldRequested,
        YieldWithdrawn,
        UnitReturned,
        BorrowerOverran,
        Overridden,
        LeaseEnded,
        ResourceRemoved,
    ];
    for kind in &all {
        match kind {
            Offered | QueueDropped | PositionChanged | ExpiryWarning | CleanupWindowStarted
            | CleanupEnding | YieldRequested | YieldWithdrawn | UnitReturned | BorrowerOverran
            | Overridden | LeaseEnded | ResourceRemoved | Unknown => {}
        }
    }
    all
}

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn plugin_path(rel: &str) -> PathBuf {
    repo().join(PLUGIN_DIR).join(rel)
}

fn read_json(path: &Path) -> Value {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

fn manifest() -> Value {
    read_json(&plugin_path(".claude-plugin/plugin.json"))
}

fn marketplace() -> Value {
    read_json(&repo().join(".claude-plugin/marketplace.json"))
}

fn hooks() -> Value {
    read_json(&plugin_path("hooks/hooks.json"))
}

fn skill_file(file: &str) -> String {
    let path = plugin_path(&format!("skills/{SKILL}/{file}"));
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The YAML frontmatter of a SKILL.md (first line `---`) and its body.
fn frontmatter(text: &str) -> (Value, String) {
    let rest = text
        .strip_prefix("---\n")
        .expect("SKILL.md starts with a --- line");
    let end = rest
        .find("\n---\n")
        .expect("frontmatter closed by a --- line");
    let yaml = &rest[..=end];
    let value: Value = serde_saphyr::from_str(yaml).expect("frontmatter is valid YAML");
    (value, rest[end + 5..].to_owned())
}

/// Kebab-case: lowercase letters, digits and single dashes.
fn is_kebab(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && !s.ends_with('-')
        && !s.contains("--")
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The matcher groups of `event` in hooks.json.
fn hook_groups(event: &str) -> Vec<Value> {
    hooks()["hooks"][event]
        .as_array()
        .unwrap_or_else(|| panic!("hooks.{event} is an array"))
        .clone()
}

mod plugin {
    use super::*;

    mod files_validate_against_documented_schemas {
        use super::*;

        #[test]
        fn manifest_names_the_plugin_in_kebab_case() {
            let m = manifest();
            let name = m["name"].as_str().expect("plugin.json name is a string");
            assert!(is_kebab(name), "name {name:?} is kebab-case");
            assert!(
                !name.starts_with("claude-"),
                "name avoids the claude- prefix"
            );
        }

        #[test]
        fn manifest_metadata_fields_have_documented_types() {
            let m = manifest();
            for key in [
                "version",
                "description",
                "license",
                "homepage",
                "repository",
            ] {
                assert!(
                    m.get(key).is_none_or(Value::is_string),
                    "plugin.json {key} is a string when present"
                );
            }
            assert!(m["version"].is_string(), "plugin.json sets a version");
            assert!(
                m["description"].is_string(),
                "plugin.json sets a description"
            );
            assert!(
                m["author"]["name"].is_string(),
                "plugin.json author is an object with a name"
            );
            if let Some(k) = m.get("keywords") {
                let k = k.as_array().expect("keywords is an array");
                assert!(k.iter().all(Value::is_string), "keywords are strings");
            }
        }

        #[test]
        fn marketplace_has_name_owner_and_plugins() {
            let mk = marketplace();
            let name = mk["name"].as_str().expect("marketplace name is a string");
            assert!(is_kebab(name), "marketplace name {name:?} is kebab-case");
            assert!(mk["owner"]["name"].is_string(), "owner.name is a string");
            assert!(mk["plugins"].is_array(), "plugins is an array");
        }

        #[test]
        fn marketplace_entry_points_at_the_plugin_by_its_manifest_name() {
            let mk = marketplace();
            let entries = mk["plugins"].as_array().expect("plugins is an array");
            assert_eq!(entries.len(), 1, "one plugin entry");
            let entry = &entries[0];
            assert_eq!(
                entry["name"],
                manifest()["name"],
                "entry name = manifest name"
            );
            assert_eq!(entry["source"], PLUGIN_SOURCE);
            assert!(entry["description"].is_string(), "entry has a description");
            let source = entry["source"].as_str().expect("source is a string");
            assert!(!source.contains(".."), "source has no ..");
        }

        #[test]
        fn hooks_json_wraps_events_in_matcher_groups_of_command_hooks() {
            let h = hooks();
            let events = h["hooks"].as_object().expect("top-level hooks object");
            for (event, groups) in events {
                for g in groups.as_array().expect("event maps to an array") {
                    assert!(
                        g.get("matcher").is_none_or(Value::is_string),
                        "{event} matcher is a string when present"
                    );
                    for hook in g["hooks"].as_array().expect("group has a hooks array") {
                        assert_eq!(hook["type"], "command", "{event} hook is a command");
                        assert!(hook["command"].is_string(), "{event} command is a string");
                        assert!(hook["timeout"].is_number(), "{event} timeout is a number");
                    }
                }
            }
        }
    }

    mod the_clerk_skill {
        use super::*;

        #[test]
        fn has_frontmatter_with_its_name_and_one_description() {
            let (fm, body) = frontmatter(&skill_file("SKILL.md"));
            assert_eq!(fm["name"], SKILL);
            let d = fm["description"].as_str().expect("description is a string");
            assert!(!d.trim().is_empty() && d.len() <= 1536, "{d}");
            for word in ["lease", "register", "notice"] {
                assert!(d.contains(word), "description covers {word}: {d}");
            }
            assert_ne!(body.trim(), "");
        }

        #[test]
        fn skill_md_stays_short_and_links_its_references() {
            let text = skill_file("SKILL.md");
            assert!(
                text.lines().count() <= SKILL_MAX_LINES,
                "{} lines",
                text.lines().count()
            );
            assert!(text.contains("leasing.md") && text.contains("registering.md"));
        }

        #[test]
        fn names_no_removed_mcp_tool() {
            for file in SKILL_FILES {
                let text = skill_file(file);
                for tool in MCP_TOOLS {
                    assert!(!text.contains(tool), "{file} names {tool}");
                }
            }
        }

        /// A Monitor watch expires every 30 minutes and wakes the agent for
        /// nothing; a background wait costs nothing while idle.
        #[test]
        fn teaches_a_background_wait_only_when_queued_or_interruptible() {
            let text = skill_file("SKILL.md");
            for needle in ["run_in_background", "clerk wait", "queued", "interruptible"] {
                assert!(text.contains(needle), "SKILL.md mentions {needle}");
            }
            for file in SKILL_FILES {
                assert!(
                    !skill_file(file).contains("Monitor"),
                    "{file} recommends Monitor"
                );
            }
        }

        /// An offer arrives claimed; the follow event carries no usage, so
        /// the skill must send the agent to `clerk leases --mine` for it.
        #[test]
        fn never_claims_an_offer_event_carries_usage() {
            for file in ["SKILL.md", "leasing.md"] {
                for para in skill_file(file).split("\n\n") {
                    let flat = para.replace('\n', " ");
                    if flat.contains("offer") && flat.contains("usage") {
                        assert!(
                            flat.contains("clerk leases --mine"),
                            "{file}: an offer paragraph mentions usage without `clerk leases --mine`: {flat}"
                        );
                    }
                }
            }
        }

        /// Every notice kind the clerk delivers is named, by the name it
        /// prints after `## Notice:`, with what to do.
        #[test]
        fn leasing_md_names_every_notice_kind() {
            let text = skill_file("leasing.md");
            for kind in all_notice_kinds() {
                let name = serde_json::to_value(kind).unwrap();
                let name = name.as_str().unwrap();
                assert!(text.contains(&format!("`{name}`")), "leasing.md: {name}");
            }
        }

        /// Only placeholders the clerk substitutes: a stray `{path}` would
        /// reach an agent's usage text literally.
        #[test]
        fn names_only_placeholders_the_clerk_substitutes() {
            let known: Vec<&str> = [
                ResourceType::Device,
                ResourceType::Service,
                ResourceType::Generic,
            ]
            .into_iter()
            .flat_map(placeholder_keys)
            .copied()
            .collect();
            for file in SKILL_FILES {
                let text = skill_file(file);
                for (i, _) in text.match_indices('{') {
                    let rest = &text[i + 1..];
                    let Some(end) = rest.find('}') else { continue };
                    let word = &rest[..end];
                    let looks_like_key = word.starts_with("unit.")
                        || word.starts_with("resource.")
                        || word == "path";
                    if looks_like_key {
                        assert!(
                            known.contains(&word),
                            "{file}: {{{word}}} is not substituted"
                        );
                    }
                }
            }
        }

        #[test]
        fn description_names_the_status_hand_back_and_override_triggers_and_excludes_clerk_source()
        {
            let (fm, _) = frontmatter(&skill_file("SKILL.md"));
            let d = fm["description"].as_str().expect("description is a string");
            for phrase in [
                "who holds",
                "what is free",
                "hand back",
                "take it anyway",
                "not your own dev server",
                "Not for working on the clerk's own source",
            ] {
                assert!(d.contains(phrase), "description lacks {phrase:?}: {d}");
            }
        }

        /// Research finding 5: agents pipe results into jq/python.
        #[test]
        fn says_to_use_json_when_piping_into_a_script() {
            let text = skill_file("SKILL.md");
            assert!(text.contains("--json") && text.contains("jq"), "SKILL.md");
        }

        #[test]
        fn says_results_are_markdown_ending_with_next_and_never_json() {
            let text = skill_file("SKILL.md");
            assert!(
                text.contains("Markdown") && text.contains("Next:"),
                "SKILL.md"
            );
            for file in SKILL_FILES {
                let text = skill_file(file);
                assert!(!text.contains("Every result is JSON"), "{file}");
            }
        }

        #[test]
        fn is_the_only_skill_and_there_is_no_mcp_server() {
            let dirs: Vec<String> = fs::read_dir(plugin_path("skills"))
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(dirs, [SKILL]);
            assert!(!plugin_path(".mcp.json").exists());
        }
    }

    mod hooks_json_wires_events_to_clerk_hook {
        use super::*;

        #[test]
        fn wires_exactly_the_four_events() {
            let h = hooks();
            let mut events: Vec<&str> = h["hooks"]
                .as_object()
                .expect("hooks object")
                .keys()
                .map(String::as_str)
                .collect();
            events.sort_unstable();
            let mut want: Vec<&str> = HOOKS.iter().map(|(e, _, _)| *e).collect();
            want.sort_unstable();
            assert_eq!(events, want);
        }

        #[test]
        fn each_event_runs_its_clerk_hook_subcommand_once() {
            for (event, sub, _) in HOOKS {
                let commands: Vec<String> = hook_groups(event)
                    .iter()
                    .flat_map(|g| g["hooks"].as_array().expect("hooks array").clone())
                    .map(|h| h["command"].as_str().expect("command").to_owned())
                    .collect();
                assert_eq!(
                    commands,
                    [format!(
                        "\"${{CLAUDE_PLUGIN_ROOT}}/hooks/clerk-hook.sh\" {sub}"
                    )],
                    "{event}"
                );
            }
        }

        #[test]
        fn each_command_runs_from_a_plugin_root_with_spaces() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("with a space");
            fs::create_dir_all(root.join("hooks")).unwrap();
            fs::copy(
                plugin_path("hooks/clerk-hook.sh"),
                root.join("hooks/clerk-hook.sh"),
            )
            .unwrap();
            let fake = dir.path().join("fake-clerk");
            fs::write(&fake, "#!/bin/sh\necho \"ran: $*\"\n").unwrap();
            fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
            for (event, sub, _) in HOOKS {
                let group = &hook_groups(event)[0];
                let command = group["hooks"][0]["command"].as_str().expect("command");
                let out = Command::new("/bin/sh")
                    .args(["-c", command])
                    .env("CLAUDE_PLUGIN_ROOT", &root)
                    .env("CLERK_BIN", &fake)
                    .stdin(Stdio::null())
                    .output()
                    .unwrap();
                assert_eq!(
                    String::from_utf8_lossy(&out.stdout),
                    format!("ran: hook {sub}\n"),
                    "{event}: {out:?}"
                );
            }
        }

        #[test]
        fn each_event_matches_every_occurrence() {
            for (event, _, _) in HOOKS {
                for g in hook_groups(event) {
                    assert!(
                        g.get("matcher").is_none_or(|m| m == "" || m == "*"),
                        "{event} matcher matches all"
                    );
                }
            }
        }

        #[test]
        fn timeouts_stay_within_each_events_budget() {
            for (event, _, max) in HOOKS {
                for g in hook_groups(event) {
                    for h in g["hooks"].as_array().expect("hooks array") {
                        let t = h["timeout"].as_f64().expect("timeout is a number");
                        assert!(t > 0.0 && t <= max, "{event} timeout {t} within {max} s");
                    }
                }
            }
        }
    }

    mod the_plugin_version {
        use super::*;

        #[test]
        fn equals_cargo_pkg_version() {
            assert_eq!(manifest()["version"], env!("CARGO_PKG_VERSION"));
        }
    }

    mod skill_on_a_wait_ended_by_another_session {
        use super::*;

        /// The bullet of SKILL.md that holds `needle`, on one line.
        fn bullet_with(needle: &str) -> String {
            let text = skill_file("SKILL.md").replace("\n  ", " ");
            text.lines()
                .find(|l| l.starts_with("- ") && l.contains(needle))
                .unwrap_or_else(|| panic!("no bullet with {needle}"))
                .to_owned()
        }

        #[test]
        fn calls_it_the_normal_end_after_clear() {
            let bullet = bullet_with("runs another session");
            assert!(bullet.contains("/clear"), "{bullet}");
            assert!(bullet.contains("normal"), "{bullet}");
        }

        #[test]
        fn does_not_blame_the_hooks() {
            let bullet = bullet_with("runs another session");
            assert!(!bullet.contains("hooks are not reaching"), "{bullet}");
            assert!(!bullet.contains("tell the user"), "{bullet}");
        }

        #[test]
        fn blames_the_hooks_for_session_not_hosted() {
            let bullet = bullet_with("session_not_hosted");
            assert!(bullet.contains("hooks are not reaching"), "{bullet}");
            assert!(bullet.contains("**tell the user**"), "{bullet}");
        }
    }

    mod clerk_hook_sh {
        use super::*;

        fn script() -> PathBuf {
            plugin_path("hooks/clerk-hook.sh")
        }

        /// Run the launcher for `event` with `PATH=path`, and `CLERK_BIN`
        /// when given.
        fn launch(path: &Path, clerk_bin: Option<&Path>, event: &str) -> Output {
            let mut cmd = Command::new("/bin/sh");
            cmd.arg(script())
                .arg(event)
                .env_clear()
                .env("PATH", path)
                .stdin(Stdio::null());
            if let Some(bin) = clerk_bin {
                cmd.env("CLERK_BIN", bin);
            }
            cmd.output().unwrap()
        }

        #[test]
        fn is_executable() {
            let mode = fs::metadata(script()).unwrap().permissions().mode();
            assert_ne!(mode & 0o111, 0, "mode {mode:o}");
        }

        mod on_clerk_missing_from_path_and_no_clerk_bin {
            use super::*;

            #[test]
            fn exits_0_silently() {
                let empty = tempfile::tempdir().unwrap();
                let out = launch(empty.path(), None, "stop");
                assert_eq!(out.status.code(), Some(0));
                assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{out:?}");
            }
        }

        mod on_clerk_bin_set {
            use super::*;

            #[test]
            fn runs_that_binary_with_hook_and_the_event() {
                let dir = tempfile::tempdir().unwrap();
                let fake = dir.path().join("fake-clerk");
                fs::write(&fake, "#!/bin/sh\necho \"ran: $*\"\n").unwrap();
                fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
                let out = launch(Path::new("/usr/bin:/bin"), Some(&fake), "pre-tool-use");
                assert_eq!(out.status.code(), Some(0));
                assert_eq!(
                    String::from_utf8_lossy(&out.stdout),
                    "ran: hook pre-tool-use\n"
                );
            }
        }
    }

    mod clerk_hook {
        use super::*;

        /// Run the real `clerk` with `args`, with or without the session
        /// variables.
        fn clerk(args: &[&str], session_vars: bool) -> Output {
            let home = tempfile::tempdir().unwrap();
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_clerk"));
            cmd.args(args)
                .env("CLERK_HOME", home.path())
                .env_remove("CLAUDE_PID")
                .env_remove("CLAUDE_CODE_SESSION_ID")
                .stdin(Stdio::null());
            if session_vars {
                cmd.env("CLAUDE_PID", "1")
                    .env("CLAUDE_CODE_SESSION_ID", "s1");
            }
            cmd.output().unwrap()
        }

        fn assert_silent_success(out: &Output) {
            assert_eq!(out.status.code(), Some(0), "{out:?}");
            assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{out:?}");
        }

        mod on_an_unknown_hook_name {
            use super::*;

            #[test]
            fn exits_0_with_empty_stdout_and_stderr() {
                for vars in [false, true] {
                    assert_silent_success(&clerk(&["hook", "bogus"], vars));
                }
            }
        }

        mod on_a_bad_flag {
            use super::*;

            #[test]
            fn exits_0_silently() {
                for vars in [false, true] {
                    assert_silent_success(&clerk(&["hook", "stop", "--bogus"], vars));
                }
            }
        }
    }

    mod hook_session_start {
        use super::*;

        fn start(plugin_version: Option<&str>) -> Output {
            let home = tempfile::tempdir().unwrap();
            let root = tempfile::tempdir().unwrap();
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_clerk"));
            cmd.args(["hook", "session-start"])
                .env("CLERK_HOME", home.path())
                .env("CLAUDE_PID", "1")
                .env_remove("CLAUDE_CODE_SESSION_ID")
                .env_remove("CLAUDE_PLUGIN_ROOT")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if let Some(v) = plugin_version {
                let dir = root.path().join(".claude-plugin");
                fs::create_dir_all(&dir).unwrap();
                fs::write(dir.join("plugin.json"), format!(r#"{{"version": "{v}"}}"#)).unwrap();
                cmd.env("CLAUDE_PLUGIN_ROOT", root.path());
            }
            let mut child = cmd.spawn().unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(br#"{"session_id": "s1", "source": "startup"}"#)
                .unwrap();
            child.wait_with_output().unwrap()
        }

        mod on_a_plugin_version_different_from_the_binarys {
            use super::*;

            #[test]
            fn the_brief_says_to_update_the_plugin_or_the_binary() {
                let out = start(Some("0.0.1"));
                assert_eq!(out.status.code(), Some(0));
                let doc: Value = serde_json::from_slice(&out.stdout).expect("JSON output");
                let text = doc["hookSpecificOutput"]["additionalContext"]
                    .as_str()
                    .expect("additionalContext");
                assert!(text.contains("0.0.1") && text.contains(env!("CARGO_PKG_VERSION")));
                assert!(
                    text.contains("update the plugin or the clerk binary"),
                    "{text}"
                );
            }
        }

        mod on_a_matching_version_or_no_plugin_root {
            use super::*;

            #[test]
            fn prints_nothing() {
                for out in [start(Some(env!("CARGO_PKG_VERSION"))), start(None)] {
                    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
                }
            }
        }
    }
}
