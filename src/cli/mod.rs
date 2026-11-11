//! The `clerk` command line: argument parsing, dispatch and exit codes.
//!
//! Every command prints human text when stdout is a terminal and JSON
//! otherwise (or with `--json`). Nothing but command output reaches stdout;
//! logs go to stderr. Stdin is never read unless it is a terminal, except
//! for `-f -` on the inventory commands and the hook input of `clerk hook`.

mod admin;
mod agent;
mod caller;
mod gate;
mod hook;
mod input;
mod inventory;
mod lease;
mod live;
mod md;
mod next;
mod output;
mod render;
mod views;
mod wait;

use std::process::ExitCode;

use clap::builder::StyledStr;
use clap::error::{ContextKind, ContextValue};
use clap::{CommandFactory, Parser, Subcommand};
use leclerk::client::{self, Client, DaemonError, DaemonInfo};
use leclerk::config::{Config, Paths};
use leclerk::daemon;
use leclerk::model::Caller;
use serde_json::{Value, json};
use thiserror::Error;

use self::caller::CallerKind;
use self::output::{Exit, Failure, Mode, Output, View};

/// A command line that parses but does not make sense; exit 2, nothing
/// reaches the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{0}")]
pub struct UsageError(pub String);

/// Le Clerk: a local resource-lease broker for AI coding agents.
#[derive(Parser)]
#[command(name = "clerk", version = leclerk::VERSION, arg_required_else_help = true)]
struct Cli {
    /// Print JSON, even on a terminal.
    #[arg(long, global = true, help_heading = "Global options")]
    json: bool,
    /// Print only ASCII: `#`, `.` and `!` for the unit glyphs (also the
    /// default when the locale is not UTF-8).
    #[arg(long, global = true, help_heading = "Global options")]
    ascii: bool,
    /// Why an agent does this for the user (the reason is recorded in the
    /// audit log). Agents need it for `approve`, `promotions`, `admin`,
    /// `log`, `release --force`, `acquire` of a unit another holder has (it
    /// takes the unit from them) and `daemon stop`.
    #[arg(
        long = "override",
        global = true,
        value_name = "REASON",
        help_heading = "Global options"
    )]
    override_reason: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon in the foreground, or manage the running one.
    Daemon {
        #[command(subcommand)]
        action: Option<DaemonAction>,
    },
    /// Create, change, delete, list or show resources.
    Resource {
        #[command(subcommand)]
        action: inventory::ResourceAction,
    },
    /// Add, change, remove or retire units of a resource.
    Unit {
        #[command(subcommand)]
        action: inventory::UnitAction,
    },
    /// Units, holders, queues, health and dirty flags.
    Status,
    /// List resources, optionally only those carrying every given tag.
    Resources(views::TagFilter),
    /// Live leases (or with --mine, this agent session's leases).
    Leases {
        /// Only this agent session's leases, each with its brief.
        #[arg(long)]
        mine: bool,
    },
    /// The requests queued on a resource.
    Queue {
        /// The resource id.
        resource: String,
    },
    /// The audit log, newest first.
    Log(views::LogArgs),
    /// Pending promotions of project resources to user scope.
    Promotions,
    /// Ask for a unit and hold it. On a terminal: a live view (Ctrl+C
    /// releases, Ctrl+] detaches). Otherwise: print the grant, or the queued
    /// request and exit 6 (or with --wait, wait for the grant).
    Acquire(lease::AcquireArgs),
    /// Re-attach the live view to a lease (Ctrl+C releases, Ctrl+]
    /// detaches again); without a terminal, print its brief. An offered
    /// lease is claimed first.
    Attach {
        /// The lease id.
        lease: String,
    },
    /// Withdraw a queued request, or the offer it is waiting to claim.
    Cancel {
        /// The request id.
        request: String,
    },
    /// Give a lease back: its cleanup is confirmed at once. With --force,
    /// end anyone's lease now with its unit dirty (`clerk admin release`).
    Release {
        /// The lease id.
        lease: String,
        /// End the lease whoever holds it, as `clerk admin release` does;
        /// asks for confirmation (or --yes), and agents need --override.
        #[arg(long)]
        force: bool,
        /// With --force: confirm without asking.
        #[arg(long, requires = "force")]
        yes: bool,
        /// Start the cleanup window instead of confirming cleanup at once;
        /// `clerk confirm <lease> clean` ends it.
        #[arg(long, conflicts_with = "force")]
        cleanup: bool,
    },
    /// The resource types, their settings, health checks and usage
    /// placeholders.
    Types,
    /// Confirm a lease's cleanup (`clean`), or that you paused at a safe
    /// point for a yield request (`yielded`).
    Confirm {
        /// The lease id.
        lease: String,
        /// clean or yielded.
        #[arg(value_enum)]
        how: agent::How,
    },
    /// Open (`on`) or close (`off`) a lease to interruption by more urgent
    /// requests (agent sessions).
    Interruptible {
        /// The lease id.
        lease: String,
        /// on or off.
        #[arg(value_enum)]
        state: agent::OnOff,
    },
    /// Answer a yield request on a lease: --refuse turns it down.
    Yield {
        /// The lease id.
        lease: String,
        /// Refuse to yield.
        #[arg(long, required = true)]
        refuse: bool,
    },
    /// Take a lent unit back from a borrower that overran.
    Reclaim {
        /// The lease id.
        lease: String,
    },
    /// Claim an offered lease.
    Claim {
        /// The lease id.
        lease: String,
    },
    /// Move a lease's end to now plus --for.
    Extend(lease::ExtendArgs),
    /// Hold a unit while a command runs; the unit's settings are in its
    /// environment (`CLERK_LEASE_ID`, `CLERK_RESOURCE`, `CLERK_UNIT`,
    /// `CLERK_UNIT_<SETTING>`).
    Run(lease::RunArgs),
    /// Print your notices as they arrive.
    Wait(lease::WaitArgs),
    /// Hard management tools: forced releases, evictions, queue surgery,
    /// repairs, export and import. Each asks for confirmation (or
    /// `--yes`); agents also need `--override`.
    Admin(admin::AdminArgs),
    /// A Claude Code hook: reads the hook input from stdin, never starts
    /// the daemon, always exits 0 and prints only the hook's output.
    Hook {
        /// The hook event.
        #[arg(value_enum)]
        event: hook::Event,
    },
    /// Approve a promotion: the resource moves to user scope.
    Approve {
        /// The promotion id, from `clerk promotions`.
        promotion: String,
        /// Promote under this id instead of the resource's own.
        #[arg(long = "as", value_name = "ID")]
        new_id: Option<String>,
    },
}

impl Command {
    /// The command name, as typed: the top-level one, with the subcommand
    /// for `resource` and `unit` (`unit retire`).
    fn name(&self) -> &'static str {
        match self {
            Self::Daemon { .. } => "daemon",
            Self::Resource { action } => action.name(),
            Self::Unit { action } => action.name(),
            Self::Status => "status",
            Self::Resources(_) => "resources",
            Self::Leases { .. } => "leases",
            Self::Queue { .. } => "queue",
            Self::Log(_) => "log",
            Self::Promotions => "promotions",
            Self::Acquire(_) => "acquire",
            Self::Attach { .. } => "attach",
            Self::Cancel { .. } => "cancel",
            Self::Release { .. } => "release",
            Self::Extend(_) => "extend",
            Self::Run(_) => "run",
            Self::Wait(_) => "wait",
            Self::Admin(_) => "admin",
            Self::Hook { .. } => "hook",
            Self::Approve { .. } => "approve",
            Self::Types => "types",
            Self::Confirm { .. } => "confirm",
            Self::Interruptible { .. } => "interruptible",
            Self::Yield { .. } => "yield",
            Self::Reclaim { .. } => "reclaim",
            Self::Claim { .. } => "claim",
        }
    }
}

#[derive(Subcommand, Clone, Copy)]
enum DaemonAction {
    /// Show whether the daemon runs, with its pid and version.
    Status,
    /// Stop the running daemon (an agent needs --override; `restart` it
    /// without).
    Stop,
    /// Stop the running daemon and start a fresh one.
    Restart,
}

/// What every command handler gets.
pub struct Ctx {
    pub paths: Paths,
    pub out: Output,
    override_reason: Option<String>,
}

impl Ctx {
    /// The caller this process acts as.
    ///
    /// # Errors
    /// As [`caller::current`].
    pub fn caller(&self, kind: CallerKind) -> anyhow::Result<Caller> {
        caller::current(kind, self.override_reason.clone())
    }

    /// A client of the running daemon, started when needed.
    ///
    /// # Errors
    /// [`DaemonError`] when no daemon of this version answers.
    pub async fn client(&self) -> Result<Client, DaemonError> {
        // An agent session reports the process running it with every call.
        let host = std::env::var_os(caller::SESSION_VAR).and(caller::host());
        Ok(Client::ensure_running(&self.paths).await?.with_host(host))
    }

    /// [`Ctx::client`] for a command that only waits: a daemon that was
    /// stopped deliberately stays stopped.
    ///
    /// # Errors
    /// [`DaemonError::Stopped`] then; otherwise as [`Ctx::client`].
    pub async fn waiting_client(&self) -> Result<Client, DaemonError> {
        let host = std::env::var_os(caller::SESSION_VAR).and(caller::host());
        Ok(Client::ensure_running_unless_stopped(&self.paths)
            .await?
            .with_host(host))
    }
}

/// Parse the command line, run the command and return its exit code.
pub fn run() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        // Hooks fail open and silent, parse errors included.
        Err(_) if std::env::args_os().nth(1).is_some_and(|a| a == "hook") => {
            return ExitCode::SUCCESS;
        }
        Err(e) => return parse_failure(&with_suggestions(e)),
    };
    render::install(cli.json, cli.ascii);
    let mut out = Output::new(Mode::detect(cli.json));
    if std::env::var_os(caller::SESSION_VAR).is_some_and(|v| !v.is_empty()) {
        out = out.for_agent(cli.command.name());
        // Agents read Markdown unless asked for JSON (`--json`, or
        // CLERK_AGENT_FORMAT=json); a terminal keeps the person view.
        // CLERK_AGENT_FORMAT=human gives the person view (no colour off a
        // terminal), e.g. to compare formats.
        let format = std::env::var("CLERK_AGENT_FORMAT").unwrap_or_default();
        match format.trim() {
            _ if cli.json => {}
            "json" => {}
            "human" => out = Output::new(Mode::Human).for_agent(cli.command.name()),
            _ => out = out.markdown(),
        }
    }
    if let Command::Daemon { action: None } = cli.command {
        // The daemon logs to its own file.
        return match run_daemon() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => out.fail(Failure::from(e)),
        };
    }
    if let Command::Hook { event } = cli.command {
        // Hooks fail open and silent: no logging, nothing but hook output.
        return hook::main(event);
    }
    init_logging();
    match execute(cli, out) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => match e.downcast::<Exit>() {
            Ok(Exit(code)) => ExitCode::from(code),
            Err(e) => out.fail(Failure::from(e)),
        },
    }
}

/// `e` with the suggestions clap lacks for the frequent mistakes: the
/// closest command first for a mistyped one (`aquire`), and `--for` for a
/// bare duration after `extend <lease>`.
fn with_suggestions(mut e: clap::Error) -> clap::Error {
    use clap::error::ErrorKind;
    let usage = match e.get(ContextKind::Usage) {
        Some(ContextValue::StyledStr(u)) => u.to_string(),
        _ => String::new(),
    };
    let invalid = |e: &clap::Error, kind| match e.get(kind) {
        Some(ContextValue::String(v)) => Some(v.clone()),
        _ => None,
    };
    match e.kind() {
        ErrorKind::InvalidSubcommand if usage.contains("clerk [OPTIONS] <COMMAND>") => {
            if let Some(typed) = invalid(&e, ContextKind::InvalidSubcommand) {
                let commands = Cli::command();
                let closest = commands
                    .get_subcommands()
                    .map(|c| (edit_distance(&typed, c.get_name()), c.get_name().to_owned()))
                    .min()
                    .filter(|(distance, _)| *distance <= 2);
                if let Some((_, name)) = closest {
                    e.insert(
                        ContextKind::SuggestedSubcommand,
                        ContextValue::Strings(vec![name]),
                    );
                }
            }
        }
        ErrorKind::UnknownArgument if usage.contains("clerk extend") => {
            if let Some(typed) = invalid(&e, ContextKind::InvalidArg)
                && leclerk::duration::parse_duration(&typed).is_ok()
            {
                e.insert(
                    ContextKind::Suggested,
                    ContextValue::StyledStrs(vec![StyledStr::from(format!(
                        "extend a lease with `clerk extend <lease> --for {typed}`"
                    ))]),
                );
            }
        }
        _ => {}
    }
    e
}

/// The Levenshtein distance between `a` and `b`.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (above + 1)
                .min(row[j] + 1)
                .min(diagonal + usize::from(ca != *cb));
            diagonal = above;
        }
    }
    row[b.len()]
}

/// A command line clap refused. Help and version text print as clap prints
/// them; a real usage error is a JSON error in JSON mode, exit 2 either way.
fn parse_failure(e: &clap::Error) -> ExitCode {
    use clap::error::ErrorKind;
    let json_flag = std::env::args_os().any(|a| a == "--json");
    let shows_text = matches!(
        e.kind(),
        ErrorKind::DisplayHelp
            | ErrorKind::DisplayVersion
            | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    );
    if shows_text || Mode::detect(json_flag) == Mode::Human {
        e.exit();
    }
    let rendered = e.render().to_string();
    // clap ends with "For more information, try '--help'.": not for JSON.
    let body = rendered
        .split("\n\nFor more information")
        .next()
        .unwrap_or_default();
    let message = body.trim().trim_start_matches("error: ").to_owned();
    let mut out = Output::new(Mode::Json);
    let agent = std::env::var_os(caller::SESSION_VAR).is_some_and(|v| !v.is_empty());
    let json_wanted = json_flag
        || std::env::var("CLERK_AGENT_FORMAT").is_ok_and(|f| matches!(f.trim(), "json" | "human"));
    if agent && !json_wanted {
        // An agent mistyping a command reads the same Markdown errors.
        out = out.for_agent("usage").markdown();
    }
    out.fail(UsageError(message))
}

fn run_daemon() -> anyhow::Result<()> {
    let paths = Paths::resolve()?;
    daemon::run(&paths, Config::load(&paths)?)?;
    Ok(())
}

/// Warnings and errors to stderr; stdout stays for command output.
fn init_logging() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::WARN)
        .try_init();
}

fn execute(cli: Cli, out: Output) -> anyhow::Result<()> {
    let ctx = Ctx {
        paths: Paths::resolve()?,
        out,
        override_reason: cli.override_reason,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(dispatch(&ctx, cli.command))
}

async fn dispatch(ctx: &Ctx, command: Command) -> anyhow::Result<()> {
    match command {
        Command::Daemon { action: None } | Command::Hook { .. } => Ok(()),
        Command::Daemon {
            action: Some(action),
        } => manage_daemon(ctx, action).await,
        Command::Resource { action } => inventory::resource(ctx, action).await,
        Command::Unit { action } => inventory::unit(ctx, action).await,
        Command::Status => views::status(ctx).await,
        Command::Resources(filter) => views::resources(ctx, filter.tags).await,
        Command::Leases { mine } => views::leases(ctx, mine).await,
        Command::Queue { resource } => views::queue(ctx, &resource).await,
        Command::Log(args) => views::log(ctx, args).await,
        Command::Promotions => views::promotions(ctx).await,
        Command::Admin(args) => admin::run(ctx, args).await,
        Command::Approve { promotion, new_id } => views::approve(ctx, promotion, new_id).await,
        Command::Acquire(args) => lease::acquire(ctx, args).await,
        Command::Attach { lease } => lease::attach(ctx, lease).await,
        Command::Cancel { request } => lease::cancel_request(ctx, request).await,
        Command::Release {
            lease,
            force: true,
            yes,
            ..
        } => admin::force_release(ctx, lease, yes).await,
        Command::Release { lease, cleanup, .. } => lease::release(ctx, lease, cleanup).await,
        Command::Types => agent::types(ctx),
        Command::Confirm { lease, how } => agent::confirm(ctx, &lease, how).await,
        Command::Interruptible { lease, state } => agent::interruptible(ctx, &lease, state).await,
        Command::Yield { lease, .. } => agent::refuse_yield(ctx, &lease).await,
        Command::Reclaim { lease } => agent::reclaim(ctx, &lease).await,
        Command::Claim { lease } => agent::claim(ctx, &lease).await,
        Command::Extend(args) => lease::extend(ctx, args).await,
        Command::Run(args) => lease::run(ctx, args).await,
        Command::Wait(args) => lease::wait(ctx, args).await,
    }
}

/// `clerk daemon status|stop|restart`.
async fn manage_daemon(ctx: &Ctx, action: DaemonAction) -> anyhow::Result<()> {
    let caller = client::cli_caller();
    let info = match action {
        // Only "lock free and nothing answers" is not running; an
        // unresponsive daemon is an error, never "not running".
        DaemonAction::Status => match Client::probe(&ctx.paths).await {
            Ok(info) => Some(info),
            Err(DaemonError::Unavailable) => None,
            // The daemon cannot run here: not running, with the reason.
            Err(DaemonError::SocketPathTooLong(e)) => {
                return ctx.out.emit(&not_running_view(&e.to_string()));
            }
            Err(e) => return Err(e.into()),
        },
        DaemonAction::Stop => {
            // An agent stops the daemon for every project only on the
            // user's word; the daemon asks for it too.
            let caller = ctx.caller(CallerKind::Detached)?;
            gate::require_override(
                "stop the daemon, which every project's agents rely on",
                &caller,
            )?;
            Client::stop(&ctx.paths, &caller).await?;
            None
        }
        DaemonAction::Restart => Some(Client::restart(&ctx.paths, &caller).await?.daemon().clone()),
    };
    ctx.out.emit(&daemon_view(action, info))
}

fn not_running_view(reason: &str) -> View {
    let text = format!("The clerk daemon is not running: {reason}");
    View::new(
        json!({"running": false, "reason": reason}),
        move |_: &Value| text.clone(),
    )
}

fn daemon_view(action: DaemonAction, info: Option<DaemonInfo>) -> View {
    let value = match &info {
        Some(d) => json!({"running": true, "pid": d.pid, "version": d.version}),
        None => json!({"running": false}),
    };
    View::new(value, move |_: &Value| match (action, &info) {
        (DaemonAction::Stop, _) => "The clerk daemon is stopped.".to_owned(),
        (_, Some(d)) => format!(
            "The clerk daemon is running (pid {}, version {}).",
            d.pid, d.version
        ),
        (_, None) => "The clerk daemon is not running; any clerk command starts it.".to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The skill's files, relative to the repo root.
    const SKILL_DIR: &str = "integrations/claude-code/skills/clerk";
    const SKILL_FILES: [&str; 3] = ["SKILL.md", "leasing.md", "registering.md"];

    /// Every `clerk ...` command in `text`: an inline code span, or a line
    /// of a fenced block (with its `\` continuations), as argv. A `<name>`
    /// placeholder becomes a value, `a|b` its first choice; a command
    /// cut off by `...` ends there.
    fn clerk_commands(text: &str) -> Vec<Vec<String>> {
        let mut spans = Vec::new();
        let mut in_block = false;
        let mut block_line = String::new();
        let mut prose = String::new();
        for line in text.lines() {
            if line.starts_with("```") {
                in_block = !in_block;
                continue;
            }
            if in_block {
                let line = line.trim_end();
                if let Some(head) = line.strip_suffix('\\') {
                    block_line.push_str(head);
                } else {
                    block_line.push_str(line);
                    spans.push(std::mem::take(&mut block_line));
                }
            } else {
                prose.push_str(line);
                prose.push(' ');
            }
        }
        spans.extend(prose.split('`').skip(1).step_by(2).map(str::to_owned));
        spans
            .iter()
            .filter(|s| s.trim_start().starts_with("clerk "))
            .map(|s| argv(s))
            .filter(|a| a.len() > 1)
            .collect()
    }

    /// The words of `span`, quotes removed; a quoted word is never a
    /// placeholder or an ellipsis.
    fn argv(span: &str) -> Vec<String> {
        let mut words: Vec<(String, bool)> = Vec::new();
        let mut word = String::new();
        let mut quote = None;
        let mut quoted = false;
        for c in span.chars() {
            match (quote, c) {
                (Some(q), c) if c == q => quote = None,
                (None, '"' | '\'') => {
                    quote = Some(c);
                    quoted = true;
                }
                (None, c) if c.is_whitespace() => {
                    if quoted || !word.is_empty() {
                        words.push((std::mem::take(&mut word), quoted));
                    }
                    quoted = false;
                }
                (_, c) => word.push(c),
            }
        }
        if quoted || !word.is_empty() {
            words.push((word, quoted));
        }
        let mut out = Vec::new();
        for (w, quoted) in words {
            if quoted {
                out.push(w);
                continue;
            }
            let w = match w.as_str() {
                // A value after a flag, else the rest of the command.
                "..." if out.last().is_some_and(|f: &String| f.starts_with("--")) => "x".to_owned(),
                // An alternative command line starts here.
                "..." | "|" => break,
                "<d>" => "5m".to_owned(),
                "<type>" => "device".to_owned(),
                _ if w.starts_with('<') && w.ends_with('>') => "x".to_owned(),
                _ => w,
            };
            out.push(w.split('|').next().unwrap_or_default().to_owned());
        }
        out
    }

    #[test]
    fn every_clerk_command_in_the_skill_parses() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut checked = 0;
        for file in SKILL_FILES {
            let text = std::fs::read_to_string(root.join(SKILL_DIR).join(file)).unwrap();
            for args in clerk_commands(&text) {
                // `clerk approve` alone names a command; what it lacks is
                // arguments, not a valid spelling.
                if let Err(e) = Cli::try_parse_from(&args)
                    && e.kind() != clap::error::ErrorKind::MissingRequiredArgument
                {
                    panic!("{file}: `{}` does not parse: {e}", args.join(" "));
                }
                checked += 1;
            }
        }
        assert!(checked > 20, "only {checked} commands found");
    }

    #[test]
    fn a_misspelled_command_does_not_parse() {
        let found = clerk_commands("Run `clerk aquire board --for 5m` now.\n");
        assert_eq!(found.len(), 1);
        assert!(Cli::try_parse_from(&found[0]).is_err());
    }
}
