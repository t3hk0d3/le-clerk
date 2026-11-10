//! Lease commands agents use besides the common ones: `types`, `confirm`,
//! `interruptible`, `yield --refuse`, `reclaim` and `claim`. Each acts as
//! the caller (an agent session acts as itself; the user as the lease's
//! `User` holder) and prints the daemon's answer.

use clap::ValueEnum;
use leclerk::model::{HealthSpec, ResourceType, allowed_builtins, placeholder_keys};
use serde_json::{Value, json};

use super::Ctx;
use super::lease::lease_caller;
use super::output::View;

/// `clerk confirm`'s outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum How {
    /// The unit is clean: the lease ends.
    Clean,
    /// You paused at a safe point for a yield request.
    Yielded,
}

/// `on` or `off`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OnOff {
    On,
    Off,
}

/// The settings fields of each type, as (name, required, meaning).
fn settings_fields(ty: ResourceType) -> &'static [(&'static str, bool, &'static str)] {
    match ty {
        ResourceType::Device => &[("path", true, "filesystem path of the device node")],
        ResourceType::Service => &[
            (
                "mode",
                true,
                "\"connect\" to a running server or \"listen\" on a reserved port",
            ),
            ("host", true, "host name or address"),
            ("port", true, "TCP port, not 0"),
            ("scheme", false, "URL scheme, e.g. \"http\""),
            ("service_id", false, "service-specific identifier"),
        ],
        ResourceType::Generic => &[("identity_key", true, "a key naming the thing")],
    }
}

/// `clerk types`: the built-in types, their settings, health checks and
/// usage placeholders.
///
/// # Errors
/// When stdout cannot be written.
pub fn types(ctx: &Ctx) -> anyhow::Result<()> {
    let types: Vec<Value> = [
        (
            ResourceType::Device,
            "a device node, e.g. a USB serial board",
        ),
        (
            ResourceType::Service,
            "a network service to connect to or listen as",
        ),
        (ResourceType::Generic, "anything else, named by a key"),
    ]
    .into_iter()
    .map(|(ty, description)| {
        let mut checks: Vec<&str> = allowed_builtins(ty).iter().map(|c| c.as_str()).collect();
        checks.push("custom");
        let fields: Vec<Value> = settings_fields(ty)
            .iter()
            .map(|(name, required, meaning)| {
                json!({"name": name, "required": required, "description": meaning})
            })
            .collect();
        json!({
            "type": ty,
            "description": description,
            "settings": fields,
            "health_checks": checks,
            "default_health": HealthSpec::default_for(ty, None),
            "usage_placeholders": placeholder_keys(ty),
        })
    })
    .collect();
    ctx.out.emit(&View::new(json!({ "types": types }), |v| {
        v["types"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|t| {
                format!(
                    "{}: {}",
                    super::output::plain(&t["type"]),
                    super::output::plain(&t["description"])
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }))
}

/// Call `method` on `lease` as its holder with `extra` arguments and print
/// the answer with `text`.
async fn on_lease(
    ctx: &Ctx,
    method: &str,
    lease: &str,
    extra: Value,
    text: fn(&Value) -> String,
) -> anyhow::Result<()> {
    let client = ctx.client().await?;
    let caller = lease_caller(ctx, &client, lease).await?;
    let mut args = json!({ "lease_id": lease });
    if let (Some(a), Value::Object(e)) = (args.as_object_mut(), extra) {
        a.extend(e);
    }
    let mut view = client.call(method, &caller, args).await?;
    if view.is_null() {
        // Some commands answer nothing: show the lease as it is now.
        view = client
            .call("get_lease", &caller, json!({ "lease_id": lease }))
            .await?;
    }
    ctx.out.emit(&View::new(view, text))
}

fn state_text(v: &Value) -> String {
    format!(
        "Lease {} is {}.",
        super::output::plain(&v["lease_id"]),
        super::output::plain(&v["state"])
    )
}

/// `clerk confirm <lease> clean|yielded`.
///
/// # Errors
/// The daemon's errors.
pub async fn confirm(ctx: &Ctx, lease: &str, how: How) -> anyhow::Result<()> {
    let how = match how {
        How::Clean => "clean",
        How::Yielded => "yielded",
    };
    on_lease(ctx, "confirm", lease, json!({ "how": how }), state_text).await
}

/// `clerk interruptible <lease> on|off`.
///
/// # Errors
/// The daemon's errors.
pub async fn interruptible(ctx: &Ctx, lease: &str, state: OnOff) -> anyhow::Result<()> {
    on_lease(
        ctx,
        "set_interruptible",
        lease,
        json!({ "interruptible": state == OnOff::On }),
        |v| {
            format!(
                "Lease {} is {}interruptible.",
                super::output::plain(&v["lease_id"]),
                if v["interruptible"] == true {
                    ""
                } else {
                    "not "
                }
            )
        },
    )
    .await
}

/// `clerk yield <lease> --refuse`.
///
/// # Errors
/// The daemon's errors.
pub async fn refuse_yield(ctx: &Ctx, lease: &str) -> anyhow::Result<()> {
    on_lease(ctx, "refuse_yield", lease, json!({}), state_text).await
}

/// `clerk reclaim <lease>`.
///
/// # Errors
/// The daemon's errors.
pub async fn reclaim(ctx: &Ctx, lease: &str) -> anyhow::Result<()> {
    on_lease(ctx, "reclaim", lease, json!({}), state_text).await
}

/// `clerk claim <lease>`.
///
/// # Errors
/// The daemon's errors.
pub async fn claim(ctx: &Ctx, lease: &str) -> anyhow::Result<()> {
    on_lease(ctx, "claim", lease, json!({}), state_text).await
}
