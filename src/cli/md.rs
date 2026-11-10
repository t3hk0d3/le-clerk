//! Results for agent sessions as labelled Markdown, rendered from the same
//! JSON scripts get: every non-empty field present and labelled, without
//! quotes, braces, escapes or nulls; times as local date and time plus how
//! far away; a fixed heading and field order for grants, queued requests,
//! leases and notices; and the next step last (`Next: …`).

use serde_json::{Map, Value};

/// Render `value`, the JSON result of agent command `command`, at `now_ms`.
/// An object ends with `Next: …` (its own `next`, or the one
/// [`super::next::note`] gives); a list is one block per item.
#[must_use]
pub fn render(command: &str, value: &Value, now_ms: i64) -> String {
    match value {
        Value::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let title = title(item).unwrap_or_else(|| format!("item {}", i + 1));
                format!("## {title}\n{}", body(item, now_ms, &[]))
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
        Value::Object(m) => {
            let (heading, used) = heading(m);
            let (summary, next) = match (m.get("summary"), m.get("next")) {
                (Some(Value::Array(s)), Some(Value::String(n))) => (
                    s.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect(),
                    n.clone(),
                ),
                _ => super::next::note(command, value),
            };
            let mut skip: Vec<&str> = vec!["summary", "next"];
            skip.extend(used);
            if heading.is_some() {
                // Fields that only restate the heading or a default.
                skip.extend(restated(m));
            }
            let mut out = String::new();
            if let Some(h) = heading {
                out.push_str(&h);
                out.push('\n');
            }
            out.push_str(&body(value, now_ms, &skip));
            let summary: Vec<String> = summary.into_iter().filter(|l| !l.is_empty()).collect();
            if !summary.is_empty() {
                out.push_str("\n\n");
                out.push_str(&summary.join("\n"));
            }
            out.push_str("\n\nNext: ");
            out.push_str(&next);
            out.trim_start_matches('\n').to_owned()
        }
        other => scalar("", other, now_ms),
    }
}

/// An error, for an agent: heading, message (ending with the next step).
#[must_use]
pub fn error(code: &str, message: &str) -> String {
    format!("## Error: {code}\n{message}")
}

/// The heading of a hand-tuned layout and the fields it already shows.
fn heading(m: &Map<String, Value>) -> (Option<String>, Vec<&'static str>) {
    let s = |k: &str| m.get(k).and_then(Value::as_str);
    let at = |r: Option<&str>, u: Option<&str>| match (r, u) {
        (Some(r), Some(u)) => format!("{r}/{u}"),
        (Some(r), None) => r.to_owned(),
        _ => "?".to_owned(),
    };
    match (
        s("outcome"),
        s("kind"),
        s("action"),
        s("lease_id"),
        s("state"),
    ) {
        (Some("granted"), ..) => (
            Some(format!("## Granted: {}", at(s("resource"), s("unit")))),
            vec!["outcome"],
        ),
        (Some("queued"), ..) => (
            Some(match s("resource") {
                Some(_) => format!("## Queued: {}", at(s("resource"), s("unit"))),
                None => format!("## Queued: request {}", s("request_id").unwrap_or("?")),
            }),
            vec!["outcome"],
        ),
        (_, Some(kind), Some(_), ..) => (Some(format!("## Notice: {kind}")), vec!["kind"]),
        (None, _, _, Some(id), Some(_)) => (Some(format!("## Lease {id}")), Vec::new()),
        _ => (None, Vec::new()),
    }
}

/// Fields of a hand-tuned layout that say nothing the heading or a default
/// does not: the caller's own holder, `held` state, normal priority, not
/// interruptible.
fn restated(m: &Map<String, Value>) -> Vec<&'static str> {
    let mut out = vec!["holder"];
    if m.get("state").and_then(Value::as_str) == Some("held") {
        out.push("state");
    }
    if m.get("priority").and_then(Value::as_str) == Some("normal") {
        out.push("priority");
    }
    if m.get("interruptible") == Some(&Value::Bool(false)) {
        out.push("interruptible");
    }
    out
}

/// The title of a list item.
fn title(item: &Value) -> Option<String> {
    let s = |k: &str| item.get(k).and_then(Value::as_str);
    s("id")
        .or_else(|| s("lease_id"))
        .or_else(|| s("request_id"))
        .map(str::to_owned)
        .or_else(|| {
            s("resource").map(|r| match s("unit") {
                Some(u) => format!("{r}/{u}"),
                None => r.to_owned(),
            })
        })
}

/// The fields of `value` (an object), `skip` left out: one `- key: value`
/// line each, then a `### key` section per multi-line string.
fn body(value: &Value, now_ms: i64, skip: &[&str]) -> String {
    let Value::Object(m) = value else {
        return scalar("", value, now_ms);
    };
    let mut lines = Vec::new();
    let mut sections = Vec::new();
    for (k, v) in m {
        if skip.contains(&k.as_str()) || is_empty(v) {
            continue;
        }
        if let Value::String(text) = v
            && text.contains('\n')
        {
            sections.push(format!("### {k}\n{}", text.trim_end()));
            continue;
        }
        lines.push(field(k, v, now_ms, 0));
    }
    let mut out = lines.join("\n");
    for s in sections {
        out.push_str("\n\n");
        out.push_str(&s);
    }
    out
}

/// `- key: value` at `indent`, nested values below.
fn field(key: &str, v: &Value, now_ms: i64, indent: usize) -> String {
    let pad = " ".repeat(indent);
    match v {
        Value::Object(m) => {
            let inner: Vec<String> = m
                .iter()
                .filter(|(_, x)| !is_empty(x))
                .map(|(k, x)| field(k, x, now_ms, indent + 2))
                .collect();
            format!("{pad}- {key}:\n{}", inner.join("\n"))
        }
        Value::Array(a) if a.iter().all(|x| !x.is_object() && !x.is_array()) => {
            let items: Vec<String> = a.iter().map(|x| scalar(key, x, now_ms)).collect();
            format!("{pad}- {key}: {}", items.join(", "))
        }
        Value::Array(a) => {
            let blocks: Vec<String> = a
                .iter()
                .filter(|x| !is_empty(x))
                .map(|x| match x {
                    Value::Object(m) => m
                        .iter()
                        .filter(|(_, y)| !is_empty(y))
                        .map(|(k, y)| field(k, y, now_ms, indent + 2))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    other => format!("{pad}  - {}", scalar(key, other, now_ms)),
                })
                .collect();
            format!("{pad}- {key}:\n{}", blocks.join("\n\n"))
        }
        Value::String(text) if text.contains('\n') => {
            let body: Vec<String> = text.lines().map(|l| format!("{pad}    {l}")).collect();
            format!("{pad}- {key}:\n{}", body.join("\n"))
        }
        other => format!("{pad}- {}: {}", label(key), scalar(key, other, now_ms)),
    }
}

/// A scalar as text: times as local date and time plus how far, durations
/// as a span.
fn scalar(key: &str, v: &Value, now_ms: i64) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) if is_time(key) => n.as_i64().map_or_else(
            || n.to_string(),
            |at| {
                format!(
                    "{} ({})",
                    super::render::date_time(at),
                    super::render::when(at, now_ms)
                )
            },
        ),
        Value::Number(n) if is_duration(key) => n
            .as_u64()
            .map_or_else(|| n.to_string(), |ms| super::render::span(ms / 1000)),
        other => other.to_string(),
    }
}

/// A field's label: a duration loses its `_ms` (its value says the unit).
fn label(key: &str) -> &str {
    if is_duration(key) {
        key.strip_suffix("_ms").unwrap_or(key)
    } else {
        key
    }
}

/// Null, an empty string, list or object.
fn is_empty(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(m) => m.is_empty(),
        _ => false,
    }
}

/// A field holding a unix-ms time.
fn is_time(key: &str) -> bool {
    key.ends_with("_at") || key == "deadline" || key.ends_with("_deadline")
}

/// A field holding a length of time in ms.
fn is_duration(key: &str) -> bool {
    key.ends_with("_ms")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const NOW: i64 = 1_800_000_000_000;

    /// Every leaf value of `v` (strings, numbers, booleans), except times
    /// and durations, which are reformatted.
    fn leaves(v: &Value, key: &str, out: &mut Vec<String>) {
        match v {
            Value::Object(m) => {
                for (k, x) in m {
                    leaves(x, k, out);
                }
            }
            Value::Array(a) => {
                for x in a {
                    leaves(x, key, out);
                }
            }
            Value::Null => {}
            Value::String(s) if s.is_empty() => {}
            Value::Number(_) if is_time(key) || is_duration(key) => {}
            Value::String(s) => out.push(s.clone()),
            other => out.push(other.to_string()),
        }
    }

    fn grant() -> Value {
        json!({
            "outcome": "granted", "lease_id": "lse_01ABC", "resource": "c6-boards",
            "unit": "c6-a", "ends_at": NOW + 20 * 60_000,
            "settings": {"type": "device", "path": "/dev/serial/by-id/usb-Espressif-if00"},
            "resource_description": "ESP32-C6 dev boards on the USB hub",
            "unit_description": "left port, blue sticker",
            "resource_tags": ["esp32c6", "devboard"],
            "usage": "Flash: `idf.py -p /dev/x flash`.\nGotcha: hold BOOT.\nClean: erase-flash.",
            "dirty": null
        })
    }

    #[test]
    fn a_flat_object_is_one_labelled_line_per_non_empty_field_in_order() {
        let md = render(
            "x",
            &json!({"b": "two", "a": 1, "n": null, "e": "", "l": [], "o": {}}),
            NOW,
        );
        // The JSON's order, which is by key (serde_json's map).
        assert!(md.contains("- a: 1\n- b: two"), "{md}");
        for gone in ["- n:", "- e:", "- l:", "- o:"] {
            assert!(!md.contains(gone), "{md}");
        }
    }

    #[test]
    fn nested_objects_indent_and_scalar_lists_join() {
        let md = render(
            "x",
            &json!({"s": {"path": "/p", "type": "device"}, "tags": ["a", "b"]}),
            NOW,
        );
        assert!(md.contains("- s:\n  - path: /p\n  - type: device"), "{md}");
        assert!(md.contains("- tags: a, b"), "{md}");
    }

    #[test]
    fn lists_of_objects_are_one_block_per_item() {
        let md = render(
            "x",
            &json!({"leases": [{"lease_id": "L1"}, {"lease_id": "L2"}]}),
            NOW,
        );
        assert!(
            md.contains("- leases:\n  - lease_id: L1\n\n  - lease_id: L2"),
            "{md}"
        );
    }

    #[test]
    fn a_multi_line_string_is_a_section_verbatim() {
        let md = render("x", &json!({"usage": "line one\n  line `two`"}), NOW);
        assert!(md.contains("### usage\nline one\n  line `two`"), "{md}");
    }

    #[test]
    fn times_show_local_date_and_time_and_how_far() {
        let md = render("x", &json!({"ends_at": NOW + 20 * 60_000}), NOW);
        assert!(md.contains("- ends_at: "), "{md}");
        assert!(md.contains("(in 20m)"), "{md}");
        assert!(!md.contains(&(NOW + 20 * 60_000).to_string()), "{md}");
    }

    #[test]
    fn a_grant_has_its_heading_every_value_and_next_last() {
        let g = grant();
        let md = render("acquire", &g, NOW);
        assert!(md.starts_with("## Granted: c6-boards/c6-a\n"), "{md}");
        assert!(
            md.trim_end().lines().last().unwrap().starts_with("Next: "),
            "{md}"
        );
        let mut want = Vec::new();
        leaves(&g, "", &mut want);
        for leaf in want.iter().filter(|l| *l != "granted") {
            for line in leaf.lines() {
                assert!(md.contains(line), "missing {line:?} in\n{md}");
            }
        }
    }

    /// A grant as an agent gets it today: every lease field, with the
    /// summary and next the agent JSON carries.
    fn full_grant() -> Value {
        let mut g = grant();
        for (k, v) in [
            ("claim_deadline", Value::Null),
            ("cleanup_deadline", Value::Null),
            ("created_at", json!(NOW)),
            ("duration_ms", json!(1_200_000)),
            ("end_cause", Value::Null),
            (
                "holder",
                json!({"kind": "agent_session", "session_id": "a34b0631-6339-456d-bbb0-6652509d1387"}),
            ),
            ("interruptible", json!(false)),
            ("override_reason", Value::Null),
            ("paused_remaining_ms", Value::Null),
            ("priority", json!("normal")),
            ("purpose", json!("flash and test the OTA path")),
            ("request_id", Value::Null),
            ("state", json!("held")),
        ] {
            g[k] = v;
        }
        super::super::next::annotate("acquire", &mut g);
        g
    }

    #[test]
    fn a_grant_is_at_most_80_percent_of_its_json() {
        let g = full_grant();
        let md = render("acquire", &g, NOW);
        let json = g.to_string().len();
        assert!(md.len() * 10 <= json * 8, "{} vs {json}:\n{md}", md.len());
    }

    #[test]
    fn queued_has_its_heading_and_every_value() {
        let q = json!({"outcome": "queued", "request_id": "req_1", "resource": "c6-boards",
                       "unit": "c6-a", "position": 2, "estimated_wait_ms": 600_000,
                       "blocked_by": {"kind": "held", "unit": "c6-a", "purpose": "soak"}});
        let md = render("acquire", &q, NOW);
        assert!(md.starts_with("## Queued: c6-boards/c6-a\n"), "{md}");
        let mut want = Vec::new();
        leaves(&q, "", &mut want);
        for leaf in want.iter().filter(|l| *l != "queued") {
            assert!(md.contains(leaf.as_str()), "missing {leaf:?} in\n{md}");
        }
        assert!(md.contains("10m"), "{md}");
    }

    #[test]
    fn a_notice_has_its_heading_and_every_value() {
        let n = json!({"id": "ntc_1", "kind": "expiry_warning", "lease_id": "lse_1",
                       "resource": "b", "unit": "u1", "deadline": NOW + 60_000,
                       "action": "Extend or release before the end.", "created_at": NOW});
        let md = render("wait", &n, NOW);
        assert!(md.starts_with("## Notice: expiry_warning\n"), "{md}");
        let mut want = Vec::new();
        leaves(&n, "", &mut want);
        for leaf in want.iter().filter(|l| *l != "expiry_warning") {
            assert!(md.contains(leaf.as_str()), "missing {leaf:?} in\n{md}");
        }
    }

    #[test]
    fn a_list_is_one_block_per_item_without_next() {
        let md = render("resources", &json!([{"id": "a"}, {"id": "b"}]), NOW);
        assert!(md.contains("## a\n") && md.contains("## b\n"), "{md}");
        assert!(!md.contains("Next:"), "{md}");
    }

    #[test]
    fn an_error_has_its_code_heading_and_message() {
        let md = error("not_found", "no such lease. Run `clerk leases --mine`.");
        assert_eq!(
            md,
            "## Error: not_found\nno such lease. Run `clerk leases --mine`."
        );
    }
}
