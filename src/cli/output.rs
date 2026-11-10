//! Printing results and errors: human text on a terminal, JSON otherwise.
//!
//! Command output goes to stdout and nothing else does; in JSON mode an
//! error is printed there too, as `{"error": {...}}`. Human-mode errors and
//! logs go to stderr.

use std::fmt::Write as _;
use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;

use leclerk::client::{ApiError, DaemonError};
use leclerk::error::ErrorCode;
use leclerk::scope::ScopeError;
use serde::Serialize;
use serde_json::{Value, json};
use thiserror::Error;

use super::UsageError;
use super::gate::GateRefusal;
use super::input::InputError;
use super::render::{self, Style};

/// Exit code: an internal failure that fits no other class.
const EXIT_INTERNAL: u8 = 1;
/// Exit code: usage error.
pub const EXIT_USAGE: u8 = 2;
/// Exit code: no such thing.
const EXIT_NOT_FOUND: u8 = 3;
/// Exit code: refused or conflict.
pub const EXIT_REFUSED: u8 = 4;
/// Exit code: the daemon is unavailable, unresponsive or another version.
pub const EXIT_DAEMON: u8 = 5;
/// Exit code: the request is queued (`--no-wait`).
pub const EXIT_QUEUED: u8 = 6;
/// Exit code: an interactive `acquire` or `attach` ended by Ctrl+C.
pub const EXIT_INTERRUPTED: u8 = 130;

/// End the command with this exit code; whatever needed saying was
/// already printed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("exit status {0}")]
pub struct Exit(pub u8);

/// How results are printed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Plain aligned text for people.
    Human,
    /// One JSON value per command.
    Json,
}

impl Mode {
    /// JSON when asked for, or when stdout is not a terminal; else human.
    #[must_use]
    pub fn detect(json_flag: bool) -> Self {
        if json_flag || !io::stdout().is_terminal() {
            Self::Json
        } else {
            Self::Human
        }
    }
}

/// How a value reads to a person.
pub trait HumanRender {
    /// The text to print, without a trailing newline.
    fn render(&self) -> String;
}

/// A daemon answer (or a CLI-built value) with the function that renders it
/// for people. Serialises as the value itself.
pub struct View {
    value: Value,
    render: Box<dyn Fn(&Value) -> String>,
}

impl View {
    pub fn new(value: Value, render: impl Fn(&Value) -> String + 'static) -> Self {
        Self {
            value,
            render: Box::new(render),
        }
    }
}

impl Serialize for View {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.value.serialize(s)
    }
}

impl HumanRender for View {
    fn render(&self) -> String {
        (self.render)(&self.value)
    }
}

/// Why a command failed, as printed: a stable code, a message, the daemon's
/// details (when it gave any) and the exit code.
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    pub code: String,
    pub message: String,
    pub details: Option<Value>,
    pub exit: u8,
}

impl From<ApiError> for Failure {
    fn from(e: ApiError) -> Self {
        Self {
            exit: exit_for(&e.code),
            code: e.code,
            message: e.message,
            details: e.details,
        }
    }
}

impl From<DaemonError> for Failure {
    fn from(e: DaemonError) -> Self {
        Self {
            code: e.code().to_owned(),
            message: e.to_string(),
            details: None,
            exit: EXIT_DAEMON,
        }
    }
}

impl From<UsageError> for Failure {
    fn from(e: UsageError) -> Self {
        Self {
            code: "usage_error".to_owned(),
            message: e.to_string(),
            details: None,
            exit: EXIT_USAGE,
        }
    }
}

impl From<InputError> for Failure {
    fn from(e: InputError) -> Self {
        Self {
            code: "invalid_input".to_owned(),
            message: e.to_string(),
            details: None,
            exit: EXIT_USAGE,
        }
    }
}

impl From<GateRefusal> for Failure {
    fn from(e: GateRefusal) -> Self {
        Self {
            code: e.code().to_owned(),
            message: e.to_string(),
            details: None,
            exit: EXIT_REFUSED,
        }
    }
}

impl From<anyhow::Error> for Failure {
    /// The typed error inside, or `internal_error` (exit 1) for anything
    /// else.
    fn from(e: anyhow::Error) -> Self {
        let e = match e.downcast::<ApiError>() {
            Ok(api) => return api.into(),
            Err(e) => e,
        };
        let e = match e.downcast::<DaemonError>() {
            Ok(daemon) => return daemon.into(),
            Err(e) => e,
        };
        let e = match e.downcast::<UsageError>() {
            Ok(usage) => return usage.into(),
            Err(e) => e,
        };
        let e = match e.downcast::<InputError>() {
            Ok(input) => return input.into(),
            Err(e) => e,
        };
        let e = match e.downcast::<GateRefusal>() {
            Ok(gate) => return gate.into(),
            Err(e) => e,
        };
        let e = match e.downcast::<ScopeError>() {
            Ok(scope) => {
                return Self {
                    code: scope.code().to_owned(),
                    message: scope.to_string(),
                    details: None,
                    exit: exit_for(scope.code()),
                };
            }
            Err(e) => e,
        };
        Self {
            code: "internal_error".to_owned(),
            message: format!("{e:#}"),
            details: None,
            exit: EXIT_INTERNAL,
        }
    }
}

/// The exit code for a daemon error code.
fn exit_for(code: &str) -> u8 {
    match code {
        "not_found" | "unknown_method" => EXIT_NOT_FOUND,
        "daemon_unavailable"
        | "daemon_unresponsive"
        | "protocol_mismatch"
        | "daemon_restarting"
        | "daemon_stopping"
        | "engine_unavailable" => EXIT_DAEMON,
        "invalid_request"
        | "invalid_argument"
        | "invalid_id"
        | "invalid_settings"
        | "invalid_tag"
        | "too_many_tags"
        | "conflicting_tag_edit"
        | "description_multiline"
        | "description_too_long"
        | "usage_too_long"
        | "unknown_placeholder"
        | "health_empty"
        | "health_blank_command"
        | "health_check_not_allowed"
        | "settings_type_mismatch"
        | "settings_empty_field"
        | "settings_zero_port"
        | "extend_not_later"
        | "agent_unidentified"
        | "no_such_directory" => EXIT_USAGE,
        "internal_error" | "database_error" | "corrupt_database" | "migration_error"
        | "io_error" | "json_error" | "bad_response" | "git_failed" => EXIT_INTERNAL,
        // Conflicts and refusals: busy, already_held, no_units, ...
        _ => EXIT_REFUSED,
    }
}

/// Where and how a command prints.
#[derive(Debug, Clone, Copy)]
pub struct Output {
    mode: Mode,
    /// The command an agent session runs: JSON results and errors then
    /// carry the next step (see [`super::next`]).
    agent: Option<&'static str>,
    /// Agent results as Markdown ([`super::md`]) instead of JSON.
    markdown: bool,
}

impl Output {
    #[must_use]
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            agent: None,
            markdown: false,
        }
    }

    /// This output for agent session command `command`.
    #[must_use]
    pub fn for_agent(self, command: &'static str) -> Self {
        Self {
            agent: Some(command),
            ..self
        }
    }

    /// Whether results print as Markdown (an agent, stdout not a terminal).
    #[must_use]
    pub fn is_markdown(self) -> bool {
        self.markdown && self.agent.is_some() && matches!(self.mode, Mode::Json)
    }

    /// This agent output printing Markdown when it would print JSON.
    #[must_use]
    pub fn markdown(self) -> Self {
        Self {
            markdown: true,
            ..self
        }
    }

    /// How this output prints.
    #[must_use]
    pub fn mode(self) -> Mode {
        self.mode
    }

    /// Print `value` on stdout.
    ///
    /// # Errors
    /// When stdout cannot be written or the value does not serialise.
    pub fn emit<T: Serialize + HumanRender>(self, value: &T) -> anyhow::Result<()> {
        let text = match self.mode {
            Mode::Json => {
                let mut json = serde_json::to_value(value)?;
                match self.agent {
                    Some(command) if self.markdown => super::md::render(
                        command,
                        &json,
                        leclerk::clock::Clock::now_ms(&leclerk::clock::SystemClock),
                    ),
                    Some(command) => {
                        super::next::annotate(command, &mut json);
                        json.to_string()
                    }
                    None => json.to_string(),
                }
            }
            Mode::Human => render::finish(&value.render(), Style::current()),
        };
        let mut stdout = io::stdout().lock();
        if text.is_empty() {
            return Ok(());
        }
        writeln!(stdout, "{}", text.trim_end_matches('\n'))?;
        stdout.flush()?;
        Ok(())
    }

    /// Report `failure` and return its exit code. JSON mode prints
    /// `{"error": {"code", "message", "details"?}}` on stdout; human mode
    /// prints the message, then one `  key: value` line per detail, on
    /// stderr.
    pub fn fail(self, failure: impl Into<Failure>) -> ExitCode {
        let mut f = failure.into();
        if self.agent.is_some() {
            f.message = super::next::error_message(&f.code, &f.message);
        }
        match self.mode {
            Mode::Json if self.markdown && self.agent.is_some() => {
                let mut stdout = io::stdout().lock();
                let now = leclerk::clock::Clock::now_ms(&leclerk::clock::SystemClock);
                let md = super::md::error(&f.code, &f.message, f.details.as_ref(), now);
                let _ = writeln!(stdout, "{md}");
            }
            Mode::Json => {
                let mut error = json!({"code": f.code, "message": f.message});
                if let Some(details) = &f.details {
                    error["details"] = details.clone();
                }
                let mut stdout = io::stdout().lock();
                let _ = writeln!(stdout, "{}", json!({ "error": error }));
            }
            Mode::Human => {
                let text =
                    render::on_stderr(|| render::finish(&human_failure(&f), Style::current()));
                eprintln!("{text}");
            }
        }
        ExitCode::from(f.exit)
    }
}

/// A failure as people read it: the message (for `not_found` with
/// details, `not found: <kind> "<id>"`), then one `  key: value` line per
/// other detail.
fn human_failure(f: &Failure) -> String {
    let named = f.details.as_ref().and_then(|d| {
        let kind = d.get("kind")?.as_str()?;
        let id = d.get("id")?.as_str()?;
        Some((kind, id))
    });
    if f.code == "not_found"
        && let Some((kind, id)) = named
    {
        return format!("not found: {kind} \"{id}\"");
    }
    let mut text = f.message.clone();
    if let Some(Value::Object(details)) = &f.details {
        for (key, value) in details {
            let _ = write!(text, "\n  {key}: {}", plain(value));
        }
    }
    text
}

/// A JSON value as plain text: strings unquoted, lists of scalars joined
/// with commas, anything else as JSON.
#[must_use]
pub fn plain(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "-".to_owned(),
        Value::Array(items) if items.iter().all(|i| !i.is_object() && !i.is_array()) => {
            items.iter().map(plain).collect::<Vec<_>>().join(", ")
        }
        other => other.to_string(),
    }
}

/// `text` cut to at most `max` characters, ending in `…` when cut.
#[must_use]
pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// Rows padded into aligned columns, separated by two spaces. The last
/// column is not padded.
#[must_use]
pub fn table(rows: &[Vec<String>]) -> String {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..columns)
        .map(|c| {
            rows.iter()
                .filter_map(|r| r.get(c))
                .map(|cell| cell.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    for row in rows {
        let mut line = String::new();
        for (c, cell) in row.iter().enumerate() {
            if c + 1 == row.len() {
                line.push_str(cell);
            } else {
                let _ = write!(line, "{cell:<w$}  ", w = widths[c]);
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_short_text() {
        assert_eq!(truncate("abc", 3), "abc");
    }

    #[test]
    fn truncate_cuts_long_text_with_an_ellipsis() {
        assert_eq!(truncate("abcdef", 4), "abc…");
    }

    #[test]
    fn plain_joins_scalar_lists() {
        assert_eq!(plain(&json!(["a", "b"])), "a, b");
    }

    #[test]
    fn table_aligns_columns() {
        let rows = vec![
            vec!["a".to_owned(), "bb".to_owned(), "c".to_owned()],
            vec!["aaa".to_owned(), "b".to_owned(), "c".to_owned()],
        ];
        assert_eq!(table(&rows), "a    bb  c\naaa  b   c\n");
    }

    #[test]
    fn exit_codes_follow_the_constraints() {
        // Every error code the daemon and client produce, by class.
        let classes: [(u8, &[&str]); 5] = [
            (3, &["not_found", "unknown_method"]),
            (
                5,
                &[
                    "daemon_unavailable",
                    "daemon_unresponsive",
                    "protocol_mismatch",
                    "engine_unavailable",
                ],
            ),
            (
                2,
                &[
                    "invalid_request",
                    "invalid_argument",
                    "invalid_id",
                    "invalid_settings",
                    "invalid_tag",
                    "too_many_tags",
                    "conflicting_tag_edit",
                    "description_multiline",
                    "description_too_long",
                    "usage_too_long",
                    "unknown_placeholder",
                    "health_empty",
                    "health_blank_command",
                    "health_check_not_allowed",
                    "settings_type_mismatch",
                    "settings_empty_field",
                    "settings_zero_port",
                    "extend_not_later",
                    "no_such_directory",
                ],
            ),
            (
                1,
                &[
                    "internal_error",
                    "database_error",
                    "corrupt_database",
                    "migration_error",
                    "io_error",
                    "json_error",
                    "bad_response",
                ],
            ),
            (
                4,
                &[
                    "already_exists",
                    "already_held",
                    "ambiguous_duplicate",
                    "bad_state",
                    "borrower_not_overrun",
                    "busy",
                    "duplicate_in_scope",
                    "extend_refused",
                    "extension_used",
                    "id_clash",
                    "import_clash",
                    "link_conflict",
                    "not_directly_linked",
                    "not_holder",
                    "no_units",
                    "retiring",
                    "scope_not_allowed",
                ],
            ),
        ];
        for (exit, codes) in classes {
            for code in codes {
                assert_eq!(exit_for(code), exit, "{code}");
            }
        }
    }
}
