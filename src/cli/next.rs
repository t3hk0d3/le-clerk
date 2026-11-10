//! What every command run by an agent session adds to its JSON result:
//! `summary` (what happened, in words, including what to tell the user)
//! and `next` (the step the clerk expects). Errors end their message with
//! the next step instead.

use serde_json::{Value, json};

use super::wait::ARM_AGAIN;

/// The next step when nothing more specific applies.
const WHERE_YOU_STAND: &str = "Run `clerk leases --mine` to see where you stand.";

/// Add `summary` and `next` to `value`, the JSON result of `command`
/// (the top-level command name, e.g. `acquire`), when it is an object.
pub fn annotate(command: &str, value: &mut Value) {
    if !value.is_object() {
        return;
    }
    let (summary, next) = note(command, value);
    value["summary"] = json!(summary);
    value["next"] = json!(next);
}

/// The next step after an error with `code`, for the end of its message.
#[must_use]
pub fn after_error(code: &str) -> &'static str {
    match code {
        "usage_error" | "invalid_request" | "invalid_argument" | "invalid_id"
        | "invalid_settings" | "invalid_tag" => {
            "Fix the command as the message says and run it again."
        }
        "daemon_unavailable"
        | "daemon_unresponsive"
        | "engine_unavailable"
        | "internal_error"
        | "database_error" => {
            "Try again in a moment; if it fails again, tell the user the clerk is not working."
        }
        "version_mismatch" | "protocol_mismatch" => {
            "Tell the user the clerk daemon must be restarted (`clerk daemon restart`)."
        }
        "agent_unidentified" => "Tell the user Claude Code needs updating.",
        "not_found" => "Run `clerk leases --mine` or `clerk resources` to see where you stand.",
        _ => "Run `clerk leases --mine` to see where you stand.",
    }
}

/// `message` ending with the next step for `code` (unchanged when it
/// already does).
#[must_use]
pub fn error_message(code: &str, message: &str) -> String {
    let next = after_error(code);
    let message = message.trim_end();
    if message.ends_with(next) {
        return message.to_owned();
    }
    let sep = if message.ends_with(['.', '!', '?']) {
        " "
    } else {
        ". "
    };
    format!("{message}{sep}{next}")
}

fn s(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or("?").to_owned()
}

/// The summary lines and next step for `command`'s result `v`.
fn note(command: &str, v: &Value) -> (Vec<String>, String) {
    let lease = s(v, "lease_id");
    match command {
        "acquire" | "claim" | "attach" if v["outcome"] == "queued" => (
            vec![format!(
                "Queued for {} at position {} (request {}).",
                s(v, "resource"),
                v["position"],
                s(v, "request_id")
            )],
            format!(
                "The unit is not yours yet: arm a wait to be told when it is. {ARM_AGAIN} \
                 `clerk cancel {}` drops the request.",
                s(v, "request_id")
            ),
        ),
        "acquire" | "claim" | "attach" | "reclaim" => (
            vec![format!(
                "You hold lease {lease} ({}/{}); read its description and usage before using it.",
                s(v, "resource"),
                s(v, "unit")
            )],
            format!(
                "Use the unit, then `clerk release {lease}` when you are done (`clerk extend \
                 {lease} --for <d>` for more time). {ARM_AGAIN}"
            ),
        ),
        "release" if v["state"] == "cleaning" => (
            vec![format!("Lease {lease} is in its cleanup window.")],
            format!("Clean the unit up, then `clerk confirm {lease} clean`."),
        ),
        "release" | "confirm" if v["state"] == "ended" => (
            vec![format!(
                "Lease {lease} has ended; the unit is free for others."
            )],
            "Nothing more for this lease.".to_owned(),
        ),
        "confirm" => (
            vec![format!("Lease {lease} is {}.", s(v, "state"))],
            format!("Wait for your unit to come back (the wait tells you). {ARM_AGAIN}"),
        ),
        "extend" => (
            vec![format!("Lease {lease} now ends at {}.", v["ends_at"])],
            format!("`clerk release {lease}` when you are done."),
        ),
        "interruptible" => (
            vec![format!(
                "Lease {lease} is {}interruptible.",
                if v["interruptible"] == true {
                    ""
                } else {
                    "not "
                }
            )],
            format!("Keep a wait armed to hear of yield requests. {ARM_AGAIN}"),
        ),
        "yield" => (
            vec![format!("You refused to yield lease {lease}.")],
            format!("`clerk release {lease}` when you are done."),
        ),
        "cancel" => (
            vec![format!("Request {} is withdrawn.", s(v, "request_id"))],
            "Nothing more for this request.".to_owned(),
        ),
        "types" => (
            vec!["The types are device, service and generic.".to_owned()],
            "Register with `clerk resource create <id> --type <type>`, then `clerk unit add` \
             with settings of that type."
                .to_owned(),
        ),
        "resource" | "unit" => (
            Vec::new(),
            "Record the resource and unit ids in the project's docs or your memory; \
             `clerk acquire <resource> --for <d> --purpose <text>` before using one."
                .to_owned(),
        ),
        "leases" => (
            vec![format!(
                "{} lease(s), {} queued request(s); a wait is {}armed.",
                v["leases"].as_array().map_or(0, Vec::len),
                v["requests"].as_array().map_or(0, Vec::len),
                if v["armed"] == true { "" } else { "not " }
            )],
            if v["armed"] == true {
                "Act on the notices as they arrive.".to_owned()
            } else {
                ARM_AGAIN.to_owned()
            },
        ),
        _ => (Vec::new(), WHERE_YOU_STAND.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_message_ends_with_the_next_step_once() {
        let m = error_message("not_found", "no such lease");
        assert!(m.ends_with("to see where you stand."), "{m}");
        assert_eq!(error_message("not_found", &m), m);
    }

    #[test]
    fn annotate_leaves_lists_alone() {
        let mut v = json!([1, 2]);
        annotate("resources", &mut v);
        assert_eq!(v, json!([1, 2]));
    }
}
