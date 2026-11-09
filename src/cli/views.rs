//! Read views (`status`, `resources`, `resource show`, `leases`, `queue`,
//! `log`, `promotions`) and `approve`.
//!
//! The user's CLI sees every scope (view `all`); an agent session sees only
//! user scope and its own project (view `caller`). `log` and `promotions`
//! cover every project, so agents need `--override "<reason>"` for them.

use std::fmt::Write as _;

use clap::Args;
use leclerk::admin::HolderSpec;
use leclerk::client::ApiError;
use leclerk::clock::{Clock, SystemClock};
use leclerk::model::{Caller, HolderRef};
use serde_json::{Value, json};

use super::admin::ScopeFlag;
use super::caller::{self, CallerKind};
use super::gate;
use super::output::{self, View, plain, table, truncate};
use super::{Ctx, UsageError};

/// `--tag TAG[,TAG…]`, repeatable: every tag must match.
#[derive(Args)]
pub struct TagFilter {
    /// Only resources carrying this tag (all given tags must match).
    #[arg(long = "tag", value_delimiter = ',', value_name = "TAG")]
    pub tags: Vec<String>,
}

/// `clerk log` filters.
#[derive(Args)]
pub struct LogArgs {
    /// Only entries since then: `<n>s|m|h|d` ago, or unix milliseconds.
    #[arg(long, value_name = "WHEN")]
    since: Option<String>,
    /// Only entries by this actor: `session:<id>`, `pid:<pid>` or
    /// `user:<tag>`.
    #[arg(long, value_name = "HOLDER")]
    actor: Option<String>,
    /// Only entries of this action, e.g. `resource_created`.
    #[arg(long)]
    action: Option<String>,
    /// At most this many entries.
    #[arg(long)]
    limit: Option<u32>,
}

/// The view an agent session may have, or every scope for the user.
fn view_of(caller: &Caller) -> &'static str {
    if caller::is_agent(caller) {
        "caller"
    } else {
        "all"
    }
}

fn not_found(message: String) -> ApiError {
    ApiError {
        code: "not_found".to_owned(),
        message,
        details: None,
    }
}

pub async fn status(ctx: &Ctx) -> anyhow::Result<()> {
    let caller = ctx.caller(CallerKind::Interactive)?;
    let result = ctx
        .client()
        .await?
        .call("status", &caller, json!({ "view": view_of(&caller) }))
        .await?;
    ctx.out.emit(&View::new(result, |v| fit(&status_text(v))))
}

pub async fn resources(ctx: &Ctx, tags: Vec<String>) -> anyhow::Result<()> {
    let caller = ctx.caller(CallerKind::Interactive)?;
    let result = ctx
        .client()
        .await?
        .call(
            "list_resources",
            &caller,
            json!({ "view": view_of(&caller), "tags": tags }),
        )
        .await?;
    ctx.out
        .emit(&View::new(result, |v| fit(&resources_text(v))))
}

pub async fn show(ctx: &Ctx, id: &str, scope: &ScopeFlag) -> anyhow::Result<()> {
    let caller = ctx.caller(CallerKind::Interactive)?;
    let mut args = json!({ "id": id });
    scope.put(&mut args);
    let result = ctx
        .client()
        .await?
        .call("get_resource", &caller, args)
        .await?;
    ctx.out.emit(&View::new(result, resource_text))
}

pub async fn leases(ctx: &Ctx, mine: bool) -> anyhow::Result<()> {
    let caller = ctx.caller(CallerKind::Interactive)?;
    if mine {
        if !caller::is_agent(&caller) {
            return Err(UsageError(format!(
                "leases --mine lists an agent session's own leases, so it needs an agent session \
                 ({} is not set). Run `clerk leases` to see every lease.",
                caller::SESSION_VAR
            ))
            .into());
        }
        let result = ctx
            .client()
            .await?
            .call("list_my_leases", &caller, json!({}))
            .await?;
        return ctx.out.emit(&View::new(result, my_leases_text));
    }
    let status = ctx
        .client()
        .await?
        .call("status", &caller, json!({ "view": view_of(&caller) }))
        .await?;
    let mut leases = Vec::new();
    for r in status["resources"].as_array().into_iter().flatten() {
        for u in r["units"].as_array().into_iter().flatten() {
            if u["occupancy"]["kind"] != "free" {
                leases.push(json!({
                    "resource": r["id"],
                    "scope": r["scope"],
                    "unit": u["id"],
                    "occupancy": u["occupancy"],
                }));
            }
        }
    }
    ctx.out
        .emit(&View::new(Value::Array(leases), |v| fit(&leases_text(v))))
}

pub async fn queue(ctx: &Ctx, resource: &str) -> anyhow::Result<()> {
    let caller = ctx.caller(CallerKind::Interactive)?;
    let status = ctx
        .client()
        .await?
        .call("status", &caller, json!({ "view": view_of(&caller) }))
        .await?;
    let queues: Vec<Value> = status["resources"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["id"] == resource)
        .map(|r| json!({"resource": r["id"], "scope": r["scope"], "queue": r["queue"]}))
        .collect();
    if queues.is_empty() {
        return Err(not_found(format!(
            "No resource {resource} is visible to you; check `clerk resources`."
        ))
        .into());
    }
    ctx.out
        .emit(&View::new(Value::Array(queues), |v| fit(&queue_text(v))))
}

pub async fn log(ctx: &Ctx, args: LogArgs) -> anyhow::Result<()> {
    let caller = ctx.caller(CallerKind::Interactive)?;
    // The log covers every project; an agent must not read it unasked.
    gate::require_override("read the audit log, which covers every project", &caller)?;
    let now = SystemClock.now_ms();
    let since = args
        .since
        .as_deref()
        .map(|s| parse_since(s, now))
        .transpose()?;
    let actor = args
        .actor
        .as_deref()
        .map(HolderSpec::parse)
        .transpose()
        .map_err(UsageError)?;
    // The log matches actors exactly; a pid alone (no start time) is
    // matched here instead.
    let exact = match &actor {
        Some(HolderSpec::Session(id)) => Some(HolderRef::AgentSession {
            session_id: id.clone(),
        }),
        Some(HolderSpec::User(tag)) => Some(HolderRef::User { tag: tag.clone() }),
        Some(HolderSpec::Pid(_)) | None => None,
    };
    let by_pid = matches!(actor, Some(HolderSpec::Pid(_)));
    let query = json!({
        "since": since,
        "actor": exact,
        "action": args.action,
        "limit": if by_pid { None } else { args.limit },
    });
    let result = ctx
        .client()
        .await?
        .call("list_audit", &caller, query)
        .await?;
    let entries: Vec<Value> = result
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| match &actor {
            Some(spec @ HolderSpec::Pid(_)) => {
                serde_json::from_value::<HolderRef>(e["actor"].clone())
                    .is_ok_and(|h| spec.matches(&h))
            }
            _ => true,
        })
        .take(args.limit.map_or(usize::MAX, |l| l as usize))
        .cloned()
        .collect();
    ctx.out.emit(&View::new(Value::Array(entries), move |v| {
        fit(&log_text(v, now))
    }))
}

pub async fn promotions(ctx: &Ctx) -> anyhow::Result<()> {
    let caller = ctx.caller(CallerKind::Interactive)?;
    // Promotions name other projects' resources and paths.
    gate::require_override("list promotions, which cover every project", &caller)?;
    let result = ctx
        .client()
        .await?
        .call("list_promotions", &caller, json!({}))
        .await?;
    ctx.out
        .emit(&View::new(result, |v| fit(&promotions_text(v))))
}

pub async fn approve(ctx: &Ctx, promotion: String, new_id: Option<String>) -> anyhow::Result<()> {
    let caller = ctx.caller(CallerKind::Interactive)?;
    gate::require_override("approve a promotion", &caller)?;
    let mut args = json!({ "id": promotion });
    if let Some(new_id) = new_id {
        args["new_id"] = json!(new_id);
    }
    let result = ctx
        .client()
        .await?
        .call("approve_promotion", &caller, args)
        .await?;
    ctx.out.emit(&View::new(result, |v| {
        format!(
            "Promoted {} to user scope.\n{}",
            text(&v["id"]),
            resource_text(v)
        )
    }))
}

/// `<n>s|m|h|d` ago, or unix milliseconds.
fn parse_since(s: &str, now_ms: i64) -> Result<i64, UsageError> {
    let bad = || {
        UsageError(format!(
            "--since {s:?} is not a time: use <n>s, <n>m, <n>h or <n>d (how long ago), or unix \
             milliseconds."
        ))
    };
    if let Ok(ms) = s.parse::<i64>() {
        return Ok(ms);
    }
    let unit = s.chars().last().ok_or_else(bad)?;
    let n: i64 = s[..s.len() - unit.len_utf8()].parse().map_err(|_| bad())?;
    let secs = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        _ => return Err(bad()),
    };
    if n < 0 {
        return Err(bad());
    }
    Ok(now_ms.saturating_sub(n.saturating_mul(secs * 1000)))
}

// ---------------------------------------------------------------------------
// Human rendering
// ---------------------------------------------------------------------------

/// Every line cut to the terminal width.
fn fit(text: &str) -> String {
    let width = output::width();
    text.lines()
        .map(|l| truncate(l, width))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A JSON value as plain text.
pub fn text(v: &Value) -> String {
    plain(v)
}

/// `user` or `project:<path>`.
pub fn scope_text(scope: &Value) -> String {
    match scope.get("project") {
        Some(path) => format!("project:{}", text(path)),
        None => text(scope),
    }
}

/// `session:<id>`, `pid:<pid>` or `user:<tag>`.
fn holder_text(h: &Value) -> String {
    match h["kind"].as_str() {
        Some("agent_session") => format!("session:{}", text(&h["session_id"])),
        Some("cli_process") => format!("pid:{}", h["pid"]),
        Some("user") => format!("user:{}", text(&h["tag"])),
        _ => h.to_string(),
    }
}

/// A unit's settings in one line.
pub fn settings_text(s: &Value) -> String {
    match s["type"].as_str() {
        Some("device") => text(&s["path"]),
        Some("service") => {
            let mut t = String::new();
            if let Some(scheme) = s["scheme"].as_str() {
                let _ = write!(t, "{scheme}://");
            }
            let _ = write!(t, "{}:{}", text(&s["host"]), s["port"]);
            if s["mode"] == "listen" {
                t.push_str(" (listen)");
            }
            if let Some(id) = s["service_id"].as_str() {
                let _ = write!(t, " id={id}");
            }
            t
        }
        Some("generic") => format!("key={}", text(&s["identity_key"])),
        _ => s.to_string(),
    }
}

fn tags_text(tags: &Value) -> String {
    match tags.as_array() {
        Some(list) if !list.is_empty() => plain(tags),
        _ => "-".to_owned(),
    }
}

/// `in 4m 10s` or `5m ago`, from unix milliseconds.
pub fn relative(at_ms: i64, now_ms: i64) -> String {
    let delta = at_ms - now_ms;
    let secs = delta.unsigned_abs() / 1000;
    let span = match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {}s", s / 60, s % 60),
        s if s < 86_400 => format!("{}h {}m", s / 3600, (s % 3600) / 60),
        s => format!("{}d {}h", s / 86_400, (s % 86_400) / 3600),
    };
    if delta >= 0 {
        format!("in {span}")
    } else {
        format!("{span} ago")
    }
}

fn occupancy_text(o: &Value, now_ms: i64) -> String {
    match o["kind"].as_str() {
        Some("free") => "free".to_owned(),
        Some("held_in_another_project") => "held by an agent in another project".to_owned(),
        Some("held") => {
            let mut t = format!(
                "held by {} ({}) for \"{}\", lease {}",
                holder_text(&o["holder"]),
                text(&o["state"]),
                text(&o["purpose"]),
                text(&o["lease_id"])
            );
            if let Some(at) = o["ends_at"].as_i64() {
                let _ = write!(t, ", ends {}", relative(at, now_ms));
            }
            t
        }
        _ => o.to_string(),
    }
}

fn status_text(v: &Value) -> String {
    let now = SystemClock.now_ms();
    let resources = v["resources"].as_array().cloned().unwrap_or_default();
    if resources.is_empty() {
        return "No resources.".to_owned();
    }
    let mut out = Vec::new();
    for r in &resources {
        let mut t = format!(
            "{} ({}, {})  tags: {}",
            text(&r["id"]),
            text(&r["type"]),
            scope_text(&r["scope"]),
            tags_text(&r["tags"])
        );
        if r["shadows_user"] == true {
            t.push_str("  [hides a user resource of the same id]");
        }
        if let Some(d) = r["description"].as_str().filter(|d| !d.is_empty()) {
            let _ = write!(t, "\n  {d}");
        }
        let units = r["units"].as_array().cloned().unwrap_or_default();
        if units.is_empty() {
            t.push_str("\n  (no units)");
        }
        for u in &units {
            let _ = write!(
                t,
                "\n  - {}: {}",
                text(&u["id"]),
                occupancy_text(&u["occupancy"], now)
            );
            if let Some(reason) = u["dirty"]["reason"].as_str() {
                let _ = write!(t, ", dirty ({reason})");
            }
            if u["retiring"] == true {
                t.push_str(", retiring");
            }
            if let Some(h) = u["last_health"].as_str() {
                let _ = write!(t, ", health: {h}");
            }
            if let Some(d) = u["description"].as_str().filter(|d| !d.is_empty()) {
                let _ = write!(t, "\n      {d}");
            }
        }
        let queue = r["queue"].as_array().cloned().unwrap_or_default();
        if !queue.is_empty() {
            t.push_str("\n  queue:");
            for q in &queue {
                t.push('\n');
                t.push_str(&queue_entry_text(q));
            }
        }
        out.push(t);
    }
    out.join("\n\n")
}

fn queue_entry_text(q: &Value) -> String {
    let mut t = format!(
        "    {}. {} ({}) \"{}\", request {}",
        q["position"],
        holder_text(&q["holder"]),
        text(&q["priority"]),
        text(&q["purpose"]),
        text(&q["request_id"])
    );
    if let Some(unit) = q["unit"].as_str() {
        let _ = write!(t, ", unit {unit}");
    }
    t
}

fn resources_text(v: &Value) -> String {
    let list = v.as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        return "No resources.".to_owned();
    }
    let mut rows = vec![
        ["ID", "TYPE", "SCOPE", "UNITS", "TAGS", "DESCRIPTION"]
            .map(str::to_owned)
            .to_vec(),
    ];
    for r in &list {
        rows.push(vec![
            text(&r["id"]),
            text(&r["type"]),
            scope_text(&r["scope"]),
            r["units"].as_array().map_or(0, Vec::len).to_string(),
            tags_text(&r["tags"]),
            text(&r["description"]),
        ]);
    }
    table(&rows)
}

/// A resource with everything, usage as the raw template.
pub fn resource_text(r: &Value) -> String {
    let mut t = format!(
        "{} ({}, {})",
        text(&r["id"]),
        text(&r["type"]),
        scope_text(&r["scope"])
    );
    if r["shadows_user"] == true {
        t.push_str("  [hides a user resource of the same id]");
    }
    let _ = write!(t, "\n  description: {}", text(&r["description"]));
    let _ = write!(t, "\n  tags: {}", tags_text(&r["tags"]));
    let health = match r["health"].as_array() {
        None => "type default".to_owned(),
        Some(checks) => checks
            .iter()
            .map(|c| match (c.get("builtin"), c.get("custom")) {
                (Some(b), _) => text(b),
                (_, Some(cmd)) => format!("custom: {}", text(cmd)),
                _ => c.to_string(),
            })
            .collect::<Vec<_>>()
            .join(", "),
    };
    let _ = write!(t, "\n  health: {health}");
    let units = r["units"].as_array().cloned().unwrap_or_default();
    if units.is_empty() {
        t.push_str("\n  units: none");
    } else {
        t.push_str("\n  units:");
    }
    for u in &units {
        let _ = write!(
            t,
            "\n    {}: {}",
            text(&u["id"]),
            settings_text(&u["settings"])
        );
        if let Some(reason) = u["dirty"]["reason"].as_str() {
            let _ = write!(t, ", dirty ({reason})");
        }
        if u["retiring"] == true {
            t.push_str(", retiring");
        }
        if let Some(d) = u["description"].as_str().filter(|d| !d.is_empty()) {
            let _ = write!(t, "  ({d})");
        }
    }
    match r["usage"].as_str().filter(|u| !u.is_empty()) {
        Some(usage) => {
            let _ = write!(t, "\n  usage:\n{usage}");
        }
        None => t.push_str("\n  usage: none"),
    }
    t
}

fn leases_text(v: &Value) -> String {
    let now = SystemClock.now_ms();
    let list = v.as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        return "No leases.".to_owned();
    }
    let mut rows = vec![
        ["UNIT", "HOLDER", "STATE", "ENDS", "LEASE", "PURPOSE"]
            .map(str::to_owned)
            .to_vec(),
    ];
    for l in &list {
        let o = &l["occupancy"];
        let unit = format!("{}/{}", text(&l["resource"]), text(&l["unit"]));
        if o["kind"] == "held" {
            rows.push(vec![
                unit,
                holder_text(&o["holder"]),
                text(&o["state"]),
                o["ends_at"]
                    .as_i64()
                    .map_or_else(|| "-".to_owned(), |at| relative(at, now)),
                text(&o["lease_id"]),
                text(&o["purpose"]),
            ]);
        } else {
            rows.push(vec![unit, "held by an agent in another project".to_owned()]);
        }
    }
    table(&rows)
}

fn my_leases_text(v: &Value) -> String {
    let now = SystemClock.now_ms();
    let leases = v["leases"].as_array().cloned().unwrap_or_default();
    let requests = v["requests"].as_array().cloned().unwrap_or_default();
    if leases.is_empty() && requests.is_empty() {
        return "You hold no leases and wait for nothing.".to_owned();
    }
    let mut out = Vec::new();
    for l in &leases {
        let mut t = format!(
            "lease {}: {}/{} ({})",
            text(&l["lease_id"]),
            text(&l["resource"]),
            text(&l["unit"]),
            text(&l["state"])
        );
        if let Some(at) = l["ends_at"].as_i64() {
            let _ = write!(t, ", ends {}", relative(at, now));
        }
        let _ = write!(t, "\n{}", brief_text(l));
        out.push(t);
    }
    for q in &requests {
        out.push(format!(
            "request {}: {} queued at position {} for \"{}\"",
            text(&q["request_id"]),
            text(&q["resource"]),
            q["position"],
            text(&q["purpose"])
        ));
    }
    out.join("\n\n")
}

/// The brief of a grant or lease: both descriptions, the resource's tags,
/// the unit's settings, the rendered usage and, when handed over dirty, why.
pub fn brief_text(l: &Value) -> String {
    let mut t = format!("  resource: {}", text(&l["resource_description"]));
    let _ = write!(t, "\n  unit: {}", text(&l["unit_description"]));
    let _ = write!(t, "\n  tags: {}", tags_text(&l["resource_tags"]));
    let _ = write!(t, "\n  settings: {}", settings_text(&l["settings"]));
    if let Some(usage) = l["usage"].as_str().filter(|u| !u.is_empty()) {
        let _ = write!(t, "\n  usage:\n{usage}");
    }
    if let Some(reason) = l["dirty"]["reason"].as_str() {
        let _ = write!(
            t,
            "\n  dirty: the last holder did not confirm cleanup ({reason}); expect a messy state"
        );
    }
    t
}

fn queue_text(v: &Value) -> String {
    let mut out = Vec::new();
    for r in v.as_array().into_iter().flatten() {
        let mut t = format!("{} ({})", text(&r["resource"]), scope_text(&r["scope"]));
        let entries = r["queue"].as_array().cloned().unwrap_or_default();
        if entries.is_empty() {
            t.push_str(": nobody waits");
        }
        for q in &entries {
            t.push('\n');
            t.push_str(&queue_entry_text(q));
        }
        out.push(t);
    }
    out.join("\n\n")
}

fn log_text(v: &Value, now_ms: i64) -> String {
    let list = v.as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        return "No log entries.".to_owned();
    }
    let mut rows = vec![
        ["WHEN", "ACTOR", "ACTION", "DETAIL"]
            .map(str::to_owned)
            .to_vec(),
    ];
    for e in &list {
        let mut detail = e["detail"].to_string();
        if let Some(reason) = e["override_reason"].as_str() {
            detail = format!("override: \"{reason}\" {detail}");
        }
        rows.push(vec![
            e["at"]
                .as_i64()
                .map_or_else(|| "-".to_owned(), |at| relative(at, now_ms)),
            holder_text(&e["actor"]),
            text(&e["action"]),
            detail,
        ]);
    }
    table(&rows)
}

fn promotions_text(v: &Value) -> String {
    let list = v.as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        return "No pending promotions.".to_owned();
    }
    let mut rows = vec![
        ["ID", "RESOURCE", "REQUESTER", "REASON"]
            .map(str::to_owned)
            .to_vec(),
    ];
    for p in &list {
        rows.push(vec![
            text(&p["id"]),
            text(&p["resource"]),
            text(&p["requester"]),
            text(&p["reason"]),
        ]);
    }
    let mut t = table(&rows);
    t.push_str("Approve one with `clerk approve <id>`.");
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_takes_a_span_ago() {
        assert_eq!(parse_since("2m", 1_000_000).unwrap(), 880_000);
    }

    #[test]
    fn since_takes_unix_milliseconds() {
        assert_eq!(parse_since("1234", 1_000_000).unwrap(), 1234);
    }

    #[test]
    fn since_refuses_anything_else() {
        assert!(parse_since("soon", 0).unwrap_err().0.contains("<n>m"));
        assert!(parse_since("5w", 0).is_err());
        assert!(parse_since("", 0).is_err());
    }

    #[test]
    fn relative_reads_both_ways() {
        assert_eq!(relative(70_000, 0), "in 1m 10s");
        assert_eq!(relative(0, 5_000), "5s ago");
    }

    #[test]
    fn invisible_holder_reads_plainly() {
        assert_eq!(
            occupancy_text(&json!({"kind": "held_in_another_project"}), 0),
            "held by an agent in another project"
        );
    }
}
