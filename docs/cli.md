# CLI reference

`clerk --help` and `clerk <command> --help` have every flag; this page is
the map. Concepts are explained in [concepts.md](concepts.md).

## Inventory

```sh
clerk types                                   # types, settings, checks, placeholders
clerk resource create <id> --type device|service|generic [--scope user|project|project:<path>] \
  [--description …] [--tag …]… [--usage … | --usage-file …] \
  [--health <check>]… [--health-command <script>]
clerk resource update <id> [--scope …] [--description …] [--tag …]… | [--add-tag …] [--remove-tag …] \
  [--usage … | --usage-file …] [--health <check>]… [--health-command <script>] [-f <path>]
clerk resource show <id> [--scope …]
clerk resource delete <id> [--scope …]
clerk resources [--tag …]…                    # list, all tags must match (also `resource list`)

clerk unit add <resource> <unit> [--scope …] [--description …] [-f <path>] \
  --path … | --host … --port … [--listen] [--scheme …] [--service-id …] | --key …
clerk unit update <resource> <unit> …         # the same flags; unset settings are kept
clerk unit retire <resource> <unit> [--scope …]   # removed once its lease ends
clerk unit remove <resource> <unit> [--scope …]   # a free unit, now
```

`resource create|update` and `unit add|update` also read their fields from a
YAML file with `-f <path>`, or `-f -` for stdin (stdin is never read
implicitly). On update only the keys present change. A field given both as
a flag and in the YAML is refused, as is an unknown key. `resource update
--tag` replaces every tag; `--add-tag` and `--remove-tag` edit the set (not
together with `--tag`).

```yaml
# clerk resource create c6-boards --type device -f c6.yaml
description: ESP32-C6 dev boards on the USB hub
tags: [esp32, esp32c6, devboard]
usage: |
  Flash with `idf.py -p {unit.path} flash`.
  Clean = erase flash.
```

Without `--scope`, `resource create` uses user scope, or in an agent
session the session's project when there is one (hardware should still go to
user scope: pass `--scope user`). Every `--scope`, on `resource`, `unit` and
the `admin` commands that take one, accepts `user`, `project` (the project of
the working directory) or `project:<path>`. Without it, `update`, `show`,
`delete` and the unit commands look in your project, then in user scope; to
reach a user resource that a project resource hides, say `--scope user`.
An agent may not name a scope on `resource show|update|delete` or
`unit add|update|remove|retire` (refused as `scope_not_allowed`, exit 4); it
works on what its project sees.

Limits: ids match `^[a-z0-9][a-z0-9._-]{0,62}$`; a description is one line of
at most 200 characters; a resource has at most 16 tags, each 1 to 32 lowercase
letters, digits or `-`, starting with a letter or digit; usage is at most 4096
bytes. Violations are usage errors (exit 2).

## Leasing

```sh
clerk acquire <resource>[/<unit>] --for <d> [--purpose …] [--interruptible] \
  [--priority low|normal|high] [--skip-checks] [--wait | --no-wait]
clerk run <resource>[/<unit>] --for <d> [lease options] [--wait] -- <command>…
clerk attach <lease>                          # live view again (claims an offer)
clerk claim <lease>                           # claim an offer
clerk cancel <request>
clerk extend <lease> --for <d>                # new end: now + d
clerk extend <lease> --cleanup                # the one cleanup extension
clerk release <lease> [--cleanup]
clerk confirm <lease> clean|yielded
clerk interruptible <lease> on|off
clerk yield <lease> --refuse
clerk reclaim <lease>
clerk wait [--follow] [--once] [--timeout <d>]
```

Durations are `<n>s|m|h|d`, up to 30 days.

- **`acquire`** on a terminal shows a live view: the queue position, then
  the grant and notices. Ctrl+C releases (or cancels while queued), Ctrl+]
  detaches. Without a terminal it prints the grant, or the queued request and
  exits 6; with `--wait` it waits for the grant. **`--wait` is refused in an
  agent session** (usage error): an agent acquires, and arms `clerk wait`
  while it is queued.
- **`--interruptible`** (on `acquire` and `run`) is for agent sessions only;
  anyone else gets a usage error. Leases from the command line are never
  interrupted.
- **`run`** waits in the queue (people), or in an agent session exits 6 when
  queued unless `--wait` is given; then runs the command with the unit in its
  environment: `CLERK_LEASE_ID`, `CLERK_RESOURCE`, `CLERK_UNIT` and
  `CLERK_UNIT_<SETTING>` (`CLERK_UNIT_PATH`, `CLERK_UNIT_HOST`,
  `CLERK_UNIT_PORT`, …). Exit 0 confirms cleanup; anything else marks the
  unit dirty. When the lease ends before the command does, `run` sends it
  SIGTERM; if it is still running after the cleanup window (at most 30 s) it
  sends SIGKILL.
- **`extend --cleanup`** asks for the lease's one cleanup extension, while it
  is in its cleanup window (the notice says when); it adds
  `cleanup_extension` seconds once and takes no `--for`.
- **`wait`** prints your notices as they arrive. Agents run it as a
  one-batch background command, which ends with the first notice that needs
  action; `--follow` keeps printing, for tools that stream a command's
  output (`--timeout <d>` gives up after that long with an empty batch,
  `--once` returns after the first batch). Only one wait runs per session. If
  the daemon is stopped on purpose, a batch `wait` ends with exit 0 and says
  so, while `wait --follow` exits 5.
- **Priority** (`low`, `normal`, `high`) orders the queue for a unit: higher
  first, then first come first served. A request may also ask an
  interruptible holder to yield when its priority is higher, or equal with a
  shorter duration than the holder has left. A holder's `extend` is refused
  while a request of strictly higher priority waits for its unit (unless the
  lease is interruptible).

## Looking around

```sh
clerk status                  # every unit: holder, queue, health, dirty flag
clerk leases [--mine]         # live leases (--mine: this agent session's)
clerk queue <resource>
clerk log [--since <when>] [--actor <holder>] [--action …] [--limit n] [--wide]
clerk promotions
clerk approve <promotion> [--as <id>]
```

`log --since` takes `<n>s|m|h|d` ago (`2h`) or an RFC 3339 time; `--actor` takes
`session:<id>`, `pid:<pid>` or `user:<tag>`. Agents need `--override` for `log`,
`promotions` and `approve`.

## Admin

Hard tools for when something goes wrong. They skip the normal rules and are
all logged. The state-changing ones ask for confirmation on a terminal (or
need `--yes`); `admin export` and `admin doctor` without `--fix` just run. An
agent needs `--override "<reason>"` for every `admin` command except those
two.

| Command | Does |
|---------|------|
| `admin release <lease>` | end a lease now, unit dirty (also `release --force`) |
| `admin evict <holder>` | end every lease and request of a holder |
| `admin dequeue <request>`, `admin clear-queue <resource>` | queue surgery |
| `admin remove-unit`, `admin delete-resource` | even if held or queued for (`--scope`) |
| `admin clean <resource>/<unit>` | clear a dirty flag (`--scope`) |
| `admin link <a> <b>`, `admin unlink <a> <b>` | fix duplicate detection (`--scope-a`, `--scope-b`) |
| `admin move <resource> --to user\|project:<path>` | change scope (`--from` when the id exists in several) |
| `admin rename <resource> <new-id>` | a resource, or a unit with `--unit` (`--scope`) |
| `admin edit-lease <lease>` | `--for`, `--priority`, `--interruptible true\|false` |
| `admin doctor [--fix]` | find (and repair) damage |
| `admin export [--out f [--force]]`, `admin import --in f [--replace]` | back up and restore the inventory (`--out` makes a new `0600` file; `--force` overwrites, `--replace` overwrites existing resources) |

## The daemon

The first command starts it; it keeps running in the background. `clerk
daemon` with no subcommand runs it in the foreground instead (for systemd or
debugging); it logs to `daemon.log` in the state directory.

```sh
clerk daemon status
clerk daemon restart          # after upgrading clerk; leases survive
clerk daemon stop
clerk hook <event>            # the Claude Code hooks call this; see claude-code.md
```

- **One daemon per store.** The daemon holds a lock next to the database
  (`clerk.db.lock`) for its whole life, so a second `clerk daemon` refuses
  with `daemon_already_running` (exit 5). Clients find the socket through
  `daemon.addr` in the state directory, whatever run directory the daemon
  uses.
- **A stop is deliberate; a crash is not.** `daemon stop` leaves a `stopped`
  marker in the state directory. A command that only waits (`clerk wait`,
  the live view) then does not restart the daemon, and ends saying it was
  stopped; any one-shot command starts a new daemon and clears the marker. A
  crash leaves no marker, so waiters start a daemon again.
- **Versions and protocol.** A client works with a daemon of another version
  if both speak the same protocol (currently 5). On a protocol change the
  client refuses the daemon (`protocol_mismatch`, exit 5) and says to run
  `clerk daemon restart`; waits that are running end with `protocol_mismatch`
  too. After an upgrade, run `clerk daemon restart`.
- **Agent policy.** In an agent session `--override "<reason>"` is required
  for `daemon stop` (a `restart` is open), the `admin` tools except `export`
  and a `doctor` without `--fix`, handing a lease to another holder, and the
  commands listed under `--override` in `clerk --help` (`approve`,
  `promotions`, `log`, `release --force`, taking a unit another holder has).
  The daemon enforces this for its own calls, whichever client sends them.
  The reason is the client's own word: the policy guards agents against
  mistakes, not against a hostile process running as you (see
  [the trust model](claude-code.md#trust-model)).
- **Environment.** An automatically started daemon, and the custom health
  checks it runs, get only `HOME`, `PATH`, `XDG_*`, `CLERK_HOME`, `LANG` and
  `RUST_LOG` from their parent's environment; checks also receive the
  unit's `CLERK_*` variables.
- **Health checks.** `exists`, `accessible`, `not-open` and `port-free` run
  in place; `tcp-connect`, `http` and custom checks run off the engine
  thread, each bounded by `health_timeout`. A cold acquire (no recent check
  result) waits up to `health_timeout` plus 2 s for the check before it
  queues.

[troubleshooting.md](troubleshooting.md) has the error codes and what to do.

## Output

- On a terminal: block lists with colour (`NO_COLOR` turns colour off,
  `--ascii` limits to ASCII).
- Without a terminal: JSON for scripts, one document per command; `--json`
  gives JSON everywhere.
- In a Claude Code agent session (`CLAUDE_CODE_SESSION_ID` set): labelled
  Markdown ending with a `Next:` line, so a script and an agent get different
  output from the same command. `CLERK_AGENT_FORMAT=json` (or `--json`) gives
  JSON instead; `CLERK_AGENT_FORMAT=human` gives the terminal view.

[cli-output.md](cli-output.md) shows each view.

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | done |
| 1 | internal failure |
| 2 | usage error (bad flag, bad value) |
| 3 | not found |
| 4 | refused or conflict (`busy`, `not_yours`, `session_not_hosted`, `device_absent`, `override_required`, …) |
| 5 | daemon unavailable, unresponsive, speaking another protocol, already running, or its store newer than this build (`store_too_new`) |
| 6 | queued (non-interactive `acquire`, or an agent's `run` without `--wait`) |
| 130 | interactive `acquire` or `attach` ended by Ctrl+C |

**`clerk run`** exits with the command's own status once the command has
started: its code, 127 when it cannot be started, 128+n when signal n killed
it. That can look like a clerk code (a command that exits 4). In an agent
session `run` therefore ends its output with `- child_exit: N` (or
`{"child_exit": N}` in JSON); an error before the command started (codes 1 to
6) prints an `## Error:` block and has no `child_exit` line.

[troubleshooting.md](troubleshooting.md) maps every error code to its exit code.

## Files and configuration

| What | Default | With `CLERK_HOME=<dir>` |
|------|---------|-------------------------|
| socket (`clerk.sock`), `daemon.lock` | `$XDG_RUNTIME_DIR/clerk/` (else `<state dir>/clerk/`) | `<dir>/run/` |
| database (`clerk.db`), `clerk.db.lock`, `daemon.addr`, `stopped`, daemon log (`daemon.log`) | `$XDG_STATE_HOME/clerk/` (default `~/.local/state/clerk/`) | `<dir>/state/` |
| config (`config.toml`) | `$XDG_CONFIG_HOME/clerk/` (default `~/.config/clerk/`) | `<dir>/config/` |

`config.toml` is read when the daemon starts: change it, then run `clerk
daemon restart`. Unknown keys, and values that are not positive (the ratio
must be between 0 and 1), make the daemon refuse to start. It sets timings,
in seconds unless noted, and how long finished records are kept, in days:

```toml
warning_lead_max = 300      # expiry warning, at most this early
warning_lead_ratio = 0.2    # …and at this fraction of the duration
cleanup_window = 120        # how long a holder has to clean up and confirm
cleanup_extension = 120     # added once by `clerk extend <lease> --cleanup`
claim_window = 300          # how long an offer waits to be claimed
yield_window = 120          # how long an interruptible holder has to answer
health_timeout = 10         # one health check's limit
reconnect_grace = 60        # an agent's leases after its Claude Code exits
retention_days = 90         # days (not seconds): ended leases and the audit log
notice_retention_days = 7   # days: delivered notices, and notices nobody will take
```

The daemon sweeps once a day, and at start, for what is past retention.
Live leases, and notices a live holder has not been given, are never swept.

Environment variables the clerk reads: `CLERK_HOME`, `XDG_STATE_HOME`,
`XDG_CONFIG_HOME`, `XDG_RUNTIME_DIR`, `HOME`; `CLAUDE_CODE_SESSION_ID` and
`CLAUDE_PID` (set by Claude Code: they make a caller an agent session);
`CLERK_AGENT_FORMAT` (`json` or `human`); `NO_COLOR`. The socket path must stay under 107 bytes; pick a short
`CLERK_HOME`.
