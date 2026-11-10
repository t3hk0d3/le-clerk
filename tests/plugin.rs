//! The Claude Code plugin under `integrations/claude-code` and the local
//! marketplace at the repo root: every JSON file and both skills checked
//! against the documented schemas (spec Appendix B), the MCP server
//! registration and the hook wiring.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// The plugin root, relative to the repo root.
const PLUGIN_DIR: &str = "integrations/claude-code";
/// The marketplace entry's `source`.
const PLUGIN_SOURCE: &str = "./integrations/claude-code";
/// The one skill (user decision: one skill keeps agents' context small).
const SKILL: &str = "clerk";
/// The skill's files: the entry point and the references it links.
const SKILL_FILES: [&str; 3] = ["SKILL.md", "leasing.md", "registering.md"];
/// The longest `SKILL.md` may be, in lines.
const SKILL_MAX_LINES: usize = 120;
/// The top-level `clerk` commands (`clerk --help`).
const COMMANDS: [&str; 25] = [
    "daemon",
    "resource",
    "unit",
    "status",
    "resources",
    "leases",
    "queue",
    "log",
    "promotions",
    "acquire",
    "attach",
    "cancel",
    "release",
    "extend",
    "run",
    "wait",
    "admin",
    "hook",
    "approve",
    "types",
    "confirm",
    "interruptible",
    "yield",
    "reclaim",
    "claim",
];
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

/// Every `clerk <word>` in command position in `text` (after a backtick,
/// or starting a line of a code block) whose word is not a clerk command.
fn unknown_commands(text: &str) -> Vec<String> {
    text.match_indices("clerk ")
        .filter(|(at, _)| {
            let before = &text[..*at];
            before.ends_with('`')
                || before
                    .rsplit('\n')
                    .next()
                    .is_some_and(|l| l.trim().is_empty())
        })
        .filter_map(|(at, m)| {
            let word: String = text[at + m.len()..]
                .chars()
                .take_while(|c| c.is_ascii_lowercase() || *c == '-')
                .collect();
            (!word.is_empty() && !word.starts_with('-') && !COMMANDS.contains(&word.as_str()))
                .then_some(word)
        })
        .collect()
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
        fn names_only_real_clerk_commands_and_no_mcp_tool() {
            for file in SKILL_FILES {
                let text = skill_file(file);
                assert_eq!(unknown_commands(&text), Vec::<String>::new(), "{file}");
                for tool in MCP_TOOLS {
                    assert!(!text.contains(tool), "{file} names {tool}");
                }
            }
        }

        #[test]
        fn teaches_monitor_on_clerk_wait_follow_with_the_background_fallback() {
            let text = skill_file("SKILL.md");
            for needle in [
                "Monitor",
                "clerk wait --follow",
                "1800000",
                "run_in_background",
            ] {
                assert!(text.contains(needle), "SKILL.md mentions {needle}");
            }
        }

        #[test]
        fn says_results_are_markdown_ending_with_next_and_never_json() {
            let text = skill_file("SKILL.md");
            assert!(text.contains("Markdown") && text.contains("Next:"), "SKILL.md");
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

    mod command_check {
        use super::*;

        #[test]
        fn flags_a_misspelled_command() {
            let text = "Run `clerk aquire board` and `clerk wait --follow`; the clerk hands.\n\
                        clerk relase x";
            assert_eq!(unknown_commands(text), ["aquire", "relase"]);
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
                assert_eq!(commands, [format!("clerk hook {sub}")], "{event}");
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
}
