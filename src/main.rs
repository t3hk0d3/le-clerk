//! `clerk`: the Le Clerk command line and daemon.

mod cli;

use std::process::ExitCode;

fn main() -> ExitCode {
    cli::run()
}
