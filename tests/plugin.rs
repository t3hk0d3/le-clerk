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
/// The skills, by directory name (equal to their frontmatter `name`).
const SKILLS: [&str; 2] = ["leasing", "registering-resources"];
/// The MCP tools the daemon serves (Global Constraints).
const TOOLS: [&str; 21] = [
    "type_list",
    "resource_create",
    "resource_update",
    "resource_delete",
    "resource_list",
    "resource_get",
    "unit_add",
    "unit_update",
    "unit_remove",
    "unit_retire",
    "lease_acquire",
    "lease_status",
    "lease_extend",
    "lease_release",
    "lease_confirm",
    "lease_set_interruptible",
    "lease_yield_refuse",
    "lease_reclaim",
    "lease_cancel",
    "lease_claim",
    "lease_wait",
];
/// Argument, result and notice field names that share a tool prefix but
/// are not tools; the skills may name them.
const FIELDS: [&str; 9] = [
    "lease_id",
    "request_id",
    "unit_description",
    "resource_description",
    "resource_tags",
    "unit_links",
    "lease_ended",
    "unit_returned",
    "resource_removed",
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

fn skill_text(name: &str) -> String {
    let path = plugin_path(&format!("skills/{name}/SKILL.md"));
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

/// Every word in `text` that looks like a clerk tool name but is neither a
/// tool nor a known field name.
fn unknown_tool_names(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| {
            ["lease_", "resource_", "unit_", "type_"]
                .iter()
                .any(|p| w.starts_with(p))
        })
        .filter(|w| !TOOLS.contains(w) && !FIELDS.contains(w))
        .map(str::to_owned)
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

        #[test]
        fn skills_have_frontmatter_with_their_name_and_a_description() {
            for skill in SKILLS {
                let (fm, body) = frontmatter(&skill_text(skill));
                assert_eq!(fm["name"], skill, "{skill} frontmatter name");
                let d = fm["description"].as_str().expect("description is a string");
                assert!(!d.trim().is_empty(), "{skill} description is not empty");
                assert!(d.len() <= 1536, "{skill} description fits the listing cap");
                assert!(!body.trim().is_empty(), "{skill} has a body");
            }
        }

        #[test]
        fn skills_mention_only_tool_names_the_clerk_serves() {
            for skill in SKILLS {
                let unknown = unknown_tool_names(&skill_text(skill));
                assert!(
                    unknown.is_empty(),
                    "{skill} names unknown tools {unknown:?}"
                );
            }
        }
    }

    mod tool_name_check {
        use super::*;

        #[test]
        fn flags_a_misspelled_tool() {
            let text = "Call `lease_aquire`, then `lease_wait` and `type_lists`.";
            assert_eq!(unknown_tool_names(text), ["lease_aquire", "type_lists"]);
        }

        #[test]
        fn accepts_tools_and_field_names() {
            let text =
                "lease_extend {\"lease_id\": \"x\"}; lease_cancel with request_id; type_list";
            assert_eq!(unknown_tool_names(text), Vec::<String>::new());
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
