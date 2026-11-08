use std::io::IsTerminal;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use leclerk::client::{self, Client, DaemonError, DaemonInfo};
use leclerk::config::{Config, Paths};
use leclerk::daemon;
use leclerk::error::ErrorCode;
use serde_json::json;

/// Exit code: the daemon is unavailable.
const EXIT_UNAVAILABLE: u8 = 5;

/// Le Clerk: a local resource-lease broker for AI coding agents.
#[derive(Parser)]
#[command(name = "clerk", version = leclerk::VERSION, arg_required_else_help = true)]
struct Cli {
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
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Show whether the daemon runs, with its pid and version.
    Status,
    /// Stop the running daemon.
    Stop,
    /// Stop the running daemon and start a fresh one.
    Restart,
}

fn main() -> anyhow::Result<ExitCode> {
    let cli = Cli::parse();
    let paths = Paths::resolve()?;
    match cli.command {
        Command::Daemon { action: None } => {
            daemon::run(&paths, Config::load(&paths)?)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Daemon {
            action: Some(action),
        } => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let caller = client::cli_caller();
            let outcome = runtime.block_on(async {
                match action {
                    // Only "lock free and nothing answers" is not running;
                    // an unresponsive daemon is an error, never "not running".
                    DaemonAction::Status => match Client::probe(&paths).await {
                        Ok(info) => Ok(Some(info)),
                        Err(DaemonError::Unavailable) => Ok(None),
                        Err(e) => Err(e),
                    },
                    DaemonAction::Stop => Client::stop(&paths, &caller).await.map(|_| None),
                    DaemonAction::Restart => Client::restart(&paths, &caller)
                        .await
                        .map(|c| Some(c.daemon().clone())),
                }
            });
            Ok(report(&action, outcome))
        }
    }
}

/// Print the outcome: human text on a terminal, JSON otherwise.
fn report(action: &DaemonAction, outcome: Result<Option<DaemonInfo>, DaemonError>) -> ExitCode {
    let human = std::io::stdout().is_terminal();
    match outcome {
        Err(e) => {
            if human {
                eprintln!("{e}");
            } else {
                println!(
                    "{}",
                    json!({"error": {"code": e.code(), "message": e.to_string()}})
                );
            }
            ExitCode::from(EXIT_UNAVAILABLE)
        }
        Ok(info) => {
            match (action, info, human) {
                (DaemonAction::Stop, _, true) => println!("The clerk daemon is stopped."),
                (_, Some(d), true) => println!(
                    "The clerk daemon is running (pid {}, version {}).",
                    d.pid, d.version
                ),
                (_, Some(d), false) => println!(
                    "{}",
                    json!({"running": true, "pid": d.pid, "version": d.version})
                ),
                (_, None, true) => {
                    println!("The clerk daemon is not running; any clerk command starts it.");
                }
                (_, None, false) => {
                    println!("{}", json!({"running": false}));
                }
            }
            ExitCode::SUCCESS
        }
    }
}
