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
use leclerk::model::{Caller, Check, HealthSpec, HolderRef, ProjectKey, ResourceType, ServiceMode};
use serde_json::{Value, json};

use super::admin::ScopeFlag;
use super::caller::{self, CallerKind};
use super::gate;
use super::output::{Mode, View, plain};
use super::render::{self, ITEM, SEP, SUB, Style};
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
    /// Only entries since then: `<n>s|m|h|d` ago (e.g. `2h`), or an RFC 3339
    /// timestamp (e.g. `2026-11-10T09:00:00Z`).
    #[arg(long, value_name = "WHEN", allow_hyphen_values = true)]
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
    /// Print ids whole instead of shortening them.
    #[arg(long)]
    wide: bool,
}

/// The view an agent session may have, or every scope for the user.
fn view_of(caller: &Caller) -> &'static str {
    if caller::is_agent(caller) {
        "caller"
    } else {
        "all"
    }
}

/// The daemon's `not_found` for a resource, with its details.
fn resource_not_found(id: &str) -> ApiError {
    ApiError {
        code: "not_found".to_owned(),
        message: "no such resource or unit".to_owned(),
        details: Some(json!({"kind": "resource", "id": id})),
    }
}

pub async fn status(ctx: &Ctx) -> anyhow::Result<()> {
    let caller = ctx.caller(CallerKind::Interactive)?;
    let result = ctx
        .client()
        .await?
        .call("status", &caller, json!({ "view": view_of(&caller) }))
        .await?;
    ctx.out.emit(&View::new(result, status_text))
}

pub async fn resources(ctx: &Ctx, tags: Vec<String>) -> anyhow::Result<()> {
    let caller = ctx.caller(CallerKind::Interactive)?;
    let client = ctx.client().await?;
    let result = client
        .call(
            "list_resources",
            &caller,
            json!({ "view": view_of(&caller), "tags": tags }),
        )
        .await?;
    // People see each resource's occupancy too; JSON stays the listing.
    let status = if ctx.out.mode() == Mode::Human {
        client
            .call("status", &caller, json!({ "view": view_of(&caller) }))
            .await?
    } else {
        Value::Null
    };
    ctx.out
        .emit(&View::new(result, move |v| resources_text(v, &status)))
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
        let client = ctx.client().await?;
        let mut result = client.call("list_my_leases", &caller, json!({})).await?;
        if let HolderRef::AgentSession { session_id } = &caller.holder {
            let brief = client
                .call(
                    "session_brief",
                    &caller,
                    json!({ "session_id": session_id }),
                )
                .await?;
            result["armed"] = json!(brief["open_waits"].as_u64().unwrap_or(0) > 0);
        }
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
    ctx.out.emit(&View::new(Value::Array(leases), leases_text))
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
        return Err(resource_not_found(resource).into());
    }
    ctx.out.emit(&View::new(Value::Array(queues), queue_text))
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
    let wide = args.wide;
    ctx.out.emit(&View::new(Value::Array(entries), move |v| {
        log_text(v, now, wide)
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
    ctx.out.emit(&View::new(result, promotions_text))
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

/// `<n>s|m|h|d` ago (n a whole number, 0 or more), or an RFC 3339
/// timestamp; unix milliseconds. Bare numbers and negative spans are
/// refused: `--since 5` reads too easily as "5 minutes".
fn parse_since(s: &str, now_ms: i64) -> Result<i64, UsageError> {
    let bad = || {
        UsageError(format!(
            "--since {s:?} is not a time: use <n>s|m|h|d for how long ago (e.g. 2h), or an \
             RFC 3339 timestamp (e.g. 2026-11-10T09:00:00Z)."
        ))
    };
    // `--since` takes hyphen values (so `-5m` gets this message), which
    // would also swallow a following option.
    if s.starts_with("--") {
        return Err(UsageError(format!(
            "--since needs a value before the next option ({s}): use <n>s|m|h|d for how long \
             ago (e.g. 2h), or an RFC 3339 timestamp (e.g. 2026-11-10T09:00:00Z)."
        )));
    }
    if let Some(ms) = parse_rfc3339(s) {
        return Ok(ms);
    }
    let unit = s.chars().last().ok_or_else(bad)?;
    let digits = &s[..s.len() - unit.len_utf8()];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let n: i64 = digits.parse().map_err(|_| bad())?;
    let secs = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        _ => return Err(bad()),
    };
    Ok(now_ms.saturating_sub(n.saturating_mul(secs * 1000)))
}

/// An RFC 3339 timestamp (`YYYY-MM-DDTHH:MM:SS[.frac](Z|±HH:MM)`) as unix
/// milliseconds; `None` for anything else.
fn parse_rfc3339(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    let num = |from: usize, len: usize| -> Option<i64> {
        let part = bytes.get(from..from + len)?;
        part.iter()
            .all(u8::is_ascii_digit)
            .then(|| std::str::from_utf8(part).ok()?.parse().ok())?
    };
    let at = |idx: usize, set: &[u8]| bytes.get(idx).is_some_and(|x| set.contains(x));
    if !(at(4, b"-") && at(7, b"-") && at(10, b"Tt ") && at(13, b":") && at(16, b":")) {
        return None;
    }
    let (year, month, day) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    let (hour, minute, second) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
    if !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    // Optional fraction: milliseconds from its first three digits.
    let mut pos = 19;
    let mut millis = 0;
    if at(pos, b".") {
        let digits = bytes[pos + 1..]
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .count();
        if digits == 0 {
            return None;
        }
        let frac = std::str::from_utf8(&bytes[pos + 1..pos + 1 + digits.min(3)]).ok()?;
        millis = frac.parse::<i64>().ok()? * 10_i64.pow(3 - u32::try_from(frac.len()).ok()?);
        pos += 1 + digits;
    }
    let offset_secs = match bytes.get(pos..)? {
        [b'Z' | b'z'] => 0,
        [sign @ (b'+' | b'-'), ..] if bytes.len() == pos + 6 && at(pos + 3, b":") => {
            let (off_h, off_m) = (num(pos + 1, 2)?, num(pos + 4, 2)?);
            if off_h > 23 || off_m > 59 {
                return None;
            }
            let secs = off_h * 3600 + off_m * 60;
            if *sign == b'+' { secs } else { -secs }
        }
        _ => return None,
    };
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second - offset_secs;
    Some(secs * 1000 + millis)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days from 1970-01-01 to the given proleptic Gregorian date (Howard
/// Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// ---------------------------------------------------------------------------
// Human rendering
// ---------------------------------------------------------------------------

/// A JSON value as plain text.
pub fn text(v: &Value) -> String {
    plain(v)
}

/// `user` or `project <folder>`.
pub fn scope_text(scope: &Value) -> String {
    render::scope(scope)
}

/// A unit's settings in one line: a device path, a service address or a
/// generic key.
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
                let _ = write!(t, " id {id}");
            }
            t
        }
        Some("generic") => format!("key {}", text(&s["identity_key"])),
        _ => s.to_string(),
    }
}

/// A unit's settings as a labelled line: `path: …`, `address: …` or
/// `key: …`.
fn settings_line(s: &Value) -> String {
    match s["type"].as_str() {
        Some("device") => format!("path: {}", text(&s["path"])),
        Some("service") => format!("address: {}", settings_text(s)),
        Some("generic") => format!("key: {}", text(&s["identity_key"])),
        _ => format!("settings: {s}"),
    }
}

/// A non-empty string field.
fn some_text(v: &Value) -> Option<&str> {
    v.as_str().filter(|s| !s.trim().is_empty())
}

/// `resource/unit`, the resource bold.
fn unit_ref(resource: &Value, unit: &Value, style: Style) -> String {
    format!("{}/{}", style.id(&text(resource)), text(unit))
}

/// The message of a failed health check, from a unit's `last_health`.
fn unhealthy_message(u: &Value) -> Option<String> {
    u["last_health"]
        .as_str()
        .and_then(|h| h.strip_prefix("unhealthy"))
        .map(|why| why.trim_start_matches(':').trim().to_owned())
}

/// Why a unit was handed over dirty, in words.
fn dirty_explanation(reason: &str) -> &'static str {
    match reason {
        "window_passed" => "the last holder did not confirm cleanup in time",
        "expired" => "the last lease expired before its cleanup was confirmed",
        "abandoned" => "the previous owner abandoned it: past its lease and did not answer",
        "holder_gone" => "the last holder went away",
        "override" => "the last holder was overridden",
        "command_failed" => "the last holder's command failed",
        "admin_forced" => "an admin ended the last lease",
        _ => "the last holder did not confirm cleanup",
    }
}

/// `▮▯ 1/2 free` (free units first) with `⚠` when a unit is unhealthy.
/// A unit counts as free when nobody holds it, it is healthy and it is not
/// retiring.
fn occupancy_bar(units: &[Value], style: Style) -> String {
    let free = units
        .iter()
        .filter(|u| {
            u["occupancy"]["kind"] == "free"
                && unhealthy_message(u).is_none()
                && u["retiring"] != true
        })
        .count();
    let bar = format!(
        "{}{}",
        render::FREE.repeat(free),
        render::BUSY.repeat(units.len() - free)
    );
    let mut t = if bar.is_empty() {
        format!("{free}/{} free", units.len())
    } else {
        format!("{bar} {free}/{} free", units.len())
    };
    if units.iter().any(|u| unhealthy_message(u).is_some()) {
        let _ = write!(t, " {}", warn_glyph(style));
    }
    t
}

/// A yellow `⚠` alone.
fn warn_glyph(style: Style) -> String {
    style.warn("").trim_end().to_owned()
}

/// The header of a resource block: id, occupancy, type, scope and (with
/// `tags`) its tags.
fn resource_header(r: &Value, units: &[Value], tags: bool, style: Style) -> String {
    let mut t = format!(
        "{ITEM}{} {}{SEP}{}{SEP}{}",
        style.id(&text(&r["id"])),
        occupancy_bar(units, style),
        text(&r["type"]),
        render::scope(&r["scope"])
    );
    if tags && r["tags"].as_array().is_some_and(|t| !t.is_empty()) {
        let _ = write!(t, "{SEP}{}", render::tags_of(&r["tags"], style));
    }
    if r["shadows_user"] == true {
        let _ = write!(t, "{SEP}hides the user resource of the same id");
    }
    t
}

/// One unit of the status view: its line, then its description and the
/// holder's purpose on an indented line.
fn unit_block(u: &Value, now_ms: i64, style: Style) -> String {
    let id = text(&u["id"]);
    let o = &u["occupancy"];
    let mut t = format!("  {SUB}{id} ");
    let mut details: Vec<String> = Vec::new();
    if let Some(d) = some_text(&u["description"]) {
        details.push(d.to_owned());
    }
    match o["kind"].as_str() {
        Some("free") => {
            if let Some(why) = unhealthy_message(u) {
                let mut warning = format!("unhealthy: {why}");
                if let Some(at) = u["last_health_at"].as_i64() {
                    let _ = write!(warning, " (checked {})", render::when(at, now_ms));
                }
                let _ = write!(t, "{} {}", render::BUSY, style.warn(&warning));
            } else {
                let _ = write!(t, "{} free", render::FREE);
                if let Some(reason) = u["dirty"]["reason"].as_str() {
                    let mut warning = format!("dirty: {reason}");
                    if let Some(at) = u["dirty"]["at"].as_i64() {
                        let _ = write!(warning, " {}", render::when(at, now_ms));
                    }
                    let _ = write!(
                        warning,
                        " ({})",
                        render::holder(&u["dirty"]["previous_holder"])
                    );
                    let _ = write!(t, "{SEP}{}", style.warn(&warning));
                }
            }
        }
        Some("held") => {
            let holder = render::holder(&o["holder"]);
            let state = text(&o["state"]);
            let _ = write!(
                t,
                "{} {}",
                render::BUSY,
                match state.as_str() {
                    "held" => format!("held by {holder}"),
                    "offered" => format!("offered to {holder}"),
                    other => format!("{other}, held by {holder}"),
                }
            );
            if o["resource"] != u["resource"] || o["unit"] != u["id"] {
                // Held through a linked unit: the same physical thing.
                let _ = write!(t, " as {}", unit_ref(&o["resource"], &o["unit"], style));
            }
            if let Some(at) = o["ends_at"].as_i64() {
                let _ = write!(t, "{SEP}ends {}", render::when(at, now_ms));
            }
            let _ = write!(t, "{SEP}{}", text(&o["lease_id"]));
            if let Some(p) = some_text(&o["purpose"]) {
                details.push(render::quoted(p));
            }
        }
        _ => {
            let _ = write!(t, "{} held by agent in another project", render::BUSY);
        }
    }
    if u["retiring"] == true {
        let _ = write!(t, "{SEP}retiring");
    }
    if !details.is_empty() {
        let _ = write!(
            t,
            "\n{}",
            render::wrap(&details.join(SEP), 6, render::width())
        );
    }
    t
}

/// A queue entry: `1. pid 130150 · normal · "purpose" · req_…`.
fn queue_entry_text(q: &Value) -> String {
    let mut t = format!(
        "{}. {}{SEP}{}",
        q["position"],
        render::holder(&q["holder"]),
        text(&q["priority"])
    );
    if let Some(p) = some_text(&q["purpose"]) {
        let _ = write!(t, "{SEP}{}", render::quoted(p));
    }
    let _ = write!(t, "{SEP}{}", text(&q["request_id"]));
    if let Some(unit) = q["unit"].as_str() {
        let _ = write!(t, "{SEP}unit {unit}");
    }
    t
}

fn status_text(v: &Value) -> String {
    let now = SystemClock.now_ms();
    let style = Style::current();
    let width = render::width();
    let resources = v["resources"].as_array().cloned().unwrap_or_default();
    if resources.is_empty() {
        return "No resources.".to_owned();
    }
    let mut out = Vec::new();
    for r in &resources {
        let units: Vec<Value> = r["units"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|u| {
                let mut u = u.clone();
                u["resource"] = r["id"].clone();
                u
            })
            .collect();
        let mut t = resource_header(r, &units, true, style);
        if let Some(d) = some_text(&r["description"]) {
            let _ = write!(t, "\n{}", render::wrap(d, 2, width));
        }
        if units.is_empty() {
            t.push_str("\n  no units");
        }
        for u in &units {
            let _ = write!(t, "\n{}", unit_block(u, now, style));
        }
        let queue = r["queue"].as_array().cloned().unwrap_or_default();
        if !queue.is_empty() {
            t.push_str("\n  queue:");
            for q in &queue {
                let _ = write!(t, "\n  {SUB}{}", queue_entry_text(q));
            }
        }
        out.push(t);
    }
    out.join("\n\n")
}

/// `clerk resources`: one block per listed resource, its occupancy from
/// `status` (the same resource by id and scope).
fn resources_text(v: &Value, status: &Value) -> String {
    let style = Style::current();
    let width = render::width();
    let list = v.as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        return "No resources.".to_owned();
    }
    let mut out = Vec::new();
    for r in &list {
        let units: Vec<Value> = status["resources"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|s| s["id"] == r["id"] && s["scope"] == r["scope"])
            .and_then(|s| s["units"].as_array().cloned())
            .unwrap_or_else(|| r["units"].as_array().cloned().unwrap_or_default());
        let mut t = resource_header(r, &units, false, style);
        if let Some(d) = some_text(&r["description"]) {
            let _ = write!(t, "\n{}", render::wrap(d, 2, width));
        }
        if r["tags"].as_array().is_some_and(|t| !t.is_empty()) {
            let _ = write!(t, "\n  tags: {}", render::tags_of(&r["tags"], style));
        }
        out.push(t);
    }
    out.join("\n\n")
}

/// The health checks of a resource: its own, or the type default.
fn health_text(r: &Value) -> String {
    let names = |checks: &[Value]| {
        checks
            .iter()
            .map(|c| match (c.get("builtin"), c.get("custom")) {
                (Some(b), _) => text(b),
                (_, Some(cmd)) => format!("custom: {}", text(cmd)),
                _ => text(c),
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    if let Some(checks) = r["health"].as_array() {
        return names(checks);
    }
    if let Some(checks) = r["health"]["checks"].as_array() {
        return names(checks);
    }
    let ty = serde_json::from_value::<ResourceType>(r["type"].clone()).ok();
    let mode =
        r["units"].as_array().into_iter().flatten().find_map(|u| {
            serde_json::from_value::<ServiceMode>(u["settings"]["mode"].clone()).ok()
        });
    match ty {
        Some(ty) => {
            let defaults: Vec<&str> = HealthSpec::default_for(ty, mode)
                .checks
                .iter()
                .filter_map(|c| match c {
                    Check::Builtin(b) => Some(b.as_str()),
                    Check::Custom { .. } => None,
                })
                .collect();
            format!("type default ({})", defaults.join(", "))
        }
        None => "type default".to_owned(),
    }
}

/// A resource with everything, usage as the raw template.
pub fn resource_text(r: &Value) -> String {
    let style = Style::current();
    let width = render::width();
    let mut t = format!(
        "{}  {}{SEP}{}",
        style.id(&text(&r["id"])),
        text(&r["type"]),
        render::scope(&r["scope"])
    );
    if r["shadows_user"] == true {
        let _ = write!(t, "{SEP}hides the user resource of the same id");
    }
    if let Some(d) = some_text(&r["description"]) {
        let _ = write!(t, "\n{}", render::wrap(d, 2, width));
    }
    if r["tags"].as_array().is_some_and(|t| !t.is_empty()) {
        let _ = write!(t, "\n  tags:   {}", render::tags_of(&r["tags"], style));
    }
    let _ = write!(t, "\n  health: {}", health_text(r));
    let units = r["units"].as_array().cloned().unwrap_or_default();
    if units.is_empty() {
        t.push_str("\n  units:  none");
    } else {
        t.push_str("\n  units:");
    }
    let id_width = units
        .iter()
        .map(|u| text(&u["id"]).chars().count())
        .max()
        .unwrap_or(0);
    for u in &units {
        let _ = write!(
            t,
            "\n  {SUB}{:<id_width$}  {}",
            text(&u["id"]),
            settings_text(&u["settings"])
        );
        if let Some(reason) = u["dirty"]["reason"].as_str() {
            let _ = write!(t, "{SEP}{}", style.warn(&format!("dirty: {reason}")));
        }
        if u["retiring"] == true {
            let _ = write!(t, "{SEP}retiring");
        }
        if let Some(d) = some_text(&u["description"]) {
            let _ = write!(t, "\n{}", render::wrap(d, 4 + id_width + 2, width));
        }
    }
    if let Some(usage) = some_text(&r["usage"]) {
        let _ = write!(t, "\n  usage (template):\n{}", render::indent(usage, 4));
    }
    t
}

/// `clerk leases`: one block per lease; a lease that also blocks linked
/// units says so.
fn leases_text(v: &Value) -> String {
    let now = SystemClock.now_ms();
    let style = Style::current();
    let width = render::width();
    let list = v.as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        return "No leases.".to_owned();
    }
    let mut out = Vec::new();
    let mut seen: Vec<&Value> = Vec::new();
    for l in &list {
        let o = &l["occupancy"];
        if o["kind"] != "held" {
            out.push(format!(
                "{ITEM}{}{SEP}held by agent in another project",
                unit_ref(&l["resource"], &l["unit"], style)
            ));
            continue;
        }
        if seen.contains(&&o["lease_id"]) {
            continue;
        }
        seen.push(&o["lease_id"]);
        let mut t = format!(
            "{ITEM}{}  {}{SEP}{}",
            text(&o["lease_id"]),
            unit_ref(&o["resource"], &o["unit"], style),
            text(&o["state"])
        );
        if let Some(at) = o["ends_at"].as_i64() {
            let _ = write!(t, "{SEP}ends {}", render::when(at, now));
        }
        let _ = write!(t, "{SEP}{}", render::holder(&o["holder"]));
        if let Some(p) = some_text(&o["purpose"]) {
            let _ = write!(t, "\n{}", render::wrap(&render::quoted(p), 2, width));
        }
        let also: Vec<String> = list
            .iter()
            .filter(|x| {
                x["occupancy"]["lease_id"] == o["lease_id"]
                    && (x["resource"] != o["resource"] || x["unit"] != o["unit"])
            })
            .map(|x| unit_ref(&x["resource"], &x["unit"], style))
            .collect();
        if !also.is_empty() {
            let _ = write!(t, "\n  also blocks {} (same device)", also.join(", "));
        }
        out.push(t);
    }
    out.join("\n\n")
}

fn my_leases_text(v: &Value) -> String {
    let now = SystemClock.now_ms();
    let style = Style::current();
    let width = render::width();
    let leases = v["leases"].as_array().cloned().unwrap_or_default();
    let requests = v["requests"].as_array().cloned().unwrap_or_default();
    if leases.is_empty() && requests.is_empty() {
        return "You hold no leases and wait for nothing.".to_owned();
    }
    let mut out = Vec::new();
    for l in &leases {
        let mut t = format!(
            "{ITEM}{}  {}{SEP}{}",
            text(&l["lease_id"]),
            unit_ref(&l["resource"], &l["unit"], style),
            text(&l["state"])
        );
        if let Some(at) = l["ends_at"].as_i64() {
            let _ = write!(t, "{SEP}ends {}", render::when(at, now));
        }
        let _ = write!(t, "\n{}", brief_text(l));
        out.push(t);
    }
    for q in &requests {
        let mut t = format!(
            "{ITEM}{}  {}",
            text(&q["request_id"]),
            style.id(&text(&q["resource"]))
        );
        if let Some(unit) = q["unit"].as_str() {
            let _ = write!(t, "/{unit}");
        }
        let _ = write!(t, "{SEP}queued at position {}", q["position"]);
        if let Some(p) = some_text(&q["purpose"]) {
            let _ = write!(t, "\n{}", render::wrap(&render::quoted(p), 2, width));
        }
        out.push(t);
    }
    out.join("\n\n")
}

/// The brief of a grant or lease, indented: both descriptions, the
/// resource's tags, the unit's settings, the rendered usage and, when
/// handed over dirty, why. Empty fields are left out.
pub fn brief_text(l: &Value) -> String {
    let style = Style::current();
    let width = render::width();
    let mut lines: Vec<String> = Vec::new();
    if let Some(d) = some_text(&l["resource_description"]) {
        lines.push(render::wrap(d, 2, width));
    }
    if let Some(d) = some_text(&l["unit_description"]) {
        lines.push(render::wrap(
            &format!("{}: {d}", text(&l["unit"])),
            2,
            width,
        ));
    }
    if l["resource_tags"].as_array().is_some_and(|t| !t.is_empty()) {
        lines.push(format!(
            "  tags: {}",
            render::tags_of(&l["resource_tags"], style)
        ));
    }
    lines.push(format!("  {}", settings_line(&l["settings"])));
    if let Some(usage) = some_text(&l["usage"]) {
        lines.push(format!("  usage:\n{}", render::indent(usage, 4)));
    }
    if let Some(reason) = l["dirty"]["reason"].as_str() {
        lines.push(format!(
            "  {}",
            style.warn(&format!(
                "dirty: {} ({reason}); expect a messy state",
                dirty_explanation(reason)
            ))
        ));
    }
    lines.join("\n")
}

fn queue_text(v: &Value) -> String {
    let style = Style::current();
    let width = render::width();
    let queues = v.as_array().cloned().unwrap_or_default();
    let mut out = Vec::new();
    for r in &queues {
        let mut lines = Vec::new();
        if queues.len() > 1 {
            lines.push(format!(
                "{}{SEP}{}:",
                style.id(&text(&r["resource"])),
                render::scope(&r["scope"])
            ));
        }
        let entries = r["queue"].as_array().cloned().unwrap_or_default();
        if entries.is_empty() {
            lines.push(format!(
                "Nobody waits for {}.",
                style.id(&text(&r["resource"]))
            ));
        }
        for q in &entries {
            let mut t = format!(
                "{ITEM}{}. {}{SEP}{}{SEP}{}",
                q["position"],
                text(&q["request_id"]),
                render::holder(&q["holder"]),
                text(&q["priority"])
            );
            if let Some(unit) = q["unit"].as_str() {
                let _ = write!(t, "{SEP}unit {unit}");
            }
            if let Some(p) = some_text(&q["purpose"]) {
                let _ = write!(t, "\n{}", render::wrap(&render::quoted(p), 2, width));
            }
            lines.push(t);
        }
        out.push(lines.join("\n"));
    }
    out.join("\n\n")
}

/// What an audit entry did, in a few words: the unit or resource, who did
/// it (or to whom it went), the ids involved (shortened unless `wide`), the
/// purpose and anything else it records.
fn audit_what(e: &Value, wide: bool) -> String {
    let style = Style::current();
    let d = &e["detail"];
    let mut head = String::new();
    if let Some(r) = d["resource"].as_str() {
        head.push_str(&style.id(r));
        if let Some(u) = d["unit"].as_str() {
            let _ = write!(head, "/{u}");
        }
    }
    let actor = &e["actor"];
    let by_clerk = actor["kind"] == "user" && actor["tag"] == "clerk";
    if d["holder"].is_object() && e["action"] == "offered" {
        let _ = write!(head, " → {}", render::holder(&d["holder"]));
    } else if !by_clerk {
        if !head.is_empty() {
            head.push(' ');
        }
        let _ = write!(head, "by {}", render::holder(actor));
    }
    let mut parts = vec![head.trim().to_owned()];
    if d["holder"].is_object() && e["action"] != "offered" {
        parts.push(format!("holder {}", render::holder(&d["holder"])));
    }
    let consumed = ["resource", "unit", "holder", "purpose", "override_reason"];
    if let Value::Object(map) = d {
        for (key, value) in map {
            if consumed.contains(&key.as_str()) || value.is_null() {
                continue;
            }
            let shown = match value.as_str() {
                Some(s) if ["lease", "request", "promotion", "offer"].contains(&key.as_str()) => {
                    render::short_id(s, wide)
                }
                _ => match value {
                    Value::Bool(true) => key.replace('_', " "),
                    Value::Bool(false) => continue,
                    // A holder, or a record with a reason (dirty): its
                    // short form; any other structure is left to --json.
                    Value::Object(o) if o.contains_key("kind") => {
                        format!("{}: {}", key.replace('_', " "), render::holder(value))
                    }
                    Value::Object(o) => match o.get("reason").and_then(Value::as_str) {
                        Some(reason) => format!("{}: {reason}", key.replace('_', " ")),
                        None => continue,
                    },
                    other => format!("{}: {}", key.replace('_', " "), plain(other)),
                },
            };
            parts.push(shown);
        }
    }
    if let Some(p) = some_text(&d["purpose"]) {
        parts.push(render::quoted(p));
    }
    if let Some(reason) = e["override_reason"]
        .as_str()
        .or_else(|| d["override_reason"].as_str())
    {
        parts.push(format!("override: {}", render::quoted(reason)));
    }
    parts.retain(|p| !p.is_empty());
    parts.join(SEP)
}

fn log_text(v: &Value, now_ms: i64, wide: bool) -> String {
    let width = render::width();
    let list = v.as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        return "No log entries.".to_owned();
    }
    let rows: Vec<(String, String, String)> = list
        .iter()
        .map(|e| {
            (
                e["at"]
                    .as_i64()
                    .map_or_else(|| "-".to_owned(), |at| render::when(at, now_ms)),
                text(&e["action"]),
                audit_what(e, wide),
            )
        })
        .collect();
    let w1 = rows.iter().map(|r| r.0.chars().count()).max().unwrap_or(0);
    let w2 = rows.iter().map(|r| r.1.chars().count()).max().unwrap_or(0);
    let indent = ITEM.chars().count() + w1 + 2 + w2 + 2;
    rows.iter()
        .map(|(when, action, what)| {
            let wrapped = render::wrap(what, indent, width);
            format!("{ITEM}{when:<w1$}  {action:<w2$}  {}", wrapped.trim_start())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn promotions_text(v: &Value) -> String {
    let style = Style::current();
    let width = render::width();
    let list = v.as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        return "No pending promotions.".to_owned();
    }
    let mut out = Vec::new();
    for p in &list {
        let mut t = format!(
            "{ITEM}{}  {}{SEP}{}",
            text(&p["id"]),
            style.id(&text(&p["resource"])),
            render::scope(&p["scope"])
        );
        if let Some(d) = some_text(&p["description"]) {
            let _ = write!(t, "\n{}", render::wrap(d, 2, width));
        }
        let reason = text(&p["reason"]);
        let line = match p["requester"].as_str() {
            Some(by) => format!(
                "requested by {}: {reason}",
                render::project(&ProjectKey(by.into()))
            ),
            None => reason,
        };
        let _ = write!(t, "\n{}", render::wrap(&line, 2, width));
        out.push(t);
    }
    let footer = match list.as_slice() {
        [one] => format!("Approve with `clerk approve {}`.", text(&one["id"])),
        _ => "Approve one with `clerk approve <id>`.".to_owned(),
    };
    format!("{}\n{footer}", out.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_takes_a_span_ago() {
        assert_eq!(parse_since("2m", 1_000_000).unwrap(), 880_000);
    }

    #[test]
    fn since_takes_an_rfc3339_timestamp() {
        assert_eq!(parse_since("1970-01-01T00:00:01Z", 0).unwrap(), 1000);
        assert_eq!(
            parse_since("2026-11-10T09:00:00.25+01:00", 0).unwrap(),
            1_794_297_600_250
        );
        assert_eq!(
            parse_since("2000-02-29t00:00:00z", 0).unwrap(),
            951_782_400_000
        );
    }

    #[test]
    fn since_refuses_bare_numbers_and_negatives() {
        for bad in ["1234", "5", "-5m", "+5m", "-1"] {
            let message = parse_since(bad, 1_000_000).unwrap_err().0;
            assert!(message.contains("<n>s|m|h|d"), "{bad}: {message}");
            assert!(message.contains("RFC 3339"), "{bad}: {message}");
        }
    }

    #[test]
    fn since_refuses_anything_else() {
        for bad in [
            "soon",
            "5w",
            "",
            "m",
            "2026-13-01T00:00:00Z",
            "2026-02-29T00:00:00Z",
            "2026-11-10T24:00:00Z",
            "2026-11-10T09:00:00",
            "2026-11-10T09:00:00.Z",
            "2026-11-10T09:00:00+0100",
            "2026-11-10",
        ] {
            assert!(parse_since(bad, 0).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_free_unit_with_a_failed_check_is_not_counted_free() {
        let units = [
            json!({"occupancy": {"kind": "free"}, "last_health": "unhealthy: connect failed"}),
            json!({"occupancy": {"kind": "free"}, "last_health": "healthy"}),
        ];
        let style = Style {
            color: false,
            unicode: true,
        };
        assert_eq!(occupancy_bar(&units, style), "▮▯ 1/2 free ⚠");
    }

    #[test]
    fn audit_entries_name_the_unit_and_actor() {
        let e = json!({
            "actor": {"kind": "user", "tag": "x"},
            "action": "queued",
            "detail": {"resource": "otbr", "request": "req_01M71FTC912KWSMGKC34CHWA9K",
                       "purpose": "late", "unit": null}
        });
        assert_eq!(
            audit_what(&e, false),
            "otbr by cli · req_01M71FTC… · \"late\""
        );
    }
}
