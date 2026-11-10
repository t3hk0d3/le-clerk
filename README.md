# Le Clerk

*"Un moment, s'il vous plaît… your esp32 is being prepared."*

Le Clerk (`clerk`) is a local broker that hands out shared resources to AI
coding agents one holder at a time: dev boards and serial ports, network
ports, services, API tokens, licence seats. Agents lease what they need
instead of messaging each other "is board X free?", and identical units are
all used before anyone queues.

It is built for [Claude Code](https://code.claude.com): agents use the
`clerk` command from their Bash tool, guided by one skill and a few hooks
that ship as a Claude Code plugin. People and scripts use the same CLI.

> Status: early and local. One machine, one OS user, Linux first (macOS
> should work). Built and used daily, but expect rough edges.

## What it does

- **Inventory:** a *resource* (`c6-boards`) has interchangeable *units*
  (`c6-a`, `c6-b`). Three types: `device` (a path such as
  `/dev/serial/by-id/…`), `service` (host and port, connect or listen) and
  `generic` (anything named by a key). Resources carry a description, tags
  and Markdown usage instructions with `{unit.*}` placeholders, handed to
  the holder on grant. Duplicates are detected per type, so the same board
  registered twice is never leased twice.
- **Scopes:** user scope (shared by every project, the place for hardware)
  and project scope (invisible to other projects).
- **Leasing:** ask for any free unit or a specific one, with an honest
  duration and a purpose others can read. Free healthy units are granted at
  once; otherwise you queue, with your position and an estimated turn.
  Leases warn before they expire, can be extended, and go through a cleanup
  window on hand-back; unconfirmed cleanup is reported to the next holder.
- **Cooperation:** interruptible leases let urgent short work borrow a unit
  and give it back; a user-directed `--override` takes a unit anyway; health
  checks run before every grant (e.g. "the serial port is not open").
- **Agents stay reachable:** notices (your turn, expiry soon, please yield,
  your unit is back) reach an idle agent through Claude Code's Monitor tool
  on `clerk wait --follow`. A dead Claude Code frees its leases after a
  short grace; `/clear` releases them at once; `--continue` keeps them.
- **Audit log** of every inventory change, grant, hand-over and override.

## Install

Needs Rust (stable) and, for agents, Claude Code 2.1.214 or later.

```sh
cargo build --release
install -m 755 target/release/clerk ~/.local/bin/clerk   # anywhere on PATH
clerk --version
```

The first `clerk` command starts the daemon. After upgrading `clerk`, run
`clerk daemon restart`; leases and queues survive it.

### Claude Code plugin

The repository is also a Claude Code plugin marketplace:

```sh
claude plugin marketplace add /path/to/le-clerk
claude plugin install leclerk@leclerk --scope user
```

Restart Claude Code. The plugin adds the `clerk` skill and four hooks. Optional:
add `"Bash(clerk:*)"` and `"Monitor"` to `permissions.allow` to skip
permission prompts. Details, what the hooks do and a manual checklist are in
[docs/claude-code.md](docs/claude-code.md).

## Quick start

Register two boards (user scope, since hardware is shared by every project):

```sh
clerk resource create c6-boards --type device --scope user \
  --description "ESP32-C6 dev boards on the USB hub" --tag esp32c6 \
  --usage 'Flash with `idf.py -p {unit.path} flash`. Clean = erase flash.'
clerk unit add c6-boards c6-a --path /dev/serial/by-id/usb-Espressif_…-if00
clerk unit add c6-boards c6-b --path /dev/serial/by-id/usb-Espressif_…-if00
```

Use one from a script, held exactly as long as the command runs:

```sh
clerk run c6-boards --for 15m -- sh -c 'idf.py -p "$CLERK_UNIT_PATH" flash'
```

Or hold one from a terminal (Ctrl+C releases, Ctrl+] detaches):

```sh
clerk acquire c6-boards --for 1h --purpose "manual testing"
```

See what is going on:

```sh
clerk status        # units, holders, queues, health, dirty flags
clerk leases        # live leases
clerk log           # the audit log
```

Agents in Claude Code need nothing more: ask one to "flash the firmware on
a C6 board" and the `clerk` skill has it find, lease, wait for and release
a board. Ask "register the boards on my hub" and it registers them.

## Output

On a terminal, people get block lists and colour ([docs/cli-output.md](docs/cli-output.md)).
Scripts get JSON (`--json`, or any non-terminal stdout). Agent sessions get
labelled Markdown that ends with a `Next:` line; `CLERK_AGENT_FORMAT=json`
switches agents back to JSON.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Conventions are in [STYLEGUIDE.md](STYLEGUIDE.md). `CLERK_HOME` points every
clerk path (socket, database, config) at one directory, which keeps tests
and experiments away from your real state.

## License

[AGPL-3.0](LICENSE).
