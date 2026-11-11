# Testing

For contributors. The automated suite runs everywhere; the manual checklist
drives real Claude Code sessions and is run by hand.

## Automated

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Tests live in `tests/<area>.rs`, one `mod` per method and scenario, one
`#[test]` per effect (see [STYLEGUIDE.md](../STYLEGUIDE.md)). Every test runs
against its own temporary `CLERK_HOME`; none touches your daemon. `cargo test
--test plugin` checks the plugin's files (hooks, skill) against the code.
The README demo is rebuilt with `scripts/demo/` (see its README).

## Manual checklist

These checks drive real Claude Code sessions; they are not in CI. Run them in
isolation so they never touch your real clerk state or Claude Code settings.
The helper scripts are in `scripts/e2e/`. They take every path from the
environment (`env.sh` lists the variables and their defaults):

- `run-claude.sh`: runs `claude` with the release `clerk` (from
  `CLERK_BIN_DIR`) first on `PATH`, the temp `CLERK_HOME`, the plugin loaded
  for that run only (`--plugin-dir $LECLERK_REPO/integrations/claude-code`)
  and user settings not read (`--setting-sources project`). Nothing is
  installed.
- `tm.sh`: `init` makes the subject project (`T7_SUBJECT`, a git repo) and
  the tmux session; `start <name> <session-uuid|->`, `send`, `cap` and `pid`
  drive interactive sessions in a private tmux server (`tmux -L $T7_TMUX`),
  with `--model haiku`, Bash and Monitor allowed (`--allowedTools=Bash,Monitor`)
  and a `--debug-file` each in `T7_SCRATCH`; `stop` kills the server.
- `clerk-user.sh`: runs that `clerk` as the user against `CLERK_HOME`.

Set-up, once, in the shell you run the checks from:

```sh
cargo build --release
export LECLERK_REPO=$PWD T7=$PWD/scripts/e2e
export CLERK_BIN_DIR=${CARGO_TARGET_DIR:-$PWD/target}/release
export CLERK_HOME=/tmp/clerk-t7          # short: the socket path must stay under 107 bytes
export T7_SCRATCH=/tmp/clerk-t7-scratch T7_TMUX=t7
mkdir -p $CLERK_HOME/config $T7_SCRATCH
echo 'reconnect_grace = 20' > $CLERK_HOME/config/config.toml   # shorter grace for item 4
$T7/tm.sh init
$T7/clerk-user.sh resource create c6-boards --type device --scope user \
  --description "ESP32-C6 dev boards on the test hub" \
  --usage 'Flash with `idf.py -p {unit.path} flash`. Clean = erase flash.' \
  --tag esp32c6 --tag devboard --health exists
touch /tmp/board-a; $T7/clerk-user.sh unit add c6-boards c6-a --path /tmp/board-a
```

`clerk-user.sh` starts the daemon on first use. Interactive sessions start in
`T7_SUBJECT`; Claude Code asks once to trust a new folder there, which it
records in your `~/.claude.json`. To avoid that, point `T7_SUBJECT` at a
directory you already trust, or use `claude -p` (no trust prompt).
Afterwards: exit the sessions, `$T7/clerk-user.sh daemon stop`,
`$T7/tm.sh stop`.

1. **Two sessions, one board: the second queues, then its wait wakes it.**
   Start sessions A and B (`tm.sh start A -`, `tm.sh start B -`). Ask A to
   lease `c6-boards`; ask B the same. B reports "queued at position 1" and
   runs `clerk wait` in the background. Ask A to release. Expect B, without
   a prompt, to read the wait's output, with the offer already claimed, and
   report the grant.
2. **An idle session is woken by an expiry warning.** Let a session lease
   for `2m` (the warning comes at 20 % of the duration, at most 5 min) with a
   wait running (queue a second request, or lease `--interruptible`: a plain
   lease needs no wait), then leave it idle. Expect it to wake about 24 s
   before the end and extend, release, or ask.
3. **`/clear` releases.** A session holds a lease; type `/clear`. Expect
   `clerk leases` to show the lease gone at once, and the new conversation's
   first answer to know it was released (the SessionStart brief).
3b. **After `/clear` the new conversation's leases belong to the new
   session.** Start a session (`tm.sh start A -`) and ask it to lease
   `c6-boards`. Run `$T7/clerk-user.sh leases --json` and note the lease's
   holder session id. Type `/clear` in the session and ask it to lease
   `c6-boards` again. Expect the lease to be granted, and `clerk-user.sh leases
   --json` to show a different session id than before (the old lease gone).
   Quit the session (`/exit`) and expect `leases` to be empty at once, not
   after `reconnect_grace`. If the Bash tool keeps the old session id after
   `/clear`, the acquire is refused with `session_not_hosted`: that is a
   failure. **Not yet run.**
4. **Killing a session frees its board after the grace; `--continue` within
   it keeps it.** A session holds a lease; `kill -9` its `claude` process
   (`tm.sh pid A`). Expect `clerk leases` to keep the lease for
   `reconnect_grace`, then the unit free and dirty (`holder_gone`). Again,
   but start `claude --continue` within the grace: the lease stays, and the
   brief lists it.
5. **The Stop hook reminder appears once.** In `claude -p`, have the agent
   lease something interruptibly (`--interruptible`) and end its turn
   without a wait. Expect exactly one Stop block whose reason starts "Le
   Clerk: this session has 1 live lease but no clerk wait is armed", and a
   second Stop (with `stop_hook_active`) that lets the turn end.
6. **Registration follows the skill.** Make fake device files (e.g.
   `/tmp/hub/serial/by-id/usb-Espressif_…-if00`, two of a kind plus one
   other) and ask: "Register the boards on my hub", saying where they are.
   Expect the agent to read the clerk skill's `registering.md`, run
   `clerk resources` first, group identical boards into one resource, write
   a description, usage with `{unit.path}` placeholders, and tags, pass
   `--scope user` (hardware belongs in user scope), and add one unit per
   device with stable paths.
7. **Plugin load.** With `--debug-file`, check the log for `Loading hooks
   from plugin: leclerk`, no hook schema error, the `clerk` skill listed,
   and `SessionEnd:… [clerk hook session-end] completed`.

### Results

Not yet run: items 1 to 7 and 3b. The last full run (2026-11-10, Claude Code
2.1.321, Haiku) predates the review fixes of that day (soft expiry, the
session-hosting check, daemon stop and restart, the hook wrapper), so it
says nothing about the current code. Record a run here with its date and
Claude Code version, and mark only the items you ran. Not covered by this
checklist: the desktop app, VS Code and real hardware.
