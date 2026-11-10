# Le Clerk in Claude Code

The `leclerk` plugin connects Claude Code sessions to the clerk. It brings:

- the clerk's MCP server (`clerk mcp`, shown as `plugin:leclerk:clerk`) with
  21 tools: inventory (`type_list`, `resource_*`, `unit_*`) and leasing
  (`lease_*`, including `lease_wait`);
- four hooks that keep agents listening for lease notices (see
  [What the hooks do](#what-the-hooks-do));
- two skills: `leasing` (how to take, hold and hand back shared things) and
  `registering-resources` (how to register boards, ports, services and
  tokens).

The plugin lives in `integrations/claude-code/`; the repository root is also
a plugin marketplace named `leclerk` (`.claude-plugin/marketplace.json`).

## Install

Checked with Claude Code 2.1.321.

1. **Build the clerk** from the repository root:

   ```sh
   cargo build --release
   ```

2. **Put `clerk` on `PATH`.** The plugin runs the bare command `clerk`
   (`clerk mcp`, `clerk hook …`), so it must be on the `PATH` of whatever
   starts Claude Code (your shell, or the desktop app's environment). For
   example:

   ```sh
   install -m 755 target/release/clerk ~/.local/bin/clerk
   # or: cargo install --path . --locked
   clerk --version
   ```

   You don't need to start the daemon: the MCP server starts it on first use.
   Hooks never start it.

   **After installing or upgrading `clerk`, run `clerk daemon restart`.** A
   daemon left running from an older build is refused by every client (each
   checks the daemon's version and protocol): `clerk` commands exit 5 with
   `version_mismatch`, and the plugin's MCP server answers every request with
   an error saying to run `clerk daemon restart`. Restarting keeps leases and
   queues: agent sessions get `reconnect_grace` to reconnect.

3. **Add the marketplace** (the repository checkout), from a shell:

   ```sh
   claude plugin marketplace add /path/to/leclerk
   ```

   or inside a session: `/plugin marketplace add /path/to/leclerk`.

4. **Install the plugin**, at the scope you want:

   ```sh
   claude plugin install leclerk@leclerk --scope user      # every project
   claude plugin install leclerk@leclerk --scope project   # this repository only
   ```

   `--scope project` writes `"enabledPlugins": {"leclerk@leclerk": true}` to
   the project's `.claude/settings.json`; collaborators also need the
   marketplace added. Inside a session, `/plugin install leclerk@leclerk`
   asks for the scope. A plugin from a local marketplace is read in place from
   the checkout (`claude plugin list` shows `Read from: …/integrations/claude-code`),
   so skill and hook edits apply on the next session start.

5. **Restart Claude Code** (running sessions don't pick up new plugins).

### Verify

- `clerk --version` and `clerk daemon status` show the same version.
- `claude plugin list` shows `leclerk@leclerk`, its scope and `✔ enabled`.
- In a session, `/mcp` shows `plugin:leclerk:clerk` as connected.
- Ask the session to "list the clerk's resources": it calls `resource_list`.
- `clerk daemon status` shows the daemon running.
- Optional: to stop permission prompts for the clerk's tools, add
  `"mcp__plugin_leclerk_clerk"` to `permissions.allow` in your settings.

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

All four hooks run `clerk hook <event>`. They read Claude Code's hook input
from stdin, ask the daemon about the session, and **fail open**: if the daemon
isn't running, is slow or answers with an error, the hook exits 0 with no
output and the agent carries on. They never start the daemon. Budget: 2 s for
the first three, 1.5 s for SessionEnd (Claude Code's limit).

| Hook | What it does |
|---|---|
| `SessionStart` | On a resumed session (`--resume`, `--continue`), hands the session's leases and queued requests over to the new MCP connection, retrying until it connects (also while the MCP server is still starting the daemon). Otherwise nothing. |
| `Stop` | When the session holds a lease or a queued request but has no `lease_wait` running, blocks the end of the turn once with a reminder to call `lease_wait`. Claude Code shows it as "Stop hook error: Le Clerk: …". |
| `PreToolUse` | Before a tool call, adds notices no hook has shown yet (offers, expiry warnings, yield requests, cleanup windows) to the agent's context. They stay pending until a `lease_wait` acknowledges them. Skipped inside subagents. |
| `SessionEnd` | Ends the session's leases and requests at once. On `/clear` or `/resume` (the Claude Code process lives on) it does nothing while the MCP connection is up. |

### Sessions and leases

- **`/exit` or closing the terminal** (SIGHUP) runs SessionEnd: the
  session's leases end at once, unconfirmed (the unit is marked dirty unless
  the agent confirmed cleanup).
- **A crash or `kill -9`** (no SessionEnd): the session counts as alive for a
  grace period after its MCP connection drops (`reconnect_grace`, 60 s by
  default), then its leases end with cause `holder_gone`.
- **`claude --resume <id>` or `claude --continue` within that grace** keeps
  the session's leases and queue places. After `--resume`, a `lease_wait`
  task the old process had running was picked up again and delivered its
  expiry warning; after `--continue` such a task showed as `reconnecting…`
  (not checked further), so let the agent re-arm `lease_wait` when in doubt.
- After a clean exit there is nothing left to resume: the leases already
  ended.
- `clerk` run from the agent's shell (the Bash tool, where
  `CLAUDE_CODE_SESSION_ID` is set) acts as the agent's session, following
  resume aliases: after `--continue`, `clerk release <lease>` releases the
  lease the resumed session took over, and a new `clerk acquire` lands on the
  live session. `clerk wait` refuses to run there (exit 2): the MCP tool
  `lease_wait` owns the session's notices.

### Trust model

The daemon's socket (`clerk.sock`, in a `0700` directory) is reachable only by
your user. Inside that boundary there is no further authentication: any
process running as you can call the daemon's RPCs, run `clerk hook …` with a
made-up `session_id` or `CLAUDE_PID`, or keep a session alive. The clerk
protects agents from each other's mistakes, not from a hostile process under
your account.

### What the model sees

Claude Code 2.1.321 hands the model a tool's structured content (the clerk's
JSON) and not the short text block beside it. So everything the text says is
in the JSON too, on every result, task result and error:

- `next`: the next step the clerk expects (the text block's last sentence),
  e.g. "Call lease_confirm when the unit is clean.";
- `summary`: the text block's other lines, including any "Tell the user …
  `clerk approve <id>`" line about a similar unit in another project;
- `ack` (on `lease_wait` results, and on a queued `lease_acquire` task's
  final result): exactly the notice IDs to pass as `ack` to the next
  `lease_wait`.

For a tool error Claude Code shows the model the text block instead
("`code`: message (details). Next step."), which states the same summary line
and next step; the structured content of an error (`error` with `code`,
`message`, `details`, beside `next` and `summary`) is for other MCP clients.
The skills tell agents to follow the next step and pass `ack` on.

`/clear` gives the conversation a new session ID (the Bash tool and the hooks
see it) while the clerk's MCP server keeps the one it started with, so the
leases stay with the MCP server's ID. Two consequences, as of this version:
`clerk` run from the agent's shell after `/clear` acts as the new ID and does
not see the session's leases (use the MCP tools), and after `/clear` followed
by a crash and `claude --continue` the leases are not handed over and end
after `reconnect_grace`.

## Manual checklist

These checks drive real Claude Code sessions; they are not in CI. Run them in
isolation so they never touch your real clerk state or Claude Code settings.
The helper scripts are in `docs/superpowers/spikes/2026-11-10-p3/task7/`
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
  haiku`, the clerk's tools allowed (`--allowedTools=mcp__plugin_leclerk_clerk`)
  and a `--debug-file` each in `T7_SCRATCH`; `stop` kills the server.
- `clerk-user.sh`: runs that `clerk` as the user against `CLERK_HOME`.

Set-up, once, in the shell you run the checks from:

```sh
cargo build --release
export LECLERK_REPO=$PWD T7=$PWD/docs/superpowers/spikes/2026-11-10-p3/task7
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

1. **Two sessions, one board: the second queues, then its `lease_wait` wakes
   it.** Start sessions A and B (`tm.sh start A`). Ask A to lease
   `c6-boards` and arm `lease_wait`; ask B the same. B reports "queued at
   position 1". Ask A to release and confirm. Expect B, without a prompt, to
   show `MCP task … (lease_wait) completed.`, report the grant and re-arm
   `lease_wait`.
2. **An idle session is woken by an expiry warning.** Let a session lease for
   `2m` (warning lead is 20 % of the duration, at most 5 min) and arm
   `lease_wait`, then leave it idle. Expect it to wake about 24 s before the
   end with the expiry warning and act on it (extend, release, or ask).
3. **Killing a session frees its board after the grace.** A session holds a
   lease; `kill -9` its `claude` process (`tm.sh pid A`). Expect `clerk
   leases` to keep the lease for `reconnect_grace`, then `clerk status` shows
   the unit free and dirty with reason `holder_gone`.
4. **`--resume` and `--continue` keep the session's leases.** Start a
   session with `--session-id <uuid>`, lease and arm `lease_wait`, `kill -9`
   it, and start `claude --resume <uuid>` within the grace. After the grace,
   `clerk leases` still shows the lease under that session ID, and
   `lease_status` in the resumed session lists it. Kill it again and start
   `claude --continue`: same result (the debug log shows `SessionStart:resume
   [clerk hook session-start]`). With a short lease, the resumed session is
   also woken by the old `lease_wait` task's expiry warning.
5. **The Stop hook reminder appears once.** In `claude -p`, have the agent
   lease something and end its turn without `lease_wait` (and not call it when
   told). Expect exactly one Stop block whose reason starts "Le Clerk: this
   session has 1 live lease but no lease_wait call is running", and a second
   Stop (with `stop_hook_active`) that lets the turn end.
6. **Registration follows the skill.** Make fake device files (e.g.
   `/tmp/hub/serial/by-id/usb-Espressif_…-if00`, two of a kind plus one
   other) and ask: "Register the boards on my hub", saying where they are.
   Expect the agent to load `registering-resources`, call `resource_list`
   first, group identical boards into one resource, write a description,
   usage with `{unit.path}` placeholders, and tags, leave `scope` out (project
   scope), and add one unit per device with stable paths.
7. **Plugin load and start-up.** With `--debug-file`, check the log for
   `Loading hooks from plugin: leclerk`, no hook schema error, and
   `SessionEnd:… [clerk hook session-end] completed` (the fractional
   `timeout: 1.5` is accepted). For start-up latency, add a slow SessionStart
   hook (`--settings '{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"sleep 4"}]}]}}'`)
   and check that `plugin:leclerk:clerk` connects before that hook finishes.

### Results, 2026-11-10 (Claude Code 2.1.321, `--model haiku`)

All seven passed; details and evidence paths are in the spec, Appendix B,
"Task 7 live checks". Left for you: installing into your real Claude Code
setup (the install commands were run only against a temp
`CLAUDE_CONFIG_DIR`), real hardware, and the desktop app or VS Code, where
`lease_wait` blocks instead of running as a task.
