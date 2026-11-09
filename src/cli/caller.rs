//! Who this CLI process acts as.

use std::path::Path;

use leclerk::model::{Caller, HolderRef, ProjectKey};
use leclerk::scheduler::liveness;
use leclerk::scope::ProjectResolver;
use ulid::Ulid;

use super::UsageError;

/// The variable Claude Code sets in an agent session's environment.
pub const SESSION_VAR: &str = "CLAUDE_CODE_SESSION_ID";

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
/// [`UsageError`] when `override_reason` is given but blank.
pub fn current(kind: CallerKind, override_reason: Option<String>) -> anyhow::Result<Caller> {
    let session = std::env::var(SESSION_VAR).ok();
    let project = std::env::current_dir()
        .ok()
        .and_then(|cwd| project_of(&cwd));
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

fn project_of(cwd: &Path) -> Option<ProjectKey> {
    match ProjectResolver::resolve(cwd) {
        Ok(project) => Some(project),
        Err(e) => {
            tracing::debug!(error = %e, "no project for the working directory");
            None
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
                start_time: 7
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
