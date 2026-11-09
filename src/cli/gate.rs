//! Gates in front of commands that need the user's say-so.
//!
//! Agents (`CLAUDE_CODE_SESSION_ID` set) need `--override "<reason>"`.
//! [`require`] also asks for confirmation: on a terminal it prompts, and
//! without one it needs `--yes`. Stdin is read only when it is a terminal.

use std::io::{self, BufRead, IsTerminal, Write};

use leclerk::model::Caller;
use thiserror::Error;

use super::caller;

/// Why a gated command did not run. Nothing was changed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GateRefusal {
    /// An agent called without `--override`.
    #[error(
        "Refused: an agent must pass --override \"<reason>\" to {effect}. Ask the user, then retry \
         with the reason they gave."
    )]
    OverrideRequired { effect: String },
    /// Not on a terminal, and `--yes` was not given.
    #[error(
        "Refused: confirm with --yes to {effect}; without a terminal there is nobody to ask. \
         Nothing was changed."
    )]
    ConfirmationRequired { effect: String },
    /// The person answered no.
    #[error("Aborted: nothing was changed.")]
    Aborted,
}

impl GateRefusal {
    /// The stable error code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::OverrideRequired { .. } => "override_required",
            Self::ConfirmationRequired { .. } => "confirmation_required",
            Self::Aborted => "aborted",
        }
    }
}

/// Refuse an agent caller that gave no override reason.
///
/// # Errors
/// [`GateRefusal::OverrideRequired`].
pub fn require_override(effect: &str, caller: &Caller) -> Result<(), GateRefusal> {
    if caller::is_agent(caller) && caller.override_reason.is_none() {
        return Err(GateRefusal::OverrideRequired {
            effect: effect.to_owned(),
        });
    }
    Ok(())
}

/// [`require_override`], then confirmation: `yes` passes; otherwise ask on
/// a terminal, or refuse without one.
///
/// # Errors
/// [`GateRefusal`] naming why.
// Used by the admin commands.
#[allow(dead_code)]
pub fn require(effect: &str, yes: bool, caller: &Caller) -> Result<(), GateRefusal> {
    require_override(effect, caller)?;
    if yes {
        return Ok(());
    }
    if !io::stdin().is_terminal() {
        return Err(GateRefusal::ConfirmationRequired {
            effect: effect.to_owned(),
        });
    }
    eprint!("This will {effect}. Proceed? [y/N] ");
    let _ = io::stderr().flush();
    let mut answer = String::new();
    if io::stdin().lock().read_line(&mut answer).is_err() || !agrees(&answer) {
        return Err(GateRefusal::Aborted);
    }
    Ok(())
}

/// True for "y" or "yes", in any case.
fn agrees(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

#[cfg(test)]
mod tests {
    use leclerk::model::HolderRef;

    use super::*;

    fn agent(reason: Option<&str>) -> Caller {
        Caller {
            holder: HolderRef::AgentSession {
                session_id: "s".to_owned(),
            },
            project: None,
            override_reason: reason.map(str::to_owned),
        }
    }

    #[test]
    fn agent_without_reason_is_refused() {
        assert_eq!(
            require_override("approve", &agent(None))
                .unwrap_err()
                .code(),
            "override_required"
        );
    }

    #[test]
    fn agent_with_reason_passes() {
        assert!(require_override("approve", &agent(Some("asked"))).is_ok());
    }

    #[test]
    fn yes_skips_the_prompt() {
        assert!(require("clean", true, &agent(Some("asked"))).is_ok());
    }

    #[test]
    fn only_y_or_yes_agrees() {
        assert!(agrees("y\n"));
        assert!(agrees("YES"));
        assert!(!agrees("n"));
        assert!(!agrees(""));
    }
}
