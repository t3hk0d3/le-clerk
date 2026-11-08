# Le Clerk styleguide

The single styleguide for this project. Every task and plan follows it.

## Formatting
- `cargo fmt` with the default config (`rustfmt.toml` is intentionally empty).
- Import convention, checked manually in review: `use` lines are grouped as std,
  then external crates, then crate-local, each group a separate block, with
  module-level granularity (`use std::io;`, not one `use` per item or a deeply
  nested tree). Stable rustfmt cannot enforce this (`group_imports` and
  `imports_granularity` are nightly-only), so reviewers check it.

## Lints
- `cargo clippy --all-targets -- -D warnings`.
- `clippy::pedantic` is warned. Allow a lint per item only, with a comment
  giving the reason.

## Naming
- Follow the Rust API Guidelines.
- Modules are singular nouns.
- Constructors are `new`, `open` or `parse`.
- Boolean fields read as statements (`interruptible`, `retiring`).

## Errors
- Library modules use `thiserror` enums, defined in the module that owns them.
- Only `main.rs` and `cli/*` use `anyhow`.
- No `unwrap()` / `expect()` outside tests, except for proven invariants, with
  a comment stating the invariant.
- API error codes are `snake_case` strings derived from the enum variant.

## Time
- Never call `SystemTime::now()` outside `clock.rs`.

## Async
- tokio. Blocking SQLite work runs only on the engine's thread, never in async
  handlers.

## Logging
- `tracing`. The daemon logs to its log file and the CLI to stderr. Nothing goes
  to stdout except command output.

## Tests
- Unit tests for pure functions go in a `mod tests` in the same file.
- Behaviour tests go in `tests/<area>.rs`: one `mod` per BDD module/method, one
  `mod` per scenario, one `#[test]` per effect.
- Test names describe the effect, e.g.
  `scheduler::acquire::on_free_healthy_unit::returns_granted_with_settings`.
- `CLERK_HOME` redirects all clerk paths; tests build commands through
  `tests/common::TestEnv`, which sets it to a private tempdir.

## Commits
- Conventional Commits: `feat:`, `fix:`, `test:`, `docs:`, `chore:`,
  `refactor:`. One task per commit or fewer.
