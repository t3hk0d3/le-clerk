# Concepts

## Resources and units

A **resource** is a kind of thing agents lease, such as `c6-boards`. Its
**units** are the interchangeable instances: `c6-a`, `c6-b`. A request names
a resource (any free unit) or one unit (`c6-boards/c6-a`).

Every resource has a **type**, which decides what a unit's settings are and
which health checks apply. `clerk types` lists them.

| Type | A unit is | Settings |
|------|-----------|----------|
| `device` | something reached by a filesystem path | `path` (prefer `/dev/serial/by-id/…`) |
| `service` | a host and port | `host`, `port`, `mode` (`connect` to a running server, or `listen` for a port the holder binds), optional `scheme`, `service_id` |
| `generic` | anything else | `identity_key` (a name for it; never a secret) |

Group interchangeable things into one resource with one unit each: two
identical boards are one resource with two units, so a request for "a
board" can be served by either.

### Descriptions, tags and usage

- **Descriptions:** one line for the resource, one per unit ("left port,
  blue sticker").
- **Tags:** what it is and what it can do (`esp32`, `esp32c6`, `thread`).
  `clerk resources --tag esp32c6` finds resources carrying every given tag.
  Tags help find a resource; they never lease "anything tagged X".
- **Usage:** Markdown instructions handed to the holder on every grant: how
  to connect, gotchas, and what "clean" means when handing it back.
  Placeholders are filled in for the granted unit:
  - every type: `{resource.id}`, `{resource.description}`, `{unit.id}`,
    `{unit.description}`;
  - `device`: `{unit.path}`;
  - `service`: `{unit.host}`, `{unit.port}`, `{unit.address}` (`host:port`),
    `{unit.mode}`, `{unit.scheme}`, `{unit.service_id}`;
  - `generic`: `{unit.identity_key}`.

  Other braces (`${PORT}`, JSON) pass through unchanged; an unknown
  `{resource.*}` or `{unit.*}` key is refused when you save the usage.

## Limits

- Resource and unit ids: `^[a-z0-9][a-z0-9._-]{0,62}$`.
- Descriptions: one line, at most 200 characters.
- Tags: at most 16 per resource, each 1 to 32 characters (lowercase letters,
  digits and `-`, starting with a letter or digit).
- Usage: at most 4096 bytes.
- Durations (`--for`): `<n>s|m|h|d`, whole n of at least 1, up to 30 days.

## Scopes

- **User scope:** visible from every project of your user. This is where
  hardware belongs: a physical board is shared by every project on the
  machine.
- **Project scope:** visible only inside one project (a git repository, by
  its common git directory, so worktrees share it). Good for ports or
  tokens that only one project uses.

Inside a project, a project resource hides a user resource with the same
id. If a project registers something that looks like a unit in another
project, the clerk files a **promotion** request; `clerk promotions` lists
them and `clerk approve <id>` moves the resource to user scope.

## Duplicates

Each type knows how to tell when two units are the same thing: for devices
the canonical path, device number and USB identity (so `/dev/ttyACM0` and its
`/dev/serial/by-id/…` link match); for services the host and port; for
generic units the key. Adding a duplicate in the same scope is refused and names the unit
that exists. A duplicate across scopes is added but **linked**: a lease on
either unit blocks the other, so one physical thing is never held twice.
`clerk admin link` and `unlink` fix what the detection got wrong.

## Health checks

Checks run on a unit just before it is handed out, never while it is held.
A unit that fails is skipped (another one is granted, or the request waits
and the unit is checked again later).

| Type | Built-in checks |
|------|-----------------|
| `device` | `exists`, `accessible`, `not-open` (no process has it open) |
| `service` | `tcp-connect` (something listens), `port-free` (nothing does), `http` (a plain `GET /` to the unit's host and port answers 2xx; `https` is not supported) |
| `generic` | none |

Every type also takes `custom`: your own command, given the unit's settings
as `CLERK_UNIT_*` environment variables, healthy when it exits 0 within the
timeout. `none` turns checks off. Devices default to `exists` and
`not-open`; a service in `connect` mode defaults to `tcp-connect`, one in
`listen` mode to `port-free`; a generic resource has none.

The local checks (`exists`, `accessible`, `not-open`, `port-free`) run in
place. `tcp-connect`, `http` and custom checks run off the daemon's engine
thread, each limited by `health_timeout` (10 s by default), so a first
acquire of a unit with no recent check result can take up to that plus 2 s.
Custom checks run with only `HOME`, `PATH`, `XDG_*`, `CLERK_HOME`, `LANG`,
`RUST_LOG` and the unit's `CLERK_*` variables in their environment.

If a unit is busy because **your own** running job uses it (a bench that
already has the port open), lease that unit with `--skip-checks` instead of
stopping the job; it never takes a unit from a lease holder, and it is
logged.

## The lease lifecycle

1. **Request.** `clerk acquire <resource>[/<unit>] --for <duration>
   --purpose <text>`. A free, healthy unit is granted at once. Otherwise the
   request **queues**, with its position, an estimated turn, and what blocks
   it (who holds the unit and why, or which health check failed).
2. **Offer.** When a unit frees up, the first suitable waiter gets an
   **offer** and has a claim window (5 minutes) to claim it; an agent's wait
   claims it automatically. Unclaimed offers go to the next in line.
3. **Held.** The lease runs for the declared duration. An **expiry warning**
   arrives before the end (20 % of the duration, at most 5 minutes);
   `clerk extend` moves the end. At the end, what happens depends on the
   holder and the queue:
   - someone is queued for the unit, or the holder is a person or script:
     the lease expires on time, and a cleanup window opens;
   - an agent's lease with nobody queued goes **overdue** and stays held.
     When someone later queues for the unit, the holder is told and gets the
     cleanup window; if it doesn't confirm, the unit is handed over marked
     **abandoned**, and the new owner is told.
4. **Hand-back.** `clerk release` gives the unit back with its cleanup
   confirmed. `release --cleanup` opens a **cleanup window** (2 minutes)
   ended by `clerk confirm <lease> clean`; `clerk extend <lease> --cleanup`
   adds one more window (`cleanup_extension`, 2 minutes), once. A hand-over without
   confirmed cleanup marks the unit **dirty**, and the next holder is told.

### Priority

A request has a priority, `low`, `normal` (the default) or `high`. A unit's
queue is ordered by priority, then by arrival. A higher priority also lets a
request ask an interruptible holder to yield (see below); a request of equal
priority can do so only when it is shorter than the holder's remaining time.
A holder's `clerk extend` is refused while a request of strictly higher
priority waits for the unit, unless the lease is interruptible. Leave it at
`normal` unless the user says otherwise.

### Interruptible leases

Long work that can pause safely (soak tests, benchmarks) can be leased
`--interruptible`. Only agent sessions can: a lease from the command line
(`clerk acquire`, `clerk run`) is never interrupted, and asking for it there
is a usage error. When a more urgent request waits, the holder gets a
**yield request**: it pauses at a safe point and runs `clerk confirm <lease>
yielded`, or refuses with `clerk yield <lease> --refuse`. The borrower uses
the unit, gives it back, and the paused holder's clock resumes; if the
borrower overruns, the holder can `clerk reclaim` it.

### Override

`--override "<reason>"` takes a unit even from a holder that cannot be
interrupted. Agents use it only when the user tells them to; the reason goes
into the audit log. The daemon refuses an agent's call to the `admin` tools,
`daemon stop` and a few other commands without it (`override_required`; the
list is in [cli.md](cli.md#the-daemon)). The reason is the caller's own
word, so this stops mistakes, not a hostile process.

## Holders and liveness

- **Agents:** a lease belongs to the Claude Code session. It stays alive
  while the Claude Code process running that session lives. When the
  process dies, a 60-second grace lets `claude --continue` or `--resume` keep
  the leases; after that they end and the units go to the next waiters.
  `/clear` and quitting release at once. Subagents share their parent's
  leases. See [Claude Code](claude-code.md).
- **`clerk run`:** the lease lives exactly as long as the command.
- **`clerk acquire` on a terminal:** Ctrl+C releases, Ctrl+] detaches; a
  detached lease lives until released or expired.

## Notices

Notices tell a holder or waiter that something happened: an offer, a new
queue position, an expiry warning, a yield request, a unit back from a
borrower, a cleanup window ending, a hand-over without confirmed cleanup.
Each names the lease or request, the deadline and the expected action.
`clerk wait` prints them; agents keep it armed (see
[Claude Code](claude-code.md)).

## The audit log

Every inventory change, grant, hand-over (and whether cleanup was
confirmed), yield, override and promotion is recorded. `clerk log` shows it,
newest first.
