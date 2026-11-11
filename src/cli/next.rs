//! What every command run by an agent session adds to its JSON result:
//! `summary` (what happened, in words, including what to tell the user)
//! and `next` (the step the clerk expects). Errors end their message with
//! the next step instead.

use leclerk::health::PATH_MISSING;
use serde_json::{Value, json};

use super::views::dirty_explanation;
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
        "usage_error"
        | "invalid_request"
        | "invalid_argument"
        | "invalid_id"
        | "invalid_settings"
        | "invalid_tag"
        | "too_many_tags"
        | "conflicting_tag_edit"
        | "description_multiline"
        | "description_too_long"
        | "usage_too_long"
        | "unknown_placeholder"
        | "health_empty"
        | "health_blank_command"
        | "health_check_not_allowed"
        | "settings_type_mismatch"
        | "settings_empty_field"
        | "settings_zero_port"
        | "extend_not_later" => "Fix the command as the message says and run it again.",
        "duplicate_in_scope" => {
            "It is registered already: use the existing resource and unit the message names."
        }
        "ambiguous_duplicate" => {
            "Check which of the units the details list is this thing, or ask the user."
        }
        "override_required" => {
            "Ask the user whether to take it anyway; only with their word, run it again with \
             --override \"<their reason>\"."
        }
        "extend_refused" => {
            "The extension is refused, for the reason given here: finish up and release or \
             confirm the cleanup, or ask the user."
        }
        "device_absent" => "Check the device: tell the user (unplugged?) or fix the unit's --path.",
        "daemon_unavailable"
        | "daemon_unresponsive"
        | "engine_unavailable"
        | "internal_error"
        | "database_error" => {
            "Try again in a moment; if it fails again, tell the user the clerk is not working."
        }
        "protocol_mismatch" => {
            "Tell the user the clerk daemon must be restarted (`clerk daemon restart`)."
        }
        "agent_unidentified" => "Tell the user Claude Code needs updating.",
        "session_not_hosted" => {
            "Tell the user the hooks are not reaching the clerk (plugin installed? daemon up?)."
        }
        "daemon_stopping" => "Tell the user the clerk daemon is stopped.",
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

/// The summary and next step of a queued request.
fn queued(v: &Value) -> (Vec<String>, String) {
    let mut summary = vec![format!(
        "{} {} at position {} (request {}).",
        if v["existing"] == true {
            "You were already queued for"
        } else {
            "Queued for"
        },
        v["unit"]
            .as_str()
            .map_or_else(|| s(v, "resource"), |u| format!("{}/{u}", s(v, "resource"))),
        v["position"],
        s(v, "request_id")
    )];
    if v["existing"] == true {
        summary.push(
            "It stays as first asked, with no second request; `clerk cancel` it first to \
             change it."
                .to_owned(),
        );
    }
    if let Some(line) = displaced_line(v) {
        summary.push(line);
    }
    summary.push("Exit 6 means queued, not failed: do not acquire again.".to_owned());
    (
        summary,
        format!(
            "The unit is not yours yet: arm a wait to be told when it is. {ARM_AGAIN} \
         `clerk cancel {}` drops the request.{}",
            s(v, "request_id"),
            unhealthy_hint(v)
        ),
    )
}

/// What an override did, when the acquire displaced a holder: whose lease
/// ends and that the unit comes to the caller after it.
fn displaced_line(v: &Value) -> Option<String> {
    let d = v.get("displaced").filter(|d| d.is_object())?;
    Some(format!(
        "Lease {} of {} was overridden at your request: it ends after its cleanup window, then \
         the unit is offered to you.",
        s(d, "lease_id"),
        super::render::holder(&d["holder"])
    ))
}

/// The summary and next step of a `clerk run` left queued, or waiting
/// (`waiting`, with `--wait`): the command has not run.
fn run_queued(v: &Value) -> (Vec<String>, String) {
    let (summary, _) = queued(v);
    let next = if v["waiting"] == true {
        "`clerk run` is waiting for the unit and runs the command once it has it: let it run."
            .to_owned()
    } else {
        format!(
            "The command has not run. Repeat this `clerk run` with --wait (with \
             run_in_background: true, as it can take long): it keeps this request, waits, then \
             runs the command. `clerk cancel {}` drops the request.{}",
            s(v, "request_id"),
            unhealthy_hint(v)
        )
    };
    (summary, next)
}

/// Whether a failed `exists` check is among what blocks a queued request.
fn device_absent(v: &Value) -> bool {
    v["blocked_by"]["kind"] == "unhealthy"
        && v["blocked_by"]["units"]
            .as_array()
            .is_some_and(|u| u.iter().any(|x| x["message"] == PATH_MISSING))
}

/// For a request a failed health check blocks: how to get past it when the
/// caller's own job is what the check sees (`--override` does not), or, for
/// a device that is not there, who can.
fn unhealthy_hint(v: &Value) -> &'static str {
    if device_absent(v) {
        " The device is absent (path missing), so waiting will not help: tell the user \
         (unplugged?) or fix the unit's --path with `clerk unit update`."
    } else if v["blocked_by"]["kind"] == "unhealthy" {
        " The unit fails its health check (see blocked_by), which --override does not get \
         past: if it is your own running job that holds it, cancel and `clerk acquire \
         <resource>/<unit> --skip-checks`; otherwise wait, it is re-checked."
    } else {
        ""
    }
}

/// A unix-ms time as local date and time plus how far away.
fn time(v: &Value) -> String {
    v.as_i64().map_or_else(
        || "?".to_owned(),
        |at| {
            let now = leclerk::clock::Clock::now_ms(&leclerk::clock::SystemClock);
            format!(
                "{} ({})",
                super::render::date_time(at),
                super::render::when(at, now)
            )
        },
    )
}

fn s(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or("?").to_owned()
}

/// Whether a `leases --mine` answer has something an idle agent must hear
/// about: a queued request or an interruptible lease.
fn needs_wait(v: &Value) -> bool {
    v["requests"].as_array().is_some_and(|r| !r.is_empty())
        || v["leases"]
            .as_array()
            .is_some_and(|l| l.iter().any(|x| x["interruptible"] == true))
}

/// What every grant's summary says: what is held, that it was held already
/// on a repeat, a dirty unit and why, and the duplicate requests it ended.
fn granted_summary(v: &Value, lease: &str) -> Vec<String> {
    let mut summary = vec![format!(
        "You {} lease {lease} ({}/{}); read its description and usage before using it.",
        if v["already"] == true {
            "already hold"
        } else {
            "hold"
        },
        s(v, "resource"),
        s(v, "unit")
    )];
    if let Some(reason) = v["dirty"]["reason"].as_str() {
        summary.push(format!(
            "The unit is dirty ({}): check it per its usage before you rely on it.",
            dirty_explanation(reason)
        ));
    }
    if let Some(cancelled) = v["cancelled_requests"].as_array().filter(|c| !c.is_empty()) {
        let ids: Vec<&str> = cancelled.iter().filter_map(Value::as_str).collect();
        summary.push(format!(
            "Your other queued request(s) for this unit are cancelled: {}.",
            ids.join(", ")
        ));
    }
    summary
}

/// The note of a grant: its summary, and a wait only when the lease can be
/// asked to yield.
fn granted(v: &Value, lease: &str) -> (Vec<String>, String) {
    let summary = granted_summary(v, lease);
    let wait = if v["interruptible"] == true {
        format!(" {ARM_AGAIN}")
    } else {
        String::new()
    };
    (
        summary,
        format!(
            "Use the unit, then `clerk release {lease}` when you are done (`clerk extend \
             {lease} --for <d>` for more time).{wait}"
        ),
    )
}

/// What each lease needs from its holder now, soonest first: cleanups by
/// their deadlines, then offers, then leases near or past their end.
fn lease_actions(v: &Value) -> Vec<String> {
    let leases = v["leases"].as_array().map_or(&[][..], Vec::as_slice);
    let mut cleaning: Vec<&Value> = leases.iter().filter(|l| l["state"] == "cleaning").collect();
    cleaning.sort_by_key(|l| l["cleanup_deadline"].as_i64());
    let mut offered: Vec<&Value> = leases.iter().filter(|l| l["state"] == "offered").collect();
    offered.sort_by_key(|l| l["claim_deadline"].as_i64());
    let mut ending: Vec<&Value> = leases
        .iter()
        .filter(|l| l["expiring"] == true || l["overdue"] == true)
        .collect();
    ending.sort_by_key(|l| l["ends_at"].as_i64());
    let cleaning = cleaning.into_iter().map(|l| {
        let id = s(l, "lease_id");
        format!(
            "`clerk confirm {id} clean` by {}, or the unit is marked dirty (`clerk extend {id} \
             --cleanup` asks for the one extension).",
            time(&l["cleanup_deadline"])
        )
    });
    let offered = offered.into_iter().map(|l| {
        format!(
            "`clerk claim {}` by {}, or it goes to the next in line.",
            s(l, "lease_id"),
            time(&l["claim_deadline"])
        )
    });
    let ending = ending.into_iter().map(|l| {
        let id = s(l, "lease_id");
        if l["overdue"] == true {
            format!(
                "Lease {id} is past its end and stays yours until someone asks for the unit: \
                 `clerk extend {id} --for <d>` if you still need it, else `clerk release {id}`."
            )
        } else {
            format!(
                "`clerk extend {id} --for <d>` before {} if you need more time, else `clerk \
                 release {id}`.",
                time(&l["ends_at"])
            )
        }
    });
    cleaning.chain(offered).chain(ending).collect()
}

/// `base`, after what each lease of `v` needs now.
fn after_lease_actions(v: &Value, base: String) -> String {
    let mut parts = lease_actions(v);
    parts.push(base);
    parts.join(" ")
}

/// The note of `leases --mine`: the counts, then what each lease needs now.
fn leases(v: &Value) -> (Vec<String>, String) {
    (
        vec![format!(
            "{} lease(s), {} queued request(s); a wait is {}armed.",
            v["leases"].as_array().map_or(0, Vec::len),
            v["requests"].as_array().map_or(0, Vec::len),
            if v["armed"] == true { "" } else { "not " }
        )],
        after_lease_actions(
            v,
            if v["armed"] == true {
                "Act on the notices as they arrive.".to_owned()
            } else if needs_wait(v) {
                ARM_AGAIN.to_owned()
            } else {
                "No wait is needed for plain leases; release them when you are done.".to_owned()
            },
        ),
    )
}

/// The summary lines and next step for `command`'s result `v`.
pub fn note(command: &str, v: &Value) -> (Vec<String>, String) {
    let lease = s(v, "lease_id");
    match command {
        "acquire" | "claim" | "attach" if v["outcome"] == "queued" => queued(v),
        "run" if v["outcome"] == "queued" => run_queued(v),
        "run" => (
            granted_summary(v, &lease),
            "`clerk run` is running your command now and gives the lease back when it exits; \
             its last line reports the command's status (`- child_exit: N`)."
                .to_owned(),
        ),
        "acquire" | "claim" | "attach" | "reclaim" => granted(v, &lease),
        "release" if v["state"] == "cleaning" => (
            vec![format!("Lease {lease} is in its cleanup window.")],
            format!("Clean the unit up, then `clerk confirm {lease} clean`."),
        ),
        "confirm" if v["already"] == true => (
            vec![format!(
                "Lease {lease} had already ended with its cleanup confirmed; nothing changed."
            )],
            "Nothing more for this lease.".to_owned(),
        ),
        "release" | "confirm" if v["state"] == "ended" => (
            vec![format!(
                "Lease {lease} has ended with its cleanup confirmed; the unit is free for others."
            )],
            "Nothing more for this lease (no `clerk confirm` needed). If you hold or wait for \
             nothing else, you may stop your clerk wait."
                .to_owned(),
        ),
        "confirm" => (
            vec![format!("Lease {lease} is {}.", s(v, "state"))],
            format!("Wait for your unit to come back (the wait tells you). {ARM_AGAIN}"),
        ),
        "extend" if v["state"] == "cleaning" => (
            vec![format!(
                "The cleanup window of lease {lease} now ends {}; that was its one extension.",
                time(&v["cleanup_deadline"])
            )],
            format!("Clean the unit up, then `clerk confirm {lease} clean`."),
        ),
        "extend" => (
            vec![format!("Lease {lease} now ends {}.", time(&v["ends_at"]))],
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
            vec![format!(
                "Request {} is withdrawn.",
                v["request_id"]
                    .as_str()
                    .or(v["cancelled"].as_str())
                    .unwrap_or("?")
            )],
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
        "leases" => leases(v),
        "daemon" if v["running"] == false => (
            vec!["The clerk daemon is stopped.".to_owned()],
            "Nothing: the daemon stays stopped until the next clerk command.".to_owned(),
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
    fn an_ended_release_says_cleanup_is_confirmed_and_nothing_is_left_to_do() {
        let mut v = json!({"lease_id": "L1", "state": "ended", "cleanup_deadline": 5});
        annotate("release", &mut v);
        assert!(
            v["summary"][0]
                .as_str()
                .unwrap()
                .contains("cleanup confirmed"),
            "{v}"
        );
        assert!(
            v["next"].as_str().unwrap().contains("no `clerk confirm`"),
            "{v}"
        );
    }

    /// Research finding 4: summaries show times, not epoch milliseconds.
    #[test]
    fn an_extend_summary_shows_a_date_and_time() {
        let mut v = json!({"lease_id": "L1", "ends_at": 1_800_000_000_000_i64});
        annotate("extend", &mut v);
        let line = v["summary"][0].as_str().unwrap();
        assert!(!line.contains("1800000000000"), "{line}");
        assert!(line.contains(':') && line.contains('('), "{line}");
    }

    /// Research finding 6: each error's next step fits the error.
    #[test]
    fn validation_errors_say_to_fix_the_command() {
        for code in [
            "description_too_long",
            "description_multiline",
            "usage_too_long",
            "unknown_placeholder",
            "too_many_tags",
            "conflicting_tag_edit",
            "settings_type_mismatch",
            "settings_empty_field",
            "settings_zero_port",
            "health_empty",
            "health_blank_command",
            "health_check_not_allowed",
            "extend_not_later",
        ] {
            assert!(after_error(code).starts_with("Fix the command"), "{code}");
        }
    }

    #[test]
    fn duplicate_and_override_errors_get_their_own_next_step() {
        assert!(after_error("duplicate_in_scope").contains("existing"));
        assert!(after_error("ambiguous_duplicate").contains("ask the user"));
        assert!(after_error("override_required").contains("--override"));
        assert!(after_error("extend_refused").contains("release"));
        assert!(after_error("device_absent").contains("tell the user"));
    }

    #[test]
    fn a_plain_grant_asks_for_no_wait() {
        let mut v = json!({"outcome": "granted", "lease_id": "L1", "resource": "r", "unit": "u",
                           "interruptible": false});
        annotate("acquire", &mut v);
        assert!(!v["next"].as_str().unwrap().contains("clerk wait"), "{v}");
    }

    #[test]
    fn an_interruptible_grant_asks_for_a_background_wait() {
        let mut v = json!({"outcome": "granted", "lease_id": "L1", "resource": "r", "unit": "u",
                           "interruptible": true});
        annotate("acquire", &mut v);
        assert!(
            v["next"].as_str().unwrap().contains("run_in_background"),
            "{v}"
        );
    }

    #[test]
    fn a_grant_of_an_abandoned_unit_says_so() {
        let mut v = json!({"outcome": "granted", "lease_id": "L1", "resource": "r", "unit": "u",
                           "dirty": {"reason": "abandoned"}});
        annotate("acquire", &mut v);
        assert!(v["summary"].to_string().contains("abandoned"), "{v}");
    }

    /// `leases --mine` answer holding `leases`, no requests, no wait armed.
    fn mine(leases: &Value) -> Value {
        json!({"leases": leases, "requests": [], "armed": false})
    }

    #[test]
    fn leases_next_starts_with_the_soonest_cleanup_then_offers_then_expiries() {
        let (_, next) = note(
            "leases",
            &mine(&json!([
                {"lease_id": "L1", "state": "held", "expiring": true,
                 "ends_at": 1_800_000_000_000_i64},
                {"lease_id": "L2", "state": "offered", "claim_deadline": 1_800_000_000_000_i64},
                {"lease_id": "L3", "state": "cleaning", "cleanup_deadline": 1_800_000_500_000_i64},
                {"lease_id": "L4", "state": "cleaning", "cleanup_deadline": 1_800_000_000_000_i64},
            ])),
        );
        assert!(next.starts_with("`clerk confirm L4 clean` by "), "{next}");
        let at = |s: &str| next.find(s).unwrap_or_else(|| panic!("{s}: {next}"));
        assert!(
            at("`clerk confirm L3 clean`") < at("`clerk claim L2`"),
            "{next}"
        );
        assert!(
            at("`clerk claim L2`") < at("`clerk extend L1 --for <d>`"),
            "{next}"
        );
    }

    #[test]
    fn leases_next_says_an_overdue_lease_stays_until_someone_asks() {
        let (_, next) = note(
            "leases",
            &mine(&json!([{"lease_id": "L1", "state": "held", "overdue": true,
                          "ends_at": 1_i64}])),
        );
        assert!(next.contains("past its end"), "{next}");
        assert!(next.contains("until someone"), "{next}");
        assert!(next.contains("`clerk extend L1 --for <d>`"), "{next}");
        assert!(next.contains("`clerk release L1`"), "{next}");
    }

    #[test]
    fn leases_next_has_no_action_for_a_plain_held_lease() {
        let (_, next) = note(
            "leases",
            &mine(&json!([{"lease_id": "L1", "state": "held", "ends_at": 1_i64}])),
        );
        assert!(next.starts_with("No wait is needed"), "{next}");
    }

    #[test]
    fn a_repeated_confirm_says_nothing_changed() {
        let mut v = json!({"lease_id": "L1", "state": "ended", "already": true});
        annotate("confirm", &mut v);
        assert!(v["summary"].to_string().contains("already"), "{v}");
    }

    #[test]
    fn a_repeated_claim_says_it_was_already_held() {
        let mut v = json!({"lease_id": "L1", "resource": "r", "unit": "u", "state": "held",
                           "already": true});
        annotate("claim", &mut v);
        assert!(v["summary"].to_string().contains("already"), "{v}");
    }

    #[test]
    fn a_grant_says_which_duplicates_it_cancelled() {
        let mut v = json!({"outcome": "granted", "lease_id": "L1", "resource": "r", "unit": "u",
                           "cancelled_requests": ["R1"]});
        annotate("acquire", &mut v);
        assert!(v["summary"].to_string().contains("R1"), "{v}");
    }

    #[test]
    fn a_cleanup_extension_names_the_new_deadline_and_the_confirm() {
        let mut v = json!({"lease_id": "L1", "state": "cleaning",
                           "cleanup_deadline": 1_800_000_000_000_i64});
        annotate("extend", &mut v);
        assert!(v["summary"].to_string().contains("cleanup"), "{v}");
        assert!(
            v["next"]
                .as_str()
                .unwrap()
                .contains("`clerk confirm L1 clean`"),
            "{v}"
        );
    }

    #[test]
    fn annotate_leaves_lists_alone() {
        let mut v = json!([1, 2]);
        annotate("resources", &mut v);
        assert_eq!(v, json!([1, 2]));
    }
}
