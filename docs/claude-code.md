# Le Clerk in Claude Code

The `leclerk` plugin connects Claude Code sessions to the clerk. Agents use
the `clerk` command from their Bash tool; there is no MCP server. The plugin
brings:

- one skill, `clerk`: how to find, lease, hold and hand back shared things,
  keep a wait armed for notices, and register new resources (its
  `SKILL.md` is short and points to `leasing.md` and `registering.md`);
- four hooks that report the session's Claude Code process to the clerk,
  brief new conversations, remind agents to arm a wait and show pending
  notices (see [What the hooks do](#what-the-hooks-do)).

The plugin lives in `integrations/claude-code/`; the repository root is also
a plugin marketplace named `leclerk` (`.claude-plugin/marketplace.json`).

## Install

Checked with Claude Code 2.1.321.

1. **Build the clerk** from the repository root:

   ```sh
   cargo build --release
   ```

2. **Put `clerk` on `PATH`.** Agents and hooks run the bare command
   `clerk`, so it must be on the `PATH` of whatever starts Claude Code (your
   shell, or the desktop app's environment). For example:

   ```sh
   install -m 755 target/release/clerk ~/.local/bin/clerk
   # or: cargo install --path . --locked
   clerk --version
   ```

   You don't need to start the daemon: the first `clerk` command starts it.
   Hooks never start it.

   **After installing or upgrading `clerk`, run `clerk daemon restart`** so
   the daemon runs the new build. A daemon of another version keeps serving
   clients as long as it speaks the same protocol; only a protocol change
   makes clients refuse it (`protocol_mismatch`, exit 5, saying to run
   `clerk daemon restart`). Restarting keeps leases and queues; agents whose
   Claude Code still runs keep theirs.

3. **Add the marketplace** (this GitHub repository), from a shell:

   ```sh
   claude plugin marketplace add t3hk0d3/le-clerk
   ```

   or inside a session: `/plugin marketplace add t3hk0d3/le-clerk`. To work
   on the plugin, add your checkout instead (`/path/to/le-clerk`).

4. **Install the plugin**, at the scope you want:

   ```sh
   claude plugin install leclerk@leclerk --scope user      # every project
   claude plugin install leclerk@leclerk --scope project   # this repository only
   ```

   `--scope project` writes `"enabledPlugins": {"leclerk@leclerk": true}` to
   the project's `.claude/settings.json`; collaborators also need the
   marketplace added. Inside a session, `/plugin install leclerk@leclerk`
   asks for the scope. `claude plugin marketplace update leclerk` fetches a
   newer version from GitHub. A plugin from a local marketplace is read in place from
   the checkout (`claude plugin list` shows `Read from: …/integrations/claude-code`),
   so skill and hook edits apply on the next session start.

5. **Restart Claude Code** (running sessions don't pick up new plugins).

### Verify

- `clerk --version` and `clerk daemon status` show the same version.
- `claude plugin list` shows `leclerk@leclerk`, its scope and `✔ enabled`.
- In a session, `/skills` lists `clerk`.
- Ask the session to "list the clerk's resources": it runs
  `clerk resources`.
- `clerk daemon status` shows the daemon running.
- Optional: to stop permission prompts, add `"Bash(clerk:*)"` (and
  `"Monitor"`) to `permissions.allow` in your settings.
- Agents need Claude Code 2.1.214 or later (it sets `CLAUDE_PID` for Bash
  commands); older versions get `agent_unidentified` when they lease.

### Uninstall

```sh
claude plugin uninstall leclerk@leclerk --scope user     # or --scope project
claude plugin marketplace remove leclerk
clerk daemon stop                                       # optional
rm ~/.local/bin/clerk                                   # optional
```

The clerk's own state (inventory, audit log) stays in its state directory
until you delete it.

## What the hooks do

All four hooks run `clerk hook <event>`. They do nothing unless Claude
Code's variables (`CLAUDE_CODE_SESSION_ID`, `CLAUDE_PID`) are set, read
Claude Code's hook input from stdin, ask the daemon about the session, and
**fail open**: if the daemon isn't running, is slow or answers with an error,
the hook exits 0 with no output and the agent carries on. They never start
the daemon. Budget: 2 s for the first three, 1.5 s for SessionEnd (Claude
Code's limit).

| Event | What it does |
|-------|--------------|
| `SessionStart` | Reports the Claude Code process now running the session (which leaves the process's previous session, e.g. on `/clear`). For `clear`, `compact` and `resume`, adds a brief to the context: what the session holds or waits for, whether a wait is armed, and what the previous conversation released. |
| `Stop` | When the session holds a lease or a queued request but no `clerk wait` is armed, blocks the end of the turn once with a reminder to arm one. |
| `PreToolUse` | Before a tool call, adds notices no hook has shown yet (offers, expiry warnings, yield requests, cleanup windows) to the agent's context. Skipped inside subagents. |
| `SessionEnd` | Releases the session's leases and queued requests at once (`/clear`, quitting, `/resume` of another conversation), when this process runs the session. |

### Sessions and leases

- Leases belong to the Claude Code **session** (`CLAUDE_CODE_SESSION_ID`).
  Every `clerk` call and hook also reports the Claude Code process running
  it (`CLAUDE_PID`); a session is alive while that process runs it.
- Subagents run with their parent's session ID, so they share its leases.
- `/clear` starts a new session: SessionEnd releases the old one's leases at
  once, and the new conversation's brief says so.
- A crash or `kill -9` runs no hook: the session's leases are kept for
  `reconnect_grace` (60 s), so `claude --continue` or `--resume` in that time
  keeps them. After a normal quit, `--continue` starts with nothing held.
- Notices reach the agent through a wait: the Monitor tool on
  `clerk wait --follow` (each notice is a Monitor event; the watch expires
  after at most 30 minutes and the agent re-arms it), or `clerk wait` run in
  the background (Claude Code wakes the agent when it completes). Either
  claims offers for the agent and ends when the session's process exits or
  moves to another session. Only one wait per session.

### Trust model

The daemon's socket (`clerk.sock`, in a `0700` directory) is reachable only by
your user. Inside that boundary there is no further authentication: any
process running as you can call the daemon's RPCs or run `clerk hook …` with
a made-up session ID or `CLAUDE_PID`. The clerk protects agents from each
other's mistakes, not from a hostile process under your account.

### What the model sees

In an agent session (stdout not a terminal) every `clerk` command prints
**labelled Markdown** rendered from its JSON: every field, times as local
date and time plus how far away, the usage as its own section, a heading
for grants, queued requests, leases and notices, then what happened
(including "tell the user" lines such as a `clerk approve` command) and a
last line `Next: …`. Errors print `## Error: <code>` and a message ending
with the next step, on stdout. `--json` or `CLERK_AGENT_FORMAT=json` prints
the JSON instead (with `summary` and `next`). The format is described in
`docs/cli-output.md`, "Agent sessions".

## Manual checklist

These checks drive real Claude Code sessions; they are not in CI. Run them in
isolation so they never touch your real clerk state or Claude Code settings.
The helper scripts are in `scripts/e2e/`
(`T7` below). They take every path from the environment (`env.sh` lists the
variables and their defaults):

- `run-claude.sh`: runs `claude` with the release `clerk` (from
  `CLERK_BIN_DIR`) first on `PATH`, the temp `CLERK_HOME`, the plugin loaded
  for that run only (`--plugin-dir $LECLERK_REPO/integrations/claude-code`)
  and user settings not read (`--setting-sources project`). Nothing is
  installed.
- `tm.sh`: `init` makes the subject project (`T7_SUBJECT`, a git repo) and
  the tmux session; `start`, `send`, `cap` and `pid` drive interactive
  sessions in a private tmux server (`tmux -L $T7_TMUX`), with `--model
  haiku`, Bash and Monitor allowed (`--allowedTools=Bash,Monitor`)
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
echo 'reconnect_grace = 20' > $CLERK_HOME/config/config.toml   # shorter grace for item 3
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
   Start sessions A and B (`tm.sh start A`). Ask A to lease `c6-boards`;
   ask B the same. B reports "queued at position 1" and arms Monitor on
   `clerk wait --follow`. Ask A to release. Expect B, without a prompt, to
   get a Monitor event with the offer already claimed and report the grant.
2. **An idle session is woken by an expiry warning.** Let a session lease
   for `2m` (the warning comes at 20 % of the duration, at most 5 min) with a
   wait armed, then leave it idle. Expect it to wake about 24 s before the
   end and extend, release, or ask.
3. **`/clear` releases.** A session holds a lease; type `/clear`. Expect
   `clerk leases` to show the lease gone at once, and the new conversation's
   first answer to know it was released (the SessionStart brief).
4. **Killing a session frees its board after the grace; `--continue` within
   it keeps it.** A session holds a lease; `kill -9` its `claude` process
   (`tm.sh pid A`). Expect `clerk leases` to keep the lease for
   `reconnect_grace`, then the unit free and dirty (`holder_gone`). Again,
   but start `claude --continue` within the grace: the lease stays, and the
   brief lists it.
5. **The Stop hook reminder appears once.** In `claude -p`, have the agent
   lease something and end its turn without a wait. Expect exactly one Stop
   block whose reason starts "Le Clerk: this session has 1 live lease but no
   clerk wait is armed", and a second Stop (with `stop_hook_active`) that
   lets the turn end.
6. **Registration follows the skill.** Make fake device files (e.g.
   `/tmp/hub/serial/by-id/usb-Espressif_…-if00`, two of a kind plus one
   other) and ask: "Register the boards on my hub", saying where they are.
   Expect the agent to read the clerk skill's `registering.md`, run
   `clerk resources` first, group identical boards into one resource, write
   a description, usage with `{unit.path}` placeholders, and tags, leave
   `--scope` out (project scope), and add one unit per device with stable
   paths.
7. **Plugin load.** With `--debug-file`, check the log for `Loading hooks
   from plugin: leclerk`, no hook schema error, the `clerk` skill listed,
   and `SessionEnd:… [clerk hook session-end] completed`.

### Results

Last run 2026-11-10 (Claude Code 2.1.321, Haiku, isolated): items 1, 3, 4,
5 and 6 passed. Items 2 (an idle session woken by an expiry warning) and 7
(plugin load) were not re-run since the agent interface moved to the CLI.
Not covered: the desktop app, VS Code and real hardware.
