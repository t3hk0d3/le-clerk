//! Who this CLI process acts as.

use std::path::Path;

use leclerk::client::ApiError;
use leclerk::model::{Caller, HolderRef, Host, ProjectKey};
use leclerk::scheduler::liveness;
use leclerk::scope::{ProjectResolver, ScopeError};
use ulid::Ulid;

use super::UsageError;

/// The variable Claude Code sets in an agent session's environment.
pub const SESSION_VAR: &str = "CLAUDE_CODE_SESSION_ID";

/// The variable Claude Code sets to its own process id for Bash and hook
/// commands (Claude Code 2.1.214 or later).
pub const PID_VAR: &str = "CLAUDE_PID";

/// The Claude Code process running this agent session: `CLAUDE_PID` with
/// its start time; `None` when unset, unparsable or no longer running.
#[must_use]
pub fn host() -> Option<Host> {
    host_from(
        std::env::var(PID_VAR).ok(),
        liveness::start_time,
        liveness::boot_id(),
    )
}

/// [`host`] with its inputs given.
fn host_from(
    pid: Option<String>,
    start_time: impl Fn(u32) -> Option<u64>,
    boot_id: Option<String>,
) -> Option<Host> {
    let pid: u32 = pid?.trim().parse().ok()?;
    Some(Host {
        pid,
        start_time: start_time(pid)?,
        boot_id,
    })
}

/// The host of `caller` when it is an agent session, for commands that
/// hold or queue for it: such a lease must be tied to a running process.
///
/// # Errors
/// `agent_unidentified` for an agent session without a usable
/// `CLAUDE_PID`.
pub fn require_host(caller: &Caller) -> Result<Option<Host>, ApiError> {
    if !is_agent(caller) {
        return Ok(None);
    }
    match host() {
        Some(host) => Ok(Some(host)),
        None => Err(ApiError {
            code: "agent_unidentified".to_owned(),
            message: format!(
                "this agent session cannot hold leases: {PID_VAR} is not set or names no running \
                 process. The clerk needs Claude Code 2.1.214 or later, which sets {PID_VAR} for \
                 Bash commands; update Claude Code and run the command again."
            ),
            details: None,
        }),
    }
}

/// How the command holds what it gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallerKind {
    /// A command the person or agent waits on: this process holds.
    Interactive,
    /// A lease that outlives this process: a fresh user holder.
    Detached,
    /// `clerk run`: this process holds while the child runs.
    Run,
}

/// The caller for this process: an agent session when
/// `CLAUDE_CODE_SESSION_ID` is set, else per `kind`; the project comes from
/// the working directory.
///
/// # Errors
/// [`UsageError`] when `override_reason` is given but blank;
/// [`ScopeError::Git`] when git fails in the working directory's checkout.
pub fn current(kind: CallerKind, override_reason: Option<String>) -> anyhow::Result<Caller> {
    let session = std::env::var(SESSION_VAR).ok();
    let project = match std::env::current_dir() {
        Ok(cwd) => project_of(&cwd)?,
        Err(_) => None,
    };
    Ok(resolve(
        kind,
        session,
        project,
        override_reason,
        liveness::self_start_time(),
    )?)
}

/// True when the caller is an agent session.
#[must_use]
pub fn is_agent(caller: &Caller) -> bool {
    matches!(caller.holder, HolderRef::AgentSession { .. })
}

/// The project of `cwd`; none when `cwd` is gone.
///
/// # Errors
/// [`ScopeError::Git`] when git fails inside a git checkout: the project
/// is unknown, and guessing could file this work under another project.
fn project_of(cwd: &Path) -> Result<Option<ProjectKey>, ScopeError> {
    match ProjectResolver::resolve(cwd) {
        Ok(project) => Ok(Some(project)),
        Err(e @ ScopeError::Git { .. }) => Err(e),
        Err(e @ ScopeError::NoSuchDirectory(_)) => {
            tracing::debug!(error = %e, "no project for the working directory");
            Ok(None)
        }
    }
}

/// [`current`] with its inputs given.
fn resolve(
    kind: CallerKind,
    session: Option<String>,
    project: Option<ProjectKey>,
    override_reason: Option<String>,
    start_time: Option<u64>,
) -> Result<Caller, UsageError> {
    if override_reason
        .as_ref()
        .is_some_and(|r| r.trim().is_empty())
    {
        return Err(UsageError(
            "--override needs a reason: say what the user asked for, e.g. --override \"the user \
             asked me to free the board\""
                .to_owned(),
        ));
    }
    let holder = match (session.filter(|s| !s.is_empty()), kind, start_time) {
        (Some(session_id), _, _) => HolderRef::AgentSession { session_id },
        (None, CallerKind::Interactive | CallerKind::Run, Some(start_time)) => {
            HolderRef::CliProcess {
                pid: std::process::id(),
                start_time,
                boot_id: liveness::boot_id(),
            }
        }
        // Without a start time this process cannot be told from a later one
        // with its pid; a fresh user holder is the safe stand-in.
        (None, _, _) => HolderRef::User {
            tag: Ulid::new().to_string(),
        },
    };
    Ok(Caller {
        holder,
        project,
        override_reason,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn host_is_claude_pid_with_its_start_time() {
        let host = host_from(Some("42".to_owned()), |pid| (pid == 42).then_some(7), None);
        assert_eq!(
            host,
            Some(Host {
                pid: 42,
                start_time: 7,
                boot_id: None
            })
        );
    }

    #[test]
    fn no_host_without_claude_pid_or_with_a_gone_or_bad_one() {
        assert_eq!(host_from(None, |_| Some(7), None), None);
        assert_eq!(host_from(Some("42".to_owned()), |_| None, None), None);
        assert_eq!(host_from(Some("x".to_owned()), |_| Some(7), None), None);
        assert_eq!(host_from(Some(String::new()), |_| Some(7), None), None);
    }

    fn project() -> ProjectKey {
        ProjectKey(PathBuf::from("/work/repo/.git"))
    }

    #[test]
    fn session_id_makes_an_agent_session_holder_with_the_project() {
        let caller = resolve(
            CallerKind::Interactive,
            Some("s1".to_owned()),
            Some(project()),
            None,
            Some(7),
        )
        .unwrap();
        assert_eq!(
            caller.holder,
            HolderRef::AgentSession {
                session_id: "s1".to_owned()
            }
        );
        assert_eq!(caller.project, Some(project()));
    }

    #[test]
    fn interactive_without_session_is_this_cli_process() {
        let caller = resolve(CallerKind::Interactive, None, None, None, Some(7)).unwrap();
        assert_eq!(
            caller.holder,
            HolderRef::CliProcess {
                pid: std::process::id(),
                start_time: 7,
                boot_id: liveness::boot_id()
            }
        );
    }

    #[test]
    fn run_without_session_is_this_cli_process() {
        let caller = resolve(CallerKind::Run, None, None, None, Some(7)).unwrap();
        assert!(matches!(caller.holder, HolderRef::CliProcess { .. }));
    }

    #[test]
    fn detached_without_session_is_a_fresh_user_holder() {
        let a = resolve(CallerKind::Detached, None, None, None, Some(7)).unwrap();
        let b = resolve(CallerKind::Detached, None, None, None, Some(7)).unwrap();
        assert!(matches!(a.holder, HolderRef::User { .. }));
        assert_ne!(a.holder, b.holder);
    }

    #[test]
    fn blank_override_reason_is_a_usage_error() {
        let err = resolve(
            CallerKind::Interactive,
            None,
            None,
            Some(" ".to_owned()),
            Some(7),
        )
        .unwrap_err();
        assert!(err.0.contains("--override"));
    }

    #[test]
    fn override_reason_is_carried() {
        let caller = resolve(
            CallerKind::Interactive,
            None,
            None,
            Some("asked".to_owned()),
            Some(7),
        )
        .unwrap();
        assert_eq!(caller.override_reason.as_deref(), Some("asked"));
    }
}
