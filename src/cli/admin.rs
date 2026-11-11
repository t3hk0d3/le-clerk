//! `clerk admin …`: the hard management tools, each mapped 1:1 to an `admin_*` RPC
//! method.
//!
//! The gate runs before any daemon call: agents need `--override`, and a
//! state-changing subcommand also needs the person's confirmation (a prompt
//! on a terminal, `--yes` otherwise). `export` and `doctor` without `--fix`
//! change nothing, so they skip the confirmation; agents still need
//! `--override` for them, since they cover every project. Nothing here
//! reads stdin except the gate's terminal prompt. Files named on the
//! command line (`import --in`) are read before the gate, which they do
//! not need; `export --out` writes a new 0600 file (`--force` overwrites).

use std::fmt::Write as _;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use leclerk::admin::{HolderSpec, ScopeSpec};
use leclerk::client::ApiError;
use leclerk::clock::{Clock, SystemClock};
use leclerk::model::Scope;
use leclerk::scope::ProjectResolver;
use serde_json::{Map, Value, json};

use super::caller::CallerKind;
use super::inventory::warnings;
use super::lease::{PriorityArg, parse_duration};
use super::output::{View, table};
use super::views::{scope_text, text};
use super::{Ctx, UsageError, gate};

/// `clerk admin`.
#[derive(Args)]
pub struct AdminArgs {
    /// Confirm without asking (needed when there is no terminal).
    #[arg(long, global = true)]
    yes: bool,
    #[command(subcommand)]
    action: AdminAction,
}

/// A `--scope` value: `user`, `project` (the project of the working
/// directory) or `project:<path>`.
///
/// # Errors
/// A message naming the accepted forms.
pub fn parse_scope(s: &str) -> Result<ScopeSpec, String> {
    if s == "project" {
        let cwd = std::env::current_dir()
            .map_err(|e| format!("cannot tell this directory's project: {e}"))?;
        let key = ProjectResolver::resolve(&cwd).map_err(|e| e.to_string())?;
        return Ok(ScopeSpec::from(&Scope::Project(key)));
    }
    ScopeSpec::parse(s).map_err(|_| {
        format!(
            "{s:?} is not a scope: use user, project (this directory's project) or \
             project:<path>"
        )
    })
}

/// Where a resource lives, for ids that exist in more than one scope.
#[derive(Args)]
pub struct ScopeFlag {
    /// `user`, `project` (this directory's project) or `project:<path>`;
    /// default: your project, then user scope.
    #[arg(long, value_parser = parse_scope, value_name = "SCOPE")]
    pub scope: Option<ScopeSpec>,
}

impl ScopeFlag {
    /// Add the scope, when one is given, to RPC arguments `args`.
    pub fn put(&self, args: &mut Value) {
        if let Some(scope) = &self.scope {
            args["scope"] = json!(scope.to_string());
        }
    }
}

#[derive(Subcommand)]
pub enum AdminAction {
    /// End a lease now; its unit is marked dirty.
    Release {
        /// The lease id.
        lease: String,
    },
    /// End every lease and queued request of a holder.
    Evict {
        /// `session:<id>`, `pid:<pid>` or `user:<tag>`.
        #[arg(value_parser = HolderSpec::parse)]
        holder: HolderSpec,
    },
    /// Remove one queued request.
    Dequeue {
        /// The request id, from `clerk queue`.
        request: String,
    },
    /// Remove every queued request of a resource.
    ClearQueue {
        /// The resource id.
        resource: String,
        #[command(flatten)]
        scope: ScopeFlag,
    },
    /// Remove a unit even if it is held or queued for.
    RemoveUnit {
        /// `<resource>/<unit>`.
        unit: String,
        #[command(flatten)]
        scope: ScopeFlag,
    },
    /// Delete a resource even if its units are held or queued for.
    DeleteResource {
        /// The resource id.
        resource: String,
        #[command(flatten)]
        scope: ScopeFlag,
    },
    /// Clear a unit's dirty flag.
    Clean {
        /// `<resource>/<unit>`.
        unit: String,
        #[command(flatten)]
        scope: ScopeFlag,
    },
    /// Link two units as one physical thing: a lease on either blocks the
    /// other.
    Link(LinkArgs),
    /// Unlink two units and keep them apart.
    Unlink(LinkArgs),
    /// Move a resource to another scope.
    Move {
        /// The resource id.
        resource: String,
        /// The scope to move it to: `user`, `project` (this directory's project) or
        /// `project:<path>`.
        #[arg(long, value_parser = parse_scope, value_name = "SCOPE")]
        to: ScopeSpec,
        /// Its current scope, when the id exists in several.
        #[arg(long = "from", value_parser = parse_scope, value_name = "SCOPE")]
        from: Option<ScopeSpec>,
    },
    /// Rename a resource, or with --unit one of its units.
    Rename {
        /// The resource id.
        resource: String,
        /// The new id.
        new_id: String,
        /// Rename this unit of the resource instead.
        #[arg(long)]
        unit: Option<String>,
        #[command(flatten)]
        scope: ScopeFlag,
    },
    /// Change a lease's end, priority or interruptibility.
    EditLease {
        /// The lease id.
        lease: String,
        /// Move the end to now plus this: <n>s|m|h|d, up to 30 days.
        #[arg(
            long = "for",
            value_name = "DURATION",
            value_parser = parse_duration,
            allow_hyphen_values = true
        )]
        duration: Option<std::time::Duration>,
        /// The new queue priority.
        #[arg(long, value_enum)]
        priority: Option<PriorityArg>,
        /// Open (`true`) or close (`false`) the lease to interruption.
        #[arg(long, value_name = "BOOL")]
        interruptible: Option<bool>,
    },
    /// Look for damage; with --fix, repair what can be repaired.
    Doctor {
        /// Repair what can be repaired (findings that cannot are reported).
        #[arg(long)]
        fix: bool,
    },
    /// Print the whole inventory as JSON, or write it to --out.
    Export {
        /// Write the document to this new file (mode 0600) instead of
        /// stdout.
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
        /// Overwrite --out when it already exists.
        #[arg(long, requires = "out")]
        force: bool,
    },
    /// Load an inventory document made by `export`.
    Import {
        /// The document to read.
        #[arg(long = "in", value_name = "FILE")]
        input: PathBuf,
        /// Overwrite resources that already exist.
        #[arg(long)]
        replace: bool,
    },
}

/// The two units of `link` and `unlink`.
#[derive(Args)]
pub struct LinkArgs {
    /// `<resource>/<unit>`.
    a: String,
    /// `<resource>/<unit>`.
    b: String,
    /// The scope of the first unit's resource.
    #[arg(long, value_parser = parse_scope, value_name = "SCOPE")]
    scope_a: Option<ScopeSpec>,
    /// The scope of the second unit's resource.
    #[arg(long, value_parser = parse_scope, value_name = "SCOPE")]
    scope_b: Option<ScopeSpec>,
}

/// What an admin subcommand does, ready to run behind the gate.
struct Plan {
    /// A plain sentence for the gate: what will happen.
    effect: String,
    /// The daemon method.
    method: &'static str,
    args: Value,
    /// False when nothing changes: only the override rule applies.
    changes_state: bool,
    shape: Shape,
}

/// How the daemon's answer reads to a person.
enum Shape {
    /// The call answers nothing; this sentence says what happened.
    Done(String),
    Evicted,
    Dropped,
    Linked(&'static str),
    Resource,
    Lease,
    Doctor,
    Import,
    /// Writes to this file (overwriting it when the flag says so), or
    /// prints without one.
    Export(Option<PathBuf>, bool),
}

pub async fn run(ctx: &Ctx, args: AdminArgs) -> anyhow::Result<()> {
    let plan = plan(args.action)?;
    let caller = ctx.caller(CallerKind::Detached)?;
    // The gate comes first: nothing reaches the daemon on a refusal.
    if plan.changes_state {
        gate::require(&plan.effect, args.yes, &caller)?;
    } else {
        gate::require_override(&plan.effect, &caller)?;
    }
    let result = ctx
        .client()
        .await?
        .call(plan.method, &caller, plan.args)
        .await?;
    finish(ctx, plan.shape, &result)
}

/// `clerk release --force <lease>`: [`AdminAction::Release`] under another
/// name, behind the same gate.
///
/// # Errors
/// As [`run`].
pub async fn force_release(ctx: &Ctx, lease: String, yes: bool) -> anyhow::Result<()> {
    run(
        ctx,
        AdminArgs {
            yes,
            action: AdminAction::Release { lease },
        },
    )
    .await
}

/// A state-changing [`Plan`], wrapped for `?`-free arms.
#[allow(clippy::unnecessary_wraps, reason = "arms of `plan` return a Result")]
fn change(effect: String, method: &'static str, args: Value, shape: Shape) -> anyhow::Result<Plan> {
    Ok(Plan {
        effect,
        method,
        args,
        changes_state: true,
        shape,
    })
}

/// Turn the command line into a [`Plan`]; nothing is called yet.
///
/// # Errors
/// [`UsageError`] for a target or edit that makes no sense.
// One arm per subcommand; splitting them would only scatter the mapping.
#[allow(clippy::too_many_lines, reason = "one flat match over the subcommands")]
fn plan(action: AdminAction) -> anyhow::Result<Plan> {
    match action {
        AdminAction::Release { lease } => change(
            format!("end lease {lease} now and mark its unit dirty"),
            "admin_release",
            json!({ "lease_id": lease }),
            Shape::Done(format!(
                "Released: lease {lease} has ended and its unit is dirty."
            )),
        ),
        AdminAction::Evict { holder } => change(
            format!("end every lease and queued request of {holder} and mark their units dirty"),
            "admin_evict",
            json!({ "holder": holder.to_string() }),
            Shape::Evicted,
        ),
        AdminAction::Dequeue { request } => change(
            format!("remove queued request {request}"),
            "admin_dequeue",
            json!({ "request_id": request }),
            Shape::Done(format!("Removed request {request} from its queue.")),
        ),
        AdminAction::ClearQueue { resource, scope } => change(
            format!("remove every queued request of resource {resource}"),
            "admin_clear_queue",
            resource_ref(&resource, scope.scope.as_ref()),
            Shape::Dropped,
        ),
        AdminAction::RemoveUnit { unit, scope } => {
            let (args, name) = unit_ref(&unit, scope.scope.as_ref())?;
            change(
                format!("remove unit {name} even if it is held or queued for"),
                "admin_remove_unit",
                args,
                Shape::Done(format!("Removed unit {name}.")),
            )
        }
        AdminAction::DeleteResource { resource, scope } => change(
            format!("delete resource {resource} and its units even if they are held or queued for"),
            "admin_delete_resource",
            resource_ref(&resource, scope.scope.as_ref()),
            Shape::Done(format!("Deleted resource {resource} and its units.")),
        ),
        AdminAction::Clean { unit, scope } => {
            let (args, name) = unit_ref(&unit, scope.scope.as_ref())?;
            change(
                format!("clear the dirty flag of unit {name} without cleaning it"),
                "admin_clean",
                args,
                Shape::Done(format!("Cleaned: unit {name} is no longer marked dirty.")),
            )
        }
        AdminAction::Link(link) => link_plan(&link, "admin_link", "link", "linked"),
        AdminAction::Unlink(link) => link_plan(&link, "admin_unlink", "unlink", "unlinked"),
        AdminAction::Move { resource, to, from } => {
            let mut args = resource_ref(&resource, from.as_ref());
            args["to"] = json!(to.to_string());
            change(
                format!("move resource {resource} to scope {to}"),
                "admin_move",
                args,
                Shape::Resource,
            )
        }
        AdminAction::Rename {
            resource,
            new_id,
            unit,
            scope,
        } => {
            let mut args = resource_ref(&resource, scope.scope.as_ref());
            args["new_id"] = json!(new_id);
            let effect = match &unit {
                Some(u) => format!("rename unit {resource}/{u} to {new_id}"),
                None => format!("rename resource {resource} to {new_id}"),
            };
            if let Some(u) = unit {
                args["unit"] = json!(u);
            }
            change(effect, "admin_rename", args, Shape::Resource)
        }
        AdminAction::EditLease {
            lease,
            duration,
            priority,
            interruptible,
        } => edit_lease_plan(&lease, duration, priority, interruptible),
        AdminAction::Doctor { fix } => Ok(Plan {
            effect: if fix {
                "let the doctor repair the damage it finds".to_owned()
            } else {
                "run the doctor, which covers every project".to_owned()
            },
            method: "admin_doctor",
            args: json!({ "fix": fix }),
            changes_state: fix,
            shape: Shape::Doctor,
        }),
        AdminAction::Export { out, force } => {
            if let Some(path) = &out
                && !force
                && path.exists()
            {
                return Err(file_exists(path).into());
            }
            Ok(Plan {
                effect: "export the whole inventory, which covers every project".to_owned(),
                method: "admin_export",
                args: json!({}),
                changes_state: false,
                shape: Shape::Export(out, force),
            })
        }
        AdminAction::Import { input, replace } => {
            let how = if replace {
                ", replacing resources that already exist"
            } else {
                ""
            };
            // Reading the named file changes nothing, so it comes before
            // the gate: a bad file fails before anyone is asked.
            let doc = read_document(&input)?;
            change(
                format!("import the inventory in {}{how}", input.display()),
                "admin_import",
                json!({ "replace": replace, "doc": doc }),
                Shape::Import,
            )
        }
    }
}

fn resource_ref(resource: &str, scope: Option<&ScopeSpec>) -> Value {
    let mut args = json!({ "resource": resource });
    if let Some(scope) = scope {
        args["scope"] = json!(scope.to_string());
    }
    args
}

/// `{"resource", "unit", "scope"?}` and `resource/unit` from `r/u`.
fn unit_ref(target: &str, scope: Option<&ScopeSpec>) -> Result<(Value, String), UsageError> {
    match target.split_once('/') {
        Some((r, u)) if !r.is_empty() && !u.is_empty() && !u.contains('/') => {
            let mut args = resource_ref(r, scope);
            args["unit"] = json!(u);
            Ok((args, target.to_owned()))
        }
        _ => Err(UsageError(format!(
            "{target:?} is not a unit: use <resource>/<unit>"
        ))),
    }
}

fn link_plan(
    link: &LinkArgs,
    method: &'static str,
    verb: &str,
    past: &'static str,
) -> anyhow::Result<Plan> {
    let (a, a_name) = unit_ref(&link.a, link.scope_a.as_ref())?;
    let (b, b_name) = unit_ref(&link.b, link.scope_b.as_ref())?;
    let effect = if verb == "link" {
        format!("link units {a_name} and {b_name} as one physical thing")
    } else {
        format!("unlink units {a_name} and {b_name} and keep them apart")
    };
    Ok(Plan {
        effect,
        method,
        args: json!({ "a": a, "b": b }),
        changes_state: true,
        shape: Shape::Linked(past),
    })
}

fn edit_lease_plan(
    lease: &str,
    duration: Option<std::time::Duration>,
    priority: Option<PriorityArg>,
    interruptible: Option<bool>,
) -> anyhow::Result<Plan> {
    if duration.is_none() && priority.is_none() && interruptible.is_none() {
        return Err(UsageError(format!(
            "Nothing to change on lease {lease}: give --for, --priority or --interruptible."
        ))
        .into());
    }
    let mut args = Map::new();
    args.insert("lease_id".into(), json!(lease));
    let mut changes = Vec::new();
    if let Some(d) = duration {
        let millis = i64::try_from(d.as_millis()).unwrap_or(i64::MAX);
        args.insert(
            "ends_at".into(),
            json!(SystemClock.now_ms().saturating_add(millis)),
        );
        changes.push(format!("end it {} from now", humanize(d)));
    }
    if let Some(p) = priority {
        args.insert("priority".into(), json!(p.as_str()));
        changes.push(format!("set its priority to {}", p.as_str()));
    }
    if let Some(i) = interruptible {
        args.insert("interruptible".into(), json!(i));
        changes.push(
            if i {
                "open it to interruption"
            } else {
                "close it to interruption"
            }
            .to_owned(),
        );
    }
    Ok(Plan {
        effect: format!("change lease {lease}: {}", changes.join(", ")),
        method: "admin_edit_lease",
        args: Value::Object(args),
        changes_state: true,
        shape: Shape::Lease,
    })
}

/// `90s`, `5m`, `2h` or `1d`, whichever divides evenly, else seconds.
fn humanize(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    for (per, unit) in [(86_400, "d"), (3600, "h"), (60, "m")] {
        if secs.is_multiple_of(per) {
            return format!("{}{unit}", secs / per);
        }
    }
    format!("{secs}s")
}

/// The JSON document in `file`.
///
/// # Errors
/// [`UsageError`] naming the file when it cannot be read or is not JSON.
fn read_document(path: &Path) -> anyhow::Result<Value> {
    let bytes = std::fs::read(path).map_err(|e| {
        UsageError(format!(
            "Cannot read {}: {e}. Check the path given to --in.",
            path.display()
        ))
    })?;
    serde_json::from_slice(&bytes).map_err(|e| {
        UsageError(format!(
            "{} is not a JSON inventory document: {e}. Use a file made by `clerk admin export`.",
            path.display()
        ))
        .into()
    })
}

/// `already_exists` (exit 4) for an `--out` file that is there already.
fn file_exists(path: &Path) -> ApiError {
    ApiError {
        code: "already_exists".to_owned(),
        message: format!(
            "{} already exists; nothing was written. Pick another --out, or add --force to \
             overwrite it.",
            path.display()
        ),
        details: None,
    }
}

/// Write `body` to `path`, readable by its owner only (0600): a new file,
/// or with `force` the existing one, replaced.
fn write_private(path: &Path, body: &str, force: bool) -> anyhow::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).mode(0o600);
    if force {
        options.create(true).truncate(true);
    } else {
        options.create_new(true);
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(file_exists(path).into());
        }
        Err(e) => return Err(cannot_write(path, &e).into()),
    };
    // An overwritten file keeps its old mode unless it is set again.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .and_then(|()| file.write_all(body.as_bytes()))
        .map_err(|e| cannot_write(path, &e))?;
    Ok(())
}

fn cannot_write(path: &Path, e: &std::io::Error) -> UsageError {
    UsageError(format!(
        "Cannot write {}: {e}. Check the path given to --out.",
        path.display()
    ))
}

/// Print the daemon's answer (or, for `export --out`, write it).
fn finish(ctx: &Ctx, shape: Shape, result: &Value) -> anyhow::Result<()> {
    if let Shape::Export(Some(path), force) = &shape {
        let mut body = serde_json::to_string_pretty(result)?;
        body.push('\n');
        write_private(path, &body, *force)?;
        let said = json!({ "written": path, "exported": count(&result["resources"]) });
        let path = path.display().to_string();
        return ctx.out.emit(&View::new(said, move |v| {
            format!("Exported {} resource(s) to {path}.", text(&v["exported"]))
        }));
    }
    let value = if result.is_null() {
        json!({ "ok": true })
    } else {
        result.clone()
    };
    ctx.out.emit(&View::new(value, move |v| render(&shape, v)))
}

fn render(shape: &Shape, v: &Value) -> String {
    match shape {
        Shape::Done(sentence) => sentence.clone(),
        Shape::Evicted => format!(
            "Evicted: {} lease(s) ended and {} request(s) dropped.",
            count(&v["leases_ended"]),
            count(&v["requests_dropped"])
        ),
        Shape::Dropped => format!(
            "Cleared the queue: {} request(s) dropped.",
            count(&v["requests_dropped"])
        ),
        Shape::Linked(past) => {
            let live = count(&v["live_leases"]);
            let mut sentence = format!("The units are {past}.");
            if live > 0 {
                let _ = write!(sentence, " {live} live lease(s) are affected.");
            }
            sentence
        }
        Shape::Resource => format!(
            "Done: resource {} is now in scope {}.",
            text(&v["id"]),
            scope_text(&v["scope"])
        ),
        Shape::Lease => format!(
            "Edited: lease {} now ends at {} with priority {}.",
            text(&v["lease_id"]),
            text(&v["ends_at"]),
            text(&v["priority"])
        ),
        Shape::Doctor => doctor_text(v),
        Shape::Import => import_text(v),
        Shape::Export(..) => serde_json::to_string_pretty(v).unwrap_or_default(),
    }
}

fn count(list: &Value) -> usize {
    list.as_array().map_or(0, Vec::len)
}

fn doctor_text(v: &Value) -> String {
    let findings = v["findings"].as_array().cloned().unwrap_or_default();
    if findings.is_empty() {
        return "The doctor found nothing wrong.".to_owned();
    }
    let rows: Vec<Vec<String>> = findings
        .iter()
        .map(|f| {
            vec![
                if f["fixed"] == true {
                    "fixed"
                } else {
                    "not fixed"
                }
                .to_owned(),
                text(&f["kind"]),
                text(&f["subject"]),
            ]
        })
        .collect();
    format!(
        "The doctor found {} problem(s):\n{}",
        findings.len(),
        table(&rows)
    )
}

fn import_text(v: &Value) -> String {
    let mut out = format!("Imported {} resource(s)", count(&v["imported"]));
    if let Some(list) = v["imported"].as_array().filter(|l| !l.is_empty()) {
        let _ = write!(out, ": {}", text(&Value::Array(list.clone())));
    }
    out.push('.');
    if count(&v["replaced"]) > 0 {
        let _ = write!(out, " Replaced: {}.", text(&v["replaced"]));
    }
    out.push_str(&warnings(&v["warnings"]));
    out
}
