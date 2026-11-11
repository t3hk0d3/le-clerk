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
   `clerk daemon restart`); waits that are running end with
   `protocol_mismatch` too. The current protocol is 5. Restarting keeps leases
   and queues; agents whose Claude Code still runs keep theirs.

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
   asks for the scope. To update the plugin, run
   `claude plugin marketplace update leclerk` and then
   `claude plugin update leclerk@leclerk`, and restart Claude Code. A plugin from a local marketplace is read in place from
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

All four hooks run `clerk hook <event>` through
`integrations/claude-code/hooks/clerk-hook.sh`, a small wrapper that runs
`$CLERK_BIN` (default: `clerk` on `PATH`) and exits 0 without output when
that binary is missing, so a hook never fails for want of it. They do nothing
unless either of Claude Code's variables (`CLAUDE_CODE_SESSION_ID`,
`CLAUDE_PID`) is set (the session itself comes from the hook input), read
Claude Code's hook input from stdin, ask the daemon about the session, and
**fail open**: if the daemon isn't running, is slow or answers with an error,
the hook exits 0 with no output and the agent carries on. They never start
the daemon. Clerk keeps each hook under 2 s (1.5 s for SessionEnd, Claude
Code's limit); `hooks.json` allows 3 s (1.5 s for SessionEnd).

| Event | What it does |
|-------|--------------|
| `SessionStart` | Reports the Claude Code process now running the session (which leaves the process's previous session, e.g. on `/clear`). For `clear`, `compact` and `resume`, adds a brief to the context: what the session holds or waits for, whether a wait is armed, and what the previous conversation released. |
| `Stop` | When the session has a queued request or an interruptible lease but no `clerk wait` is armed, blocks the end of the turn once with a reminder to start one. |
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
- An idle agent is reached through `clerk wait`, run in the background: it
  costs nothing while idle, and Claude Code wakes the agent when it
  completes. It claims offers for the agent and ends when the session's
  process exits or moves to another session. Only one wait per session.
  Agents need one only while queued or holding an interruptible lease; a
  plain lease needs none. (`clerk wait --follow` suits Claude Code's Monitor
  tool, but a Monitor watch expires every 30 minutes and each expiry wakes
  the agent and re-reads its whole conversation, so the skill doesn't use
  it.)
- **Daemon restarts are ridden out.** `clerk daemon restart` tells the
  clients waiting on the daemon that it is restarting; they reconnect to the
  new daemon (starting one themselves if it is not back within a few
  seconds) and carry on, so agents' waits, `acquire --wait` and the live
  view keep going. `clerk daemon stop` tells them it stopped: an agent's
  wait ends cleanly saying so (a stopped daemon leaves a `stopped` marker, so
  waits do not restart it; any other `clerk` command does). A crashed daemon
  leaves no marker, so waits start a new one. A client keeps working with a
  daemon of another version as long as both speak the same protocol; a
  protocol change ends running waits with `protocol_mismatch` until you run
  `clerk daemon restart`.

### Trust model

The daemon's socket (`clerk.sock`, in a `0700` directory) is reachable only by
your user. Inside that boundary there is no further authentication: any
process running as you can call the daemon's RPCs or run `clerk hook …` with
a made-up session ID or `CLAUDE_PID`. The clerk protects agents from each
other's mistakes, not from a hostile process under your account.

**Agent policy.** In an agent session the daemon requires `--override
"<reason>"` for `daemon stop` (but not `daemon restart`), the `admin` tools
(except `export` and a `doctor` without `--fix`) and handing a lease to
another holder; the CLI also asks for it for `approve`, `promotions`, `log`,
`release --force` and taking a unit another holder has. Agents are told to
use it only on the user's word. The reason is asserted by the client, so it
is a speed bump against mistakes, not a security control. An auto-started
daemon and custom health checks run with an allow-listed environment
(`HOME`, `PATH`, `XDG_*`, `CLERK_HOME`, `LANG`, `RUST_LOG`; checks also get the
unit's `CLERK_*` variables), so an agent's variables and your shell's tokens
do not leak into them.

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

## Problems

[troubleshooting.md](troubleshooting.md) covers the daemon log, error codes
and what to do about `session_not_hosted`, `agent_unidentified` and
`protocol_mismatch`. The manual test checklist for contributors is in
[testing.md](testing.md).
