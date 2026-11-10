# CLI reference

`clerk --help` and `clerk <command> --help` have every flag; this page is
the map. Concepts are explained in [concepts.md](concepts.md).

## Inventory

```sh
clerk types                                   # types, settings, checks, placeholders
clerk resource create <id> --type device|service|generic [--scope user|project] \
  [--description …] [--tag …]… [--usage … | --usage-file …] \
  [--health <check>]… [--health-command <script>]
clerk resource update <id> [--description …] [--add-tag …] [--remove-tag …] [--usage …]
clerk resource show <id>
clerk resource delete <id>
clerk resources [--tag …]…                    # list, all tags must match

clerk unit add <resource> <unit> --path … | --host … --port … [--listen] | --key … \
  [--description …]
clerk unit update <resource> <unit> …
clerk unit retire <resource> <unit>           # removed once its lease ends
clerk unit remove <resource> <unit>           # a free unit, now
```

`resource create|update` and `unit add|update` also read their fields from a
YAML file with `-f <path>`, or `-f -` for stdin (stdin is never read
implicitly). On update only the keys present change. A field given both as
a flag and in the YAML is refused, as is an unknown key.

```yaml
# clerk resource create c6-boards --type device -f c6.yaml
description: ESP32-C6 dev boards on the USB hub
tags: [esp32, esp32c6, devboard]
usage: |
  Flash with `idf.py -p {unit.path} flash`.
  Clean = erase flash.
```

Without `--scope`, people create in user scope and agents in their project
(hardware should still go to user scope).

## Leasing

```sh
clerk acquire <resource>[/<unit>] --for <d> [--purpose …] [--interruptible] \
  [--priority low|normal|high] [--skip-checks] [--wait | --no-wait]
clerk run <resource>[/<unit>] --for <d> [lease options] -- <command>…
clerk attach <lease>                          # live view again (claims an offer)
clerk claim <lease>                           # claim an offer
clerk cancel <request>
clerk extend <lease> --for <d>                # new end: now + d
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
  exits 6; with `--wait` it waits for the grant.
- **`run`** waits in the queue, then runs the command with the unit in its
  environment: `CLERK_LEASE_ID`, `CLERK_RESOURCE`, `CLERK_UNIT` and
  `CLERK_UNIT_<SETTING>` (`CLERK_UNIT_PATH`, `CLERK_UNIT_HOST`,
  `CLERK_UNIT_PORT`, …). Exit 0 confirms cleanup; anything else marks the
  unit dirty.
- **`wait`** prints your notices as they arrive. Agents run it as a
  one-batch background command, which ends with the first notice that needs
  action; `--follow` keeps printing, for tools that stream a command's
  output.

## Looking around

```sh
clerk status                  # every unit: holder, queue, health, dirty flag
clerk leases [--mine]         # live leases (--mine: this agent session's)
clerk queue <resource>
clerk log [--action …] [--limit n] [--wide]
clerk promotions
clerk approve <promotion> [--as <id>]
```

## Admin

Hard tools for when something goes wrong. They skip the normal rules, are
all logged, ask for confirmation on a terminal (or need `--yes`), and an
agent also needs `--override "<reason>"`.

| Command | Does |
|---------|------|
| `admin release <lease>` | end a lease now, unit dirty (also `release --force`) |
| `admin evict <holder>` | end every lease and request of a holder |
| `admin dequeue <request>`, `admin clear-queue <resource>` | queue surgery |
| `admin remove-unit`, `admin delete-resource` | even if held or queued for |
| `admin clean <resource>/<unit>` | clear a dirty flag |
| `admin link`, `admin unlink` | fix duplicate detection |
| `admin move <resource> --to user\|project:<path>` | change scope |
| `admin rename` | a resource, or a unit with `--unit` |
| `admin edit-lease <lease>` | end time, priority, interruptibility |
| `admin doctor [--fix]` | find (and repair) damage |
| `admin export [--out f]`, `admin import --in f` | back up and restore the inventory |

## The daemon

The first command starts it; it keeps running in the background.

```sh
clerk daemon status
clerk daemon restart          # after upgrading clerk; leases survive
clerk daemon stop
```

A client refuses a daemon of another version (exit 5) and says to restart it.

## Output

- On a terminal: block lists with colour (`NO_COLOR` turns colour off,
  `--ascii` limits to ASCII).
- Otherwise, or with `--json`: JSON, one document per command.
- In a Claude Code agent session: labelled Markdown ending with a `Next:`
  line. `CLERK_AGENT_FORMAT=json` (or `--json`) gives JSON instead.

[cli-output.md](cli-output.md) shows each view.

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | done |
| 1 | internal failure |
| 2 | usage error (bad flag, bad value) |
| 3 | not found |
| 4 | refused or conflict |
| 5 | daemon unavailable, unresponsive or another version |
| 6 | queued (non-interactive `acquire`) |
| 130 | interactive `acquire` or `attach` ended by Ctrl+C |

## Files and configuration

| What | Default | With `CLERK_HOME=<dir>` |
|------|---------|-------------------------|
| socket, lock | `$XDG_RUNTIME_DIR/clerk/` | `<dir>/run/` |
| database (`clerk.db`), daemon log | `~/.local/state/clerk/` | `<dir>/state/` |
| config (`config.toml`) | `~/.config/clerk/` | `<dir>/config/` |

`config.toml` sets timings, in seconds unless noted:

```toml
warning_lead_max = 300      # expiry warning, at most this early
warning_lead_ratio = 0.2    # …and at this fraction of the duration
cleanup_window = 120
cleanup_extension = 120
claim_window = 300
yield_window = 120
health_timeout = 10
reconnect_grace = 60        # an agent's leases after its Claude Code exits
```

The socket path must stay under 107 bytes; pick a short `CLERK_HOME`.
