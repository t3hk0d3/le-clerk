//! Human rendering shared by every view, as `docs/cli-output.md` sets out:
//! styling, glyphs, holders, projects, times and wrapped text.
//!
//! Renderers always write Unicode glyphs; [`finish`] turns them into ASCII
//! when the terminal cannot show them. Colour is applied by [`Style`] only
//! on a terminal without `NO_COLOR`, and never in JSON mode.

use std::cell::Cell;
use std::io::{self, IsTerminal};
use std::path::Path;
use std::sync::OnceLock;

use crossterm::style::Stylize;
use leclerk::model::ProjectKey;
use serde_json::Value;

/// A free unit.
pub const FREE: &str = "▮";
/// A busy (held or unusable) unit.
pub const BUSY: &str = "▯";
/// An unhealthy or dirty unit.
pub const WARN: &str = "⚠";
/// Between short facts on a line.
pub const SEP: &str = " · ";
/// Starts a top-level block (a resource, lease, request, promotion or log
/// entry).
pub const ITEM: &str = "• ";
/// Starts a nested item (a unit or queue entry).
pub const SUB: &str = "◦ ";

/// Unicode glyphs and their ASCII stand-ins.
const ASCII: [(char, &str); 8] = [
    ('▮', "#"),
    ('▯', "."),
    ('⚠', "!"),
    ('·', "-"),
    ('→', "->"),
    ('…', "..."),
    ('•', "*"),
    ('◦', "-"),
];

/// How human output looks on this terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    /// Escape sequences for bold, cyan and yellow.
    pub color: bool,
    /// Unicode glyphs; ASCII stand-ins otherwise.
    pub unicode: bool,
}

static CURRENT: OnceLock<Style> = OnceLock::new();
static STDERR: OnceLock<Style> = OnceLock::new();

thread_local! {
    /// Set while text for stderr is rendered (see [`on_stderr`]).
    static ON_STDERR: Cell<bool> = const { Cell::new(false) };
}

/// Detect and install the styles of stdout and stderr: each colours only
/// when it is a terminal itself.
pub fn install(json: bool, ascii_flag: bool) {
    let var = |k: &str| std::env::var(k).ok();
    let _ = CURRENT.set(Style::from_env(
        io::stdout().is_terminal(),
        json,
        ascii_flag,
        var,
    ));
    let _ = STDERR.set(Style::from_env(
        io::stderr().is_terminal(),
        json,
        ascii_flag,
        var,
    ));
}

/// Run `render` with [`Style::current`] answering stderr's style: for text
/// that goes to stderr.
pub fn on_stderr<T>(render: impl FnOnce() -> T) -> T {
    let before = ON_STDERR.with(|f| f.replace(true));
    let out = render();
    ON_STDERR.with(|f| f.set(before));
    out
}

impl Style {
    /// The style for an output that is (`tty`) or is not a terminal:
    /// colour only on a terminal, without `NO_COLOR` and without `--json`;
    /// Unicode unless `--ascii` or a locale that is not UTF-8.
    #[must_use]
    pub fn from_env(
        tty: bool,
        json: bool,
        ascii_flag: bool,
        var: impl Fn(&str) -> Option<String>,
    ) -> Self {
        let no_color = var("NO_COLOR").is_some_and(|v| !v.is_empty());
        // The first of these that is set decides the character set.
        let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
            .iter()
            .find_map(|k| var(k).filter(|v| !v.is_empty()));
        let utf8 = locale.is_some_and(|l| {
            let l = l.to_ascii_lowercase();
            l.contains("utf-8") || l.contains("utf8")
        });
        Self {
            color: tty && !json && !no_color,
            unicode: utf8 && !ascii_flag,
        }
    }

    /// The installed style (stderr's inside [`on_stderr`]); plain Unicode
    /// without colour when none is installed.
    #[must_use]
    pub fn current() -> Self {
        let slot = if ON_STDERR.with(Cell::get) {
            &STDERR
        } else {
            &CURRENT
        };
        slot.get().copied().unwrap_or(Self {
            color: false,
            unicode: true,
        })
    }

    /// A resource id: bold.
    #[must_use]
    pub fn id(self, text: &str) -> String {
        if self.color {
            text.bold().to_string()
        } else {
            text.to_owned()
        }
    }

    /// One tag: bold cyan.
    #[must_use]
    pub fn tag(self, text: &str) -> String {
        if self.color {
            text.cyan().bold().to_string()
        } else {
            text.to_owned()
        }
    }

    /// A warning: `⚠ text`, yellow.
    #[must_use]
    pub fn warn(self, text: &str) -> String {
        let text = format!("{WARN} {text}");
        if self.color {
            text.yellow().to_string()
        } else {
            text
        }
    }
}

/// Tags, each styled, separated by spaces.
#[must_use]
pub fn tags(list: &[String], style: Style) -> String {
    list.iter()
        .map(|t| style.tag(t))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The tags of a JSON list.
#[must_use]
pub fn tags_of(list: &Value, style: Style) -> String {
    let list: Vec<String> = list
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t.as_str().map(str::to_owned))
        .collect();
    tags(&list, style)
}

/// Text as the terminal can show it: with ASCII stand-ins for the glyphs
/// (and `?` for any other non-ASCII character) unless it shows Unicode.
#[must_use]
pub fn finish(text: &str, style: Style) -> String {
    if style.unicode {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match ASCII.iter().find(|(u, _)| *u == c) {
            Some((_, a)) => out.push_str(a),
            None if c.is_ascii() => out.push(c),
            None => out.push('?'),
        }
    }
    out
}

/// A holder: `cli` (a user lease), `pid N`, `agent <8 chars>`, else
/// `agent in another project`.
#[must_use]
pub fn holder(h: &Value) -> String {
    match h["kind"].as_str() {
        Some("user") => "cli".to_owned(),
        Some("cli_process") => format!("pid {}", h["pid"]),
        Some("agent_session") => {
            let id = h["session_id"].as_str().unwrap_or_default();
            format!("agent {}", id.chars().take(8).collect::<String>())
        }
        _ => "agent in another project".to_owned(),
    }
}

/// A project as its repository folder, `~` for the home directory.
#[must_use]
pub fn project(key: &ProjectKey) -> String {
    project_path(&key.0)
}

/// [`project`] for a path: a `.git` directory stands for its parent.
#[must_use]
pub fn project_path(path: &Path) -> String {
    let folder = if path.file_name().is_some_and(|n| n == ".git") {
        path.parent().unwrap_or(path)
    } else {
        path
    };
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty());
    if let Some(home) = home
        && let Ok(rest) = folder.strip_prefix(Path::new(&home))
    {
        return if rest.as_os_str().is_empty() {
            "~".to_owned()
        } else {
            format!("~/{}", rest.display())
        };
    }
    folder.display().to_string()
}

/// A scope: `user` or `project <folder>`.
#[must_use]
pub fn scope(scope: &Value) -> String {
    match scope.get("project").and_then(Value::as_str) {
        Some(path) => format!("project {}", project(&ProjectKey(path.into()))),
        None => scope.as_str().unwrap_or("user").to_owned(),
    }
}

/// How far `at_ms` is from `now_ms`, rounded in its largest unit:
/// `in 27m`, `9s ago`; `just now` within a second either way (a time the
/// daemon wrote a moment ago must not read as the future).
#[must_use]
pub fn when(at_ms: i64, now_ms: i64) -> String {
    let delta = at_ms - now_ms;
    if delta.unsigned_abs() < 1000 {
        return "just now".to_owned();
    }
    let span = span_ms(delta.unsigned_abs());
    if delta >= 0 {
        format!("in {span}")
    } else {
        format!("{span} ago")
    }
}

/// A span of seconds in its largest unit: `45s`, `27m`, `2h`, `3d`.
#[must_use]
pub fn span(secs: u64) -> String {
    span_ms(secs.saturating_mul(1000))
}

/// A span of milliseconds, rounded to the nearest whole largest unit.
fn span_ms(ms: u64) -> String {
    let round = |unit: u64| ms.saturating_add(unit / 2) / unit;
    if round(1000) < 60 {
        format!("{}s", round(1000))
    } else if round(60_000) < 60 {
        format!("{}m", round(60_000))
    } else if round(3_600_000) < 24 {
        format!("{}h", round(3_600_000))
    } else {
        format!("{}d", round(86_400_000))
    }
}

/// The local date and time of `at_ms`: `2026-11-10 14:32`.
#[must_use]
pub fn date_time(at_ms: i64) -> String {
    let secs: libc::time_t = at_ms.div_euclid(1000);
    // SAFETY: as in `clock`.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&raw const secs, &raw mut tm).is_null() {
            return "?".to_owned();
        }
        tm
    };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

/// The local clock time of `at_ms`: `14:32`.
#[must_use]
pub fn clock(at_ms: i64) -> String {
    let secs: libc::time_t = at_ms.div_euclid(1000);
    // SAFETY: an all-zero `tm` is a valid value, and `localtime_r` writes
    // into it only, reading `secs`; both live for the whole call.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&raw const secs, &raw mut tm).is_null() {
            return "?".to_owned();
        }
        tm
    };
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

/// The terminal's width: its real size, else `COLUMNS`, else 80.
#[must_use]
pub fn width() -> usize {
    let columns = || {
        std::env::var("COLUMNS")
            .ok()
            .and_then(|c| c.trim().parse::<usize>().ok())
            .filter(|c| *c > 0)
    };
    match crossterm::terminal::size() {
        Ok((cols, _)) if cols > 0 => usize::from(cols),
        _ => columns().unwrap_or(80),
    }
    .max(20)
}

/// The longest resource or unit id (see `model::ids`).
const ID_MAX: usize = 63;

/// `text`'s width in columns: its characters, escape sequences left out.
#[must_use]
pub fn visible_len(text: &str) -> usize {
    let mut len = 0;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // `ESC [ … m`: skip to the final letter.
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            len += 1;
        }
    }
    len
}

/// True when `word` may be split across lines: never an id (a lease,
/// request or promotion id, anything as short as a resource or unit id),
/// a path or `resource/unit`, or styled text.
fn breakable(word: &str) -> bool {
    let bare = word.trim_start_matches(['"', '`', '(', '\'']);
    !(word.contains('\x1b')
        || word.contains('/')
        || ["lse_", "req_", "prm_"].iter().any(|p| bare.starts_with(p))
        || visible_len(word) <= ID_MAX)
}

/// `text` wrapped to `width` columns (escape sequences take none), every
/// line starting with `indent` spaces. Ids and paths are moved whole to the
/// next line, even when longer than a line; other over-long words are
/// broken. No trailing newline.
#[must_use]
pub fn wrap(text: &str, indent: usize, width: usize) -> String {
    let room = width.saturating_sub(indent).max(10);
    let pad = " ".repeat(indent);
    let mut lines: Vec<String> = Vec::new();
    for paragraph in text.lines() {
        let mut line = String::new();
        let mut len = 0;
        for word in paragraph.split_whitespace() {
            let mut word = word.to_owned();
            // The rest of a word already broken is no id: break it further.
            let mut broken = false;
            loop {
                let gap = usize::from(len > 0);
                let wide = visible_len(&word);
                if len + gap + wide <= room {
                    if gap == 1 {
                        line.push(' ');
                    }
                    line.push_str(&word);
                    len += gap + wide;
                    break;
                }
                if len > 0 {
                    lines.push(std::mem::take(&mut line));
                    len = 0;
                    continue;
                }
                if !broken && !breakable(&word) {
                    // Alone on its line, overflowing.
                    line = word;
                    len = wide;
                    break;
                }
                let head: String = word.chars().take(room).collect();
                let rest: String = word.chars().skip(room).collect();
                lines.push(head);
                word = rest;
                broken = true;
            }
        }
        lines.push(line);
    }
    lines
        .iter()
        .map(|l| format!("{pad}{l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every line of `text` indented by `indent` spaces, as written.
#[must_use]
pub fn indent(text: &str, indent: usize) -> String {
    let pad = " ".repeat(indent);
    text.lines()
        .map(|l| format!("{pad}{l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `"text"`.
#[must_use]
pub fn quoted(text: &str) -> String {
    format!("\"{text}\"")
}

/// `id`, cut to its first 12 characters and `…` unless `wide`.
#[must_use]
pub fn short_id(id: &str, wide: bool) -> String {
    if wide || id.chars().count() <= 12 {
        id.to_owned()
    } else {
        format!("{}…", id.chars().take(12).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            vars.iter()
                .find(|(name, _)| *name == k)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    mod style {
        use super::*;

        mod detect {
            use super::*;

            mod on_a_tty_without_no_color {
                use super::*;

                #[test]
                fn colours() {
                    let s = Style::from_env(true, false, false, env(&[("LANG", "en_US.UTF-8")]));
                    assert!(s.color);
                }

                #[test]
                fn uses_unicode_on_a_utf8_locale() {
                    let s = Style::from_env(true, false, false, env(&[("LANG", "C.utf8")]));
                    assert!(s.unicode);
                }
            }

            mod on_no_color {
                use super::*;

                #[test]
                fn does_not_colour() {
                    let s = Style::from_env(true, false, false, env(&[("NO_COLOR", "1")]));
                    assert!(!s.color);
                }
            }

            mod on_json {
                use super::*;

                #[test]
                fn does_not_colour() {
                    let s = Style::from_env(true, true, false, env(&[]));
                    assert!(!s.color);
                }
            }

            mod on_no_tty {
                use super::*;

                #[test]
                fn does_not_colour() {
                    let s = Style::from_env(false, false, false, env(&[]));
                    assert!(!s.color);
                }
            }

            mod on_ascii_flag {
                use super::*;

                #[test]
                fn uses_ascii() {
                    let s = Style::from_env(true, false, true, env(&[("LANG", "C.UTF-8")]));
                    assert!(!s.unicode);
                }
            }

            mod on_a_non_utf8_locale {
                use super::*;

                #[test]
                fn uses_ascii() {
                    let vars = [("LC_ALL", "C"), ("LANG", "en_US.UTF-8")];
                    let s = Style::from_env(true, false, false, env(&vars));
                    assert!(!s.unicode);
                }
            }
        }

        mod finish {
            use super::*;

            mod on_ascii {
                use super::*;

                #[test]
                fn replaces_glyphs_with_ascii_stand_ins() {
                    let s = Style {
                        color: false,
                        unicode: false,
                    };
                    assert_eq!(finish("• ◦ ▮▯ ⚠ a · b → c…", s), "* - #. ! a - b -> c...");
                }

                #[test]
                fn replaces_other_non_ascii_with_a_question_mark() {
                    let s = Style {
                        color: false,
                        unicode: false,
                    };
                    assert_eq!(finish("café", s), "caf?");
                }
            }
        }

        mod tags {
            use super::*;

            #[test]
            fn without_colour_joins_them_with_spaces() {
                let s = Style {
                    color: false,
                    unicode: true,
                };
                assert_eq!(tags(&["a".into(), "b".into()], s), "a b");
            }

            #[test]
            fn with_colour_makes_each_bold_cyan() {
                let s = Style {
                    color: true,
                    unicode: true,
                };
                let expected = format!("{} {}", "a".cyan().bold(), "b".cyan().bold());
                assert_eq!(tags(&["a".into(), "b".into()], s), expected);
            }
        }
    }

    mod holder {
        use super::*;

        #[test]
        fn a_user_holder_is_cli() {
            assert_eq!(holder(&json!({"kind": "user", "tag": "01ABC"})), "cli");
        }

        #[test]
        fn a_cli_process_is_its_pid() {
            let h = json!({"kind": "cli_process", "pid": 130_150, "start_time": 5});
            assert_eq!(holder(&h), "pid 130150");
        }

        #[test]
        fn an_agent_is_its_first_8_characters() {
            let h = json!({"kind": "agent_session", "session_id": "3f2a9c1e-77aa"});
            assert_eq!(holder(&h), "agent 3f2a9c1e");
        }

        #[test]
        fn anything_else_is_an_agent_in_another_project() {
            assert_eq!(holder(&Value::Null), "agent in another project");
        }
    }

    mod project {
        use super::*;

        #[test]
        fn a_git_dir_is_shown_as_its_repo_folder() {
            let key = ProjectKey(PathBuf::from("/srv/code/app/.git"));
            assert_eq!(project(&key), "/srv/code/app");
        }

        #[test]
        fn another_dir_is_shown_as_it_is() {
            let key = ProjectKey(PathBuf::from("/srv/code/app"));
            assert_eq!(project(&key), "/srv/code/app");
        }

        #[test]
        fn home_is_shown_as_tilde() {
            let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) else {
                return;
            };
            let key = ProjectKey(Path::new(&home).join("dev/leclerk/.git"));
            assert_eq!(project(&key), "~/dev/leclerk");
        }
    }

    mod relative_time {
        use super::*;

        #[test]
        fn a_future_time_reads_in_its_largest_unit_rounded() {
            assert_eq!(when(27 * 60_000 + 20_000, 0), "in 27m");
        }

        #[test]
        fn a_past_time_reads_ago() {
            assert_eq!(when(0, 9_400), "9s ago");
        }

        #[test]
        fn hours_read_whole() {
            assert_eq!(when(2 * 3_600_000 - 1, 0), "in 2h");
        }

        #[test]
        fn days_read_whole() {
            assert_eq!(when(0, 3 * 86_400_000), "3d ago");
        }
    }

    mod wrap {
        use super::*;

        #[test]
        fn breaks_lines_between_words_within_the_width() {
            assert_eq!(wrap("aaa bbb ccc", 2, 12), "  aaa bbb\n  ccc");
        }

        #[test]
        fn takes_no_width_for_escape_sequences() {
            let id = "ab".bold().to_string();
            assert_eq!(wrap(&format!("{id} cd ef"), 0, 10), format!("{id} cd ef"));
        }

        #[test]
        fn moves_a_styled_id_whole_to_the_next_line() {
            let id = "esp32-c6-devkit-bench".bold().to_string();
            assert_eq!(
                wrap(&format!("see {id} now"), 2, 20),
                format!("  see\n  {id}\n  now")
            );
        }

        #[test]
        fn never_splits_a_lease_id() {
            let text = "held as lse_01M71FRGTW9BC1FSABQRDH7TZ1 here";
            assert_eq!(
                wrap(text, 0, 20),
                "held as\nlse_01M71FRGTW9BC1FSABQRDH7TZ1\nhere"
            );
        }

        #[test]
        fn never_splits_a_path() {
            let path = format!("/dev/serial/by-id/{}", "x".repeat(70));
            assert_eq!(wrap(&format!("at {path}"), 0, 30), format!("at\n{path}"));
        }

        #[test]
        fn breaks_a_word_longer_than_a_line() {
            let text = "x".repeat(70);
            let wrapped = wrap(&text, 0, 30);
            assert_eq!(
                wrapped,
                format!("{}\n{}\n{}", "x".repeat(30), "x".repeat(30), "x".repeat(10))
            );
        }
    }

    mod short_id {
        use super::*;

        #[test]
        fn cuts_to_12_characters() {
            assert_eq!(short_id("req_01M71FTC912K", false), "req_01M71FTC…");
        }

        #[test]
        fn wide_keeps_it_whole() {
            assert_eq!(short_id("req_01M71FTC912K", true), "req_01M71FTC912K");
        }
    }

    mod just_now {
        use super::*;

        #[test]
        fn a_time_within_a_second_is_just_now() {
            assert_eq!(when(1_000_300, 1_000_000), "just now");
            assert_eq!(when(999_700, 1_000_000), "just now");
            assert_eq!(when(1_000_000, 1_000_000), "just now");
        }

        #[test]
        fn later_times_keep_their_span() {
            assert_eq!(when(1_005_000, 1_000_000), "in 5s");
            assert_eq!(when(995_000, 1_000_000), "5s ago");
        }
    }
}
