# Troubleshooting

## Where to look

- **The daemon log:** `daemon.log` in the state directory
  (`~/.local/state/clerk/daemon.log`, or `$XDG_STATE_HOME/clerk/`, or
  `<CLERK_HOME>/state/`). The daemon appends to it, and it is the first place
  to look when a command says the daemon is unavailable.
- `clerk daemon status` says whether it runs, with pid and version.
- `clerk admin doctor` looks for damage in the store; `--fix` repairs what it
  can.
- `clerk log` is the audit log: what was granted, handed over, overridden.
- Paths and environment variables are in [cli.md](cli.md#files-and-configuration).

## Error codes and exit codes

Errors print as `## Error: <code>` in an agent session, as a sentence on a
terminal, and as `{"error": {"code": …}}` in JSON. The exit code follows the
code's class.

| Exit | Codes | What to do |
|------|-------|------------|
| 2 usage | `usage_error`, `invalid_request`, `invalid_argument`, `invalid_id`, `invalid_settings`, `invalid_tag`, `too_many_tags`, `conflicting_tag_edit`, `description_multiline`, `description_too_long`, `usage_too_long`, `unknown_placeholder`, `health_empty`, `health_blank_command`, `health_check_not_allowed`, `settings_type_mismatch`, `settings_empty_field`, `settings_zero_port`, `extend_not_later`, `no_such_directory`, `agent_unidentified` | Fix the command as the message says; limits are in [concepts.md](concepts.md#limits). |
| 3 not found | `not_found`, `unknown_method` | Check the id with `clerk resources`, `clerk leases` or `clerk queue`. |
| 4 refused | `busy`, `already_exists`, `already_held`, `no_units`, `retiring`, `bad_state`, `extend_refused`, `extension_used`, `not_holder`, `not_yours`, `session_not_hosted`, `device_absent`, `scope_not_allowed`, `override_required`, `confirmation_required`, `aborted`, `duplicate_in_scope`, `ambiguous_duplicate`, `link_conflict`, `borrower_not_overrun`, `id_clash`, `import_clash`, `not_directly_linked` | The message says why; see below for the common ones. |
| 5 daemon | `daemon_unavailable`, `daemon_unresponsive`, `daemon_already_running`, `daemon_restarting`, `daemon_stopping`, `protocol_mismatch`, `engine_unavailable`, `store_too_new`, `socket_path_too_long` | See below. |
| 1 internal | `internal_error`, `database_error`, `corrupt_database`, `migration_error`, `io_error`, `json_error`, `bad_response` | Check the daemon log; `clerk admin doctor`. |

`clerk acquire` exits 6 (not an error) when the request is queued. `clerk run`
exits with its command's status once the command has started (see [exit
codes](cli.md#exit-codes)).

## Common failures

**`session_not_hosted` (4).** An agent session may hold or queue for a unit
only while the clerk knows the Claude Code process that runs it, and the
hooks tell it. The hooks may not be reaching the clerk: the plugin is not
installed or enabled in that session (`claude plugin list`), `clerk` is not on
the `PATH` of whatever started Claude Code, or the daemon was not running when
the session started. Restart the session, or run `/clear`. After `/clear` the
Bash tool must report the new session's id; if it keeps the old one, acquires
are refused with this code (checklist item 3b in [testing.md](testing.md)).

**`agent_unidentified` (2).** `CLAUDE_PID` is not set, or names no running
process. Claude Code 2.1.214 or later sets it for Bash commands; update Claude
Code. The clerk needs Linux: it reads `/proc`.

**`not_yours` (4).** The lease or request belongs to another holder, and the
command is not an admin tool. `clerk leases --mine` shows yours. To end
someone else's lease use `clerk release <lease> --force` (agents need
`--override "<reason>"`).

**`device_absent` (4).** `acquire --skip-checks` vouches for a device, but
nothing exists at the unit's path: it is unplugged or the path is wrong.
Fix the unit with `clerk unit update … --path`.

**`override_required` (4).** An agent called something that needs the user's
word. The list is in [cli.md](cli.md#the-daemon). Ask the user, then repeat
with `--override "<their reason>"`.

**`daemon_unavailable` (5).** No daemon answers and none could be started.
Read `daemon.log`. Typical causes: the socket path is too long
(`socket_path_too_long`, over 107 bytes: set a shorter `CLERK_HOME`), or the
state directory is not writable. When the daemon was started on demand by
your command and then refused to start (for example `store_too_new` below, or
an unreadable `config.toml`), the client sees only `daemon_unavailable`; the
reason is in the daemon log. Run `clerk daemon` in a terminal to see it start
in the foreground.

**`daemon_unresponsive` (5).** A process holds the daemon's lock but does not
answer. It may still be starting. If it stays like that, end that process
(the message gives its pid) and run the command again.

**`daemon_already_running` (5).** You ran `clerk daemon` (the foreground
daemon) while one already runs for the same store. There is one daemon per
store (`clerk.db.lock`). Use `clerk daemon status`, or `clerk daemon restart`.

**`protocol_mismatch` (5).** The running daemon speaks another protocol than
this `clerk`, usually after an upgrade. Run `clerk daemon restart`. Waits that
were running end with this code; agents arm them again.

**`store_too_new` (5).** A newer `clerk` wrote the database, and this older
one will not touch it. Run the newer build (check `clerk --version` against the
one on your `PATH`), or restore an older database. If the daemon was started
on demand, your command shows `daemon_unavailable` and the reason is in the
daemon log; `clerk daemon` in a terminal prints it.

**`daemon_stopping` (5) and a wait that ended.** The daemon was stopped on
purpose (`clerk daemon stop`). Waiting commands do not restart it; any other
`clerk` command does. A crash leaves no stop marker, so waiters start a
new daemon themselves.

**A lease that outlives its end.** Soft expiry: an agent's lease with nobody
queued goes overdue and stays held until someone queues for the unit; then the
holder gets the cleanup window. See [concepts.md](concepts.md#the-lease-lifecycle).

**No notices reach the agent.** A wait only runs while the agent is queued or
holds an interruptible lease. Check `clerk leases --mine`; the SessionStart
brief and the Stop hook say whether one is armed.

**A hook seems to do nothing.** Hooks fail open and print nothing on
errors. Run the daemon (`clerk daemon status`), check that `clerk` is on the
`PATH` or set `CLERK_BIN`, and see [claude-code.md](claude-code.md).

**The daemon runs the old build.** After upgrading run `clerk daemon
restart`; update the plugin with `claude plugin update leclerk@leclerk` and
restart Claude Code.

**A unit is marked dirty or abandoned.** Cleanup was not confirmed. Clean it
up, then `clerk admin clean <resource>/<unit>`.

**Last resorts.** `clerk daemon stop` then any command starts a fresh daemon;
`clerk admin doctor --fix` repairs a damaged store; `clerk admin export --out
<file>` backs up the inventory first.
