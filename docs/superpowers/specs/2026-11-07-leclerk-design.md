# Le Clerk: design

*"Un moment, s'il vous plaît… your esp32 is being prepared."*

Status: draft for review · Date: 2026-11-07 · Binary: `clerk`

## 1. Intent

Several AI coding agents (Claude Code sessions) work on one machine at the same
time. They share scarce local resources: ESP32 dev boards of several types,
test servers, databases, and so on. Today they coordinate by messaging each
other ("is board X taken?"). That burns time and tokens, and it fails anyway:
agents piled onto one board while identical boards sat idle, because they forgot
the others existed or didn't know about them.

Le Clerk is a local resource broker for agents. It gives agents one source of
truth for what exists, who holds what, and who waits for what. An agent asks
the clerk for a resource in one call and either gets a specific unit at once or
a place in the queue. Agents never negotiate with each other.

The name is self-mockery in the style of a pompous French bureaucrat. Its
messages and docs may keep that tone, provided they stay clear.

## 2. Goals, non-goals, anti-goals

### Goals (MVP)

- **One call to get a resource.** An agent asks for a resource (a pool) and gets
  a specific free, healthy unit at once, or a queue position. The wait does not
  block the agent.
- **Inventory agents can manage.** Agents create, update and delete resources
  and units. They choose stable IDs and record them in project docs or memory
  so later sessions reuse them.
- **Fair, cooperative sharing.** Leases have a declared duration, warnings
  before expiry, extension, cleanup on handover, and opt-in interruption by
  shorter or higher-priority work. The user can force an override.
- **Notices reach agents** whether they are idle, mid-turn, or running
  long background work, through documented mechanisms only.
- **No double use.** One physical thing is never leased to two agents at once,
  even if agents registered it twice.

### Non-goals (MVP)

- More than one machine: resources and agents are local to one host and one OS
  user.
- Agent harnesses other than Claude Code. The design should not block them,
  and choosing MCP Tasks helps here, but nothing is built or tested for them.
- Enforcement: blocking an agent from touching a unit it doesn't hold. Le Clerk
  is advisory, like most of the prior art. Enforcement hooks are a later phase.
- Restoring units after use, such as reflashing firmware. The next holder
  prepares the unit itself.
- Syncing with external licence servers, automatic hardware discovery, and
  resource types beyond the three in §4.3.
- GUI or web console.
- Monitoring a unit while it is held. Holders may do "all kind of crazy stuff";
  health matters only when a unit is handed out.

### Anti-goals: failure even if everything else works

- **One physical unit registered twice and used concurrently** by two agents.
  This is the worst failure. See §4.4.
- **Relying on undocumented Claude Code internals,** such as posting JSON into
  session sockets or forging the cross-session message envelope. This was
  spiked, worked, and was rejected by the user. See §6 and Appendix A.
- **The clerk taking a unit from a holder on its own initiative.** Only the
  holder's own yield, expiry or session death, or an override the user asked
  for, moves a held unit.
- **Agents still needing to message each other** to coordinate resources.

## 3. Prior art (summary)

The research is in the conversation that produced this document. What we
borrow:

- **labgrid:** reservations filtered by tags, priority, and a reservation token
  that expires unless refreshed. This is our pool-of-identical-units model.
- **Slurm and Kueue:** preemption with a grace period, suspend-and-resume
  rather than kill, and protection from preemption right after start.
- **FlexLM / LSF License Scheduler:** reserved headroom, per-group caps, and
  the finding that pausing does not free a licence.
- **Consul sessions / fencing tokens:** leases tied to liveness, and lease
  tokens that make a stale holder detectable.
- **Simlock:** identity per agent session, so subagents share a lease. Leases
  auto-renew while held and survive daemon restarts.
- **block/agent-task-queue:** local SQLite, a liveness check on the holder, and
  long output written to logs to save tokens.
- **agent-lease-mcp:** all-or-nothing claims and hook-based guards (guards are a
  later phase for us).
- **Allot:** wait-time estimates and declared durations.

None of them combines priority, opt-in interruption, cleanup handshakes,
agent-managed inventory with deduplication, and wake-ups for LLM agents. That
combination is Le Clerk.

## 4. Inventory model

### 4.1 Hierarchy

**Type → Resource → Unit.**

- **Type:** a built-in kind of thing: `device`, `service`, `generic` (§4.3).
  The type defines what a unit's settings look like, how duplicates are
  detected, and a default health check. Types are built into the binary, and
  agents don't create them.
- **Resource:** a pool created by an agent or the user. It has:
  - an agent-chosen **ID**: human-readable, stable, unique within its scope,
    e.g. `esp32-c6`, `thread-testbed`;
  - a type;
  - a **scope** (§4.2);
  - a one-line **description** and a **usage** template: instructions for
    agents, with unit placeholders (§4.7);
  - **tags** that agents search by, e.g. `esp32c6`, `thread` (§4.8);
  - an optional **health check** definition (§4.5);
  - creator and timestamps.
- **Unit:** a concrete instance inside a resource, e.g. the board on
  `/dev/serial/by-id/usb-Espressif_…`. It has:
  - its own ID, unique within its resource;
  - **settings** shaped by the resource's type (device path, address, port…);
  - a one-line **description** (e.g. "cracked antenna, hub port 3"), but no
    usage of its own: the resource's usage template covers all its units
    (§4.7);
  - a **retiring** flag (§4.6).

  What an agent receives on grant is the unit's ID and settings, both
  descriptions, the resource's usage rendered for that unit (§4.7), and the
  resource's tags.

A lease request names a resource and is satisfied by any eligible unit, or it
names one specific unit.

### 4.2 Scopes

- **User/system scope:** visible to every agent of this OS user on this
  machine.
- **Project scope:** visible only to agents working in that project. To agents
  in other projects these resources **do not exist**: they aren't listed and
  can't be leased or referenced.

**Project identity (my call):** the git repository's common directory, so all
worktrees of one repo are one project. Outside git, the working directory the
agent session started in. A session's project is fixed when it connects
(§7.3).

**ID clashes:**
- IDs are unique within a scope.
- If a project resource and a user/system resource share an ID, the project one
  wins inside that project (my call).
- **The clash is always reported** (user decision):
  - creating either resource returns a warning naming the shadowed or
    shadowing resource;
  - `resource_list`, `resource_get` and `clerk status` mark a project resource
    that hides a user/system one.

### 4.3 Types and duplicate detection

Each type has its own normalisation and matching. The goal is **suspicion**,
not certainty: catch the same physical thing reached through different names.

- **`device`**: device nodes and files (serial ports, block devices, files,
  directories).
  - Identity signals:
    - the canonical real path after resolving symlinks, so
      `/dev/serial/by-id/…` matches `/dev/ttyUSB1`;
    - for character and block devices, the device number;
    - for USB-backed devices, USB vendor, product and serial number from sysfs.
      These survive re-plugging and renumbering.
  - Default health check: the path exists and is accessible.
- **`service`**: network endpoints, both somewhere to connect to (a test server,
  a database) and somewhere to listen on (a dev server's address and port).
  - Identity signals:
    - scheme, normalised host and port;
    - `localhost`, `127.0.0.1` and `::1` are equivalent;
    - a wildcard listen address (`0.0.0.0`, `::`) overlaps any address on the
      same port;
    - an optional service ID (e.g. a container name) as an extra signal.
  - Default health check: a TCP connect for connect-type units; the port is
    free to bind for listen-type units.
- **`generic`**: anything else, such as licence-style pools of N units.
  - Identity is an exact match on an identity key the agent supplies.
  - No default health check.

The exact signals and how strongly each counts are left to the builder, within
this intent. They must be unit-tested per type.

### 4.4 Duplicate handling

Checks run whenever a unit is created or its settings change. They run against
**every unit on the machine, in all scopes**, regardless of visibility.

- **Match within the same scope:** refused. The response names the existing
  resource and unit, with their descriptions, so the agent can use them.
- **Project unit matching a user/system unit (user decision: warn).** Both are
  visible to the agent, so it is warned that the physical thing already exists
  at user level, and given that resource and unit so it can use them instead.
  The warning carries the existing resource's and unit's descriptions, so the
  agent can judge whether they are the same thing. If it goes ahead, the units are linked as described
  below.
- **Match with a unit in another project's scope:**
  - Not refused, because it may be fine or may be horribly wrong.
  - The agent is told *that* a similar unit seems to be registered in another
    project, and which project (my call: the project's path or name, not its
    settings, descriptions or usage).
  - It is offered a **promotion**: move the existing resource to user/system
    scope so both projects share one registration and one queue. The resource
    keeps its ID, so the original project's docs and memories stay valid.
- **Promotion edge cases** (my call):
  - If the promoted resource's ID clashes with an existing user/system
    resource, approval needs a new ID (`clerk approve <promotion> --as <id>`).
  - After promotion, units that were linked stay linked. They aren't merged.
    The user can remove the redundant unit. A merge operation is not in the MVP.
- **Promotion needs the user's confirmation** (my call), because it changes
  what another project sees. An agent can only *request* a promotion. The user
  approves it with the CLI (§8.4), which shows the resource's description, or by telling an agent so, in which case the
  agent applies it citing the user's request, the same way overrides work
  (§5.6).
- **If the agent registers the unit anyway**, the clerk records the two units
  as **linked**: the same physical thing. Linked units are mutually exclusive.
  A lease on either one blocks the other. This mechanism is what enforces the
  anti-goal in §2.

### 4.5 Health checks

- A health check is defined at **resource** level and runs **per unit**,
  receiving that unit's settings.
- **Each type offers a set of named built-in checks; a resource picks one of
  them or a custom script** (user decision). Proposed set (my call):
  - `device`:
    - `exists`;
    - `accessible` (openable read/write by this user);
    - `not-open` (user decision: no process has the file or device open; only
      processes of the same OS user are visible, which is enough here);
    - `none`;
    - default: `exists` + `not-open` (my call).
  - `service`: `tcp-connect` (default for connect units), `port-free` (default
    for listen units), `http` (GET on the endpoint returns 2xx), `none`;
  - `generic`: `none` (default);
  - any type: `custom` with a script.

  A resource that doesn't choose gets its type's default. **Checks can be
  combined** (my call): a resource's health check is a list, every entry must
  pass, and the first failure is reported.
  - My call: a user-defined command gets the settings as environment variables,
    not substituted into the command text, so a malicious or careless setting
    can't inject shell. Exit status 0 means healthy. It has a timeout.
- Checks run **only when the clerk is about to grant a unit.** An unhealthy unit
  is skipped in favour of another eligible unit; if none is healthy, the request
  keeps waiting. Held units are never checked or reported on.
- My call: the last result and its time are recorded per unit and shown in
  `clerk status`, so the user can see that a board was unplugged.

### 4.6 Changing and deleting

- Any agent that can see a resource may update or delete it, and add, update or
  remove its units (my call: no ownership rules in the MVP).
- A unit that is leased or queued for can't be removed. It can be marked
  **retiring**: no new grants, and it is removed when its current lease ends.
  A resource with held units is handled the same way.
- Every change is recorded in an audit log (§8.5).

### 4.7 Descriptions and usage instructions

Agents working with a resource need context: what it is, how to use it, what
"clean" means. The clerk stores that context and hands it over on grant
(user decision).

- **Description** (resource and unit): one line of plain text, at most 200
  characters. It says what the thing is. Line breaks are refused.
- **Usage** (resource only, user decision): Markdown instructions for agents,
  at most 4 KiB, e.g. how to flash or connect, gotchas, how to clean up. Units
  have no usage of their own; instead the resource's usage is a template that
  is filled in per unit.
- **Placeholders** (user decision): `{resource.<key>}` and `{unit.<key>}` in
  the usage text are replaced on grant. Keys (my call):
  - every type: `{resource.id}`, `{resource.description}`, `{unit.id}`,
    `{unit.description}`;
  - `device`: `{unit.path}`;
  - `service`: `{unit.mode}`, `{unit.scheme}`, `{unit.host}`, `{unit.port}`,
    `{unit.service_id}`, and `{unit.address}` (`host:port`, IPv6 hosts in
    brackets);
  - `generic`: `{unit.identity_key}`.

  My calls:
  - Only text of the exact form `{resource.<key>}` or `{unit.<key>}` (key:
    lowercase letters, digits, `_`) is a placeholder. Anything else in braces,
    such as shell `${PORT}` or JSON, passes through unchanged, so no escaping
    is needed.
  - A resource's settings shape is fixed by its type, so placeholders are
    checked when usage is written: an unknown key is refused, and the error
    lists the keys valid for that type.
  - Optional settings that are unset (`scheme`, `service_id`) render as empty
    text.
  - Substituted values are inserted as-is; the usage is instructions for an
    agent, not something the clerk executes.
- **Where agents see them:**
  - on grant (offered or held) and in the agent's own lease status: both
    descriptions and the rendered usage;
  - `resource_list` and listings: descriptions only, to keep them short;
  - `resource_get`: everything, with usage as the raw template so it can be
    edited.
- **Who writes them:** agents on create and update (the skill asks them to fill
  both in when they register something, §8.3); the user through the CLI
  (`--description`, `--usage`, `--usage-file <path>`, §8.4).
- Changing them is an inventory change and is audited (§8.5). It does not
  affect leases already granted: a holder sees the new text the next time it
  asks for its lease status.

### 4.8 Tags

Agents find resources by what they are, e.g. "a Thread devboard", without
knowing resource IDs (user decision).

- **Tags** (resource only, user decision): short labels such as `esp32`,
  `esp32c6`, `zigbee`, `thread`, `devboard`. Units have none; a resource's
  units are interchangeable, so the resource says what they are.
- **Format** (my calls):
  - lowercase ASCII letters, digits and `-`; starts with a letter or digit;
    1 to 32 characters;
  - at most 16 per resource;
  - duplicates collapse to one, and tags are stored sorted;
  - anything else is refused (`invalid_tag`, `too_many_tags`). Uppercase is
    refused, not lowercased, so agents learn the format.
- **Search** (user decision: a filter, not a lease): `resource_list` takes
  tags and returns the visible resources that carry **all** of them (my call:
  more tags narrow the search). The agent then picks a resource and leases it
  as usual. The clerk never leases "any resource with tag X".
  - Only visible scopes are searched; other projects' resources never match
    (§4.2).
  - Matching is exact (my call): no prefixes, no fuzzy matching, and
    descriptions are not searched.
- **Editing:** create sets the tags; update either replaces the whole set or
  adds and removes individual tags (my call: both, applied atomically, so the
  CLI's `--add-tag` cannot lose a concurrent change). Tag changes are
  inventory changes and are audited (§8.5).
- **Where they appear:** `resource_list`, `resource_get`, `clerk resources`,
  `clerk status`, the grant and the agent's lease status. Exports carry them.
- Duplicate detection ignores tags (§4.3).

## 5. Leasing

### 5.1 Request

An agent requests a lease with:
- **target:** a resource ID (any eligible unit), or a specific unit ID;
- **expected duration:** required;
- **interruptible:** whether this lease is open to interruption (§5.5);
  default no;
- **priority:** `low` / `normal` / `high`, default `normal` (my call: small
  fixed scale);
- **purpose:** a short free-text reason, shown to other agents and the user;
- **override:** optional, with a reason. Only for when the user told the agent
  to grab the resource anyway (§5.6).

My call: a request covers one unit. All-or-nothing requests for several units
are left for later.

The response is either an immediate **grant** (unit ID, unit settings, lease ID,
end time) or **queued** (position, estimated wait from declared durations).

### 5.2 Queue

- The queue is per resource. Ordering: by priority, then first come first
  served (my call).
- **Scheduling works per physical thing** (my call). Linked units (§4.4) form one
  **identity group**: one physical thing reachable through several resources,
  possibly in different scopes.
  - A free group is eligible for waiters of **every** resource that contains
    one of its members.
  - Waiters across those queues are ordered globally: priority, then arrival.
  - Yield, borrowing and paused-holder return (§5.5) work at group level.
  - An unlinked unit is simply a group of one.
- **Visibility across projects:** when a waiter is blocked by a holder in a
  project it can't see, it is told only "held by an agent in another project".
  It gets no purpose text, resource ID or settings.
- When a unit frees up, the head of the queue gets it after its health check
  passes.
- **Claiming an asynchronous grant** (my call).
  - A grant made while the agent is waiting starts as **offered**. The agent
    must claim it, with `lease_claim` or any lease call carrying the lease ID,
    within a claim window (default 5 min).
  - An unclaimed offer goes to the next waiter. The requester's queue entry is
    dropped, with a notice.
  - The lease clock starts at claim time.
  - A grant returned directly from `lease_acquire` is claimed at once.
  - This stops a board from sitting idle while its new owner hasn't noticed.
- A paused holder whose unit is lent out (§5.5) is **ahead of the whole queue**
  for that unit.
- An agent can cancel its queued request. A queued request disappears when the
  requesting session ends.

### 5.3 Holding, warnings, extension

- A lease ends at grant time plus the declared duration.
- Before the end, the holder gets an **expiry warning** and may **extend**,
  stating a new expected end time.
  - My call: a default lead time of 5 minutes, or 20% of the duration for
    short leases. Configurable.
  - My call: an extension is refused if the lease is non-interruptible and
    someone of strictly higher priority is waiting. The holder is told who is
    waiting and can switch the lease to interruptible.
- **Expiry is hard (user decision).** Without an extension, the lease ends at
  its time **even if nobody is waiting**. Expiry starts the cleanup phase
  (§5.4). If the holder doesn't confirm cleanup in time, the unit becomes free
  and is marked **dirty**. The holder may still be running something on it.

### 5.4 Handover with cleanup

Every handover goes through a cleanup phase, whether the cause is release,
yield, expiry, or an override:

```
held ──(release / expiry / yield request accepted / override)──▶ cleaning
cleaning ──(holder confirms "clean" or "yielded")──▶ handed over, cleanup confirmed
cleaning ──(cleanup window passes, or holder's session ends)──▶ handed over, cleanup NOT confirmed
```

- The holder gets a **cleanup window**.
  - My call: 2 minutes by default. The holder may ask for one extension, and
    the request states a reason.
- The holder **confirms** when it has cleaned up.
- If the window passes, or the holder's session dies, the unit is handed over
  anyway, or goes back to the free pool if nobody is waiting. Either way it is
  marked **dirty**.
- **Dirty** is a unit state meaning the last handover didn't confirm cleanup.
  - It records why: cleanup window passed, lease expired, session ended, or
    override.
  - **Whoever gets a dirty unit next is told on grant**, so it is ready for a
    messy state.
  - The flag clears when that new owner is granted the unit; the warning has
    then been delivered.
  - `clerk status` shows dirty units.
- The clerk never restores, reflashes or resets a unit.

### 5.5 Interruptible leases and yielding

- A holder opts in by marking its lease **interruptible**. For example, a
  network stress test that can be paused and resumed safely.
- A request **can interrupt** an interruptible lease when the requester has a
  higher priority than the holder, or the same priority and a declared
  duration shorter than the time left on the holder's lease. That precise rule
  is my call.
- When all eligible units are held and at least one holder can be interrupted,
  the clerk sends a **yield request** to one of them.
  - My call on which one: prefer the one with the most time left, then the
    lowest priority.
- **The holder controls yielding.** It picks a safe point, pauses its work,
  cleans up, and confirms "yielded".
  - The holder may refuse.
  - My call: a refusal, or no answer within a configurable window, moves the
    request on to the next interruptible holder. Failing that, the request
    waits at the head of the queue.
- While the unit is lent out:
  - the paused holder's lease clock **stops**;
  - the borrower's lease uses the borrower's own declared duration.
- When the borrower releases, after its own cleanup, the unit **goes straight
  back to the paused holder**, whose clock resumes. The holder is told "your
  unit is back", including whether the borrower confirmed cleanup.
- **If the borrower overruns** its declared duration, the paused holder is told
  and may **reclaim**: the borrower gets the usual cleanup window, then the unit
  returns.

### 5.6 Override ("grab it anyway")

- Only used when the user tells an agent to take a resource regardless. The
  request carries `override` and a reason.
- It works against non-interruptible leases too. The holder gets a notice and
  its cleanup window, and the unit is handed over whether or not it confirms.
  The new owner is told if cleanup wasn't confirmed.
- The clerk does not verify that the user really asked; these are the user's
  own agents on the user's machine. Every override is written to the audit log
  with the agent's stated reason.
- My call: the overridden holder's lease **ends**. It isn't paused, because a
  non-interruptible job usually can't simply resume. The holder may queue
  again.

### 5.7 Session death

If a holding or waiting session goes away (§7.3):
- its queued requests are dropped;
- its held units go through §5.4 as "not confirmed: session ended";
- a paused holder that dies forfeits its claim to get its unit back.

## 6. Notices to agents

Notices tell an agent that something happened:
- granted;
- position changed;
- expiry warning;
- yield requested;
- your unit is back;
- the borrower overran;
- cleanup window ending;
- unit handed over to you without confirmed cleanup.

**Constraint: only documented Claude Code and MCP mechanisms.** Two kinds were
spiked and rejected, and must not be used:
- writing JSON lines into Claude Code's per-session inbox socket from outside;
- forging the `<cross-session-message>` envelope.

They worked, but they are undocumented. Agents also treated those messages with
suspicion. See Appendix A.

The notices themselves are an event stream on the daemon. Getting them to
agents works in layers:

1. **Primary: MCP Tasks.**
   - The tools that can take a long time, `lease_acquire` when the request is
     queued and `lease_wait`, declare task support.
   - A client that supports tasks gets a task handle at once and the result
     when it completes. In Claude Code, an idle session starts on a completed
     task's result unprompted.
   - This is the protocol-level mechanism, which also makes porting to other
     agents easier. The user explicitly favoured it for that reason.
2. **Fallback: a blocking `lease_wait`.**
   - For clients without tasks, `lease_wait` simply blocks until there is a
     notice.
   - Claude Code moves an MCP call still running after 2 minutes to a
     background task and delivers the result later, waking an idle session.
     This was verified in the spike.
   - While it waits, the server sends progress notifications. That keeps the
     call alive past the idle limit, about 30 minutes for stdio servers. This is
     documented but not yet verified, see §11.
3. **Re-arming.**
   - `lease_wait` returns one batch of notices and ends. Its result tells the
     agent to call it again while it holds or waits for anything.
   - A **Stop hook** checks with the daemon at the end of each turn. If the
     session holds a lease or a queued request but has no open `lease_wait`, the
     hook adds a reminder to the context.
4. **Mid-turn backup.** A **PreToolUse hook** attaches pending notices to the
   context on the next tool call. These are documented `additionalContext`
   hooks.
5. **Optional extra wake-up: `asyncRewake`.** A hook that runs in the
   background and exits with code 2 when a notice arrives. Hook timeouts are
   enforced, as the spike confirmed, so it must be bounded and re-armed on each
   Stop. The builder decides whether the MVP needs it once layers 1–3 work.

Hooks 3–5 are Claude Code-specific and live in the integration package (§9).
The daemon and MCP surface stay client-neutral.

## 7. Architecture

### 7.1 One binary, three roles

The binary is `clerk`. Every invocation first checks whether the daemon is
running and starts it if not. The roles:
- **Daemon** (`clerk daemon`, normally started automatically):
  - a resident process that owns all state;
  - runs the scheduler and the timers (expiry, warnings, cleanup windows);
  - runs health checks;
  - serves an MCP endpoint over **Streamable HTTP**, which streams with SSE, as
    the user specified.
  - My notes: plain "HTTP+SSE" is a deprecated MCP transport, so Streamable
    HTTP is the right reading. Clients reach it on a **Unix socket** in the
    user's runtime directory, with permissions limited to the user, rather than
    a TCP port. That avoids clashing ports, and it is itself the kind of
    resource this tool manages.
- **stdio proxy** (`clerk mcp`): what Claude Code launches as its MCP server.
  - It forwards MCP traffic between stdio and the daemon, unchanged in meaning.
  - It adds the session's identity (§7.3).
  - Its connection is the session's liveness signal.
- **CLI** (`clerk <command>`): for the user and for scripts (§8.4). It talks to
  the daemon through the same socket.
  - My call: through an admin API alongside MCP, rather than by speaking MCP.
    Left to the builder.

### 7.2 Daemon lifecycle

- **Starting:** several invocations may race to start the daemon. Exactly one
  daemon must win, and the others connect to it (a lock file or equivalent).
  The daemon is detached from the terminal that started it.
- **Persistence:** resources, units, leases, queue and audit log live in SQLite
  under the user's state directory. A daemon restart loses nothing.
  - The daemon is the only writer.
  - WAL mode, so CLI reads such as `status` and `doctor` don't block it.
  - Every scheduling decision (grant, handover, link check) is **one
    transaction**. Together with the single writer, this makes the "no double
    use" rule (§2) survive crashes.
  - Deadlines are stored as absolute times. After a restart, timers are rebuilt
    from the database.
  - My call on the crate: `rusqlite` with bundled SQLite, used from a dedicated
    database thread, with numbered schema migrations.
- **After a restart:** sessions get a reconnection grace period, before their
  leases are treated as belonging to dead sessions.
- My call: the daemon keeps running while idle. Shutting down after idling is
  left to the builder.
- A version mismatch between a client binary and the running daemon must be
  detected and reported (e.g. after an upgrade).

### 7.3 Agent identity and liveness

- **Identity:** the Claude Code session. The stdio proxy reads
  `CLAUDE_CODE_SESSION_ID`, which Claude Code sets for stdio MCP server
  processes, and the session's starting working directory, which determines
  the project (§4.2).
- Subagents share their parent session's MCP connection, and therefore its
  leases. That is deliberate, as in Simlock.
- **Liveness:**
  - The session is alive while its proxy is connected.
  - When the proxy disconnects, a short grace period (my call: 60 s) allows for
    reconnection.
  - After that, the session is dead (§5.7).
  - A `SessionEnd` hook may additionally tell the daemon immediately.
- **Resumed sessions:**
  - Known risk: on `--continue` / `--resume`, the proxy may receive the
    original start-up session ID.
  - Mitigation: identity is (session ID, proxy connection). A new connection
    with the same session ID takes over that session's leases and queue
    entries.
  - Verify early, see §11.
- **Fallback if `CLAUDE_CODE_SESSION_ID` is missing** (my call): identity is
  the proxy connection itself. The session still works, but a resumed session
  loses its leases.
- **Other kinds of holder (user decision): the CLI.** The user, scripts and
  smoke tests can hold and wait for leases through the CLI (§8.4). My calls on
  how:
  - **`clerk run <resource|unit> [lease options] -- <command>`:**
    - Waits in the queue in the foreground, then runs the command with the
      unit's settings in environment variables.
    - The lease lives exactly as long as the command. When the command exits,
      cleanup is confirmed automatically if it exited 0. Otherwise the unit is
      marked dirty.
    - This mode is meant for smoke tests and scripts.
  - **`clerk acquire <resource|unit>`** (user decision): its behaviour depends
    on whether stdin is a terminal.
    - **Interactive (stdin is a TTY):**
      - blocks in the foreground and shows queue position, then the grant
        (unit and settings), then expiry warnings and other notices as they
        happen;
      - **Ctrl+C releases** the lease, or cancels the request while still
        queued. My call: a deliberate Ctrl+C counts as cleanup confirmed.
      - **A detach shortcut** (my call: Ctrl+], as in telnet) leaves the lease
        held in the background and returns the shell. The lease then lives
        until `clerk release <lease>` or expiry.
      - `clerk attach <lease>` re-attaches the live view (my call).
    - **Non-interactive (agents' Bash, scripts):**
      - never needs a TTY;
      - prints a structured result (grant, or queued with position) and exits;
      - the lease, or the queued request, stays held detached.
      - `--wait` blocks until granted, for scripts. Agents should prefer the MCP
        tools for waiting, because a long foreground Bash call gets moved to
        the background.
  - **`clerk release <lease>`** releases from any shell.
  - **Agents can use the CLI directly.** When `CLAUDE_CODE_SESSION_ID` is set
    in the environment (Claude Code sets it for Bash commands), a lease made
    through the CLI belongs to **that agent session**. It behaves exactly like
    an MCP lease: session liveness, notices through `lease_wait`, and
    interruptible if requested.
  - **A lease from a CLI with no agent session** is owned by the user or
    script.
    - Kept until released or expired.
    - Notices appear in the attached view, `clerk status` and `clerk wait`.
  - **CLI holders without an agent session are never interruptible** in the
    MVP. Overrides still apply:
    - for `clerk run`, the command gets SIGTERM when its cleanup window ends;
    - for `clerk acquire`, the lease simply moves on and the unit is marked
      dirty.
  - **Liveness:** the `clerk run` process's PID; for `clerk acquire`, the
    lease's own expiry.
- **Lease tokens:** each grant carries a lease ID. Calls that act on a lease
  must present it, so a stale holder (after takeover or handover) is rejected.
  This is a fencing token in the Consul / AI Sense sense.

### 7.4 Stack (my call; user said "Go or Rust")

- **Rust, with the official MCP Rust SDK (`rmcp`), and SQLite.**
- Reason: the official Go SDK does not implement MCP Tasks yet (go-sdk issue
  #626 is still open, and its v1.7.0 audit fails the Tasks scenarios). `rmcp`
  supports tasks, and tasks are central to this design.
- Platform: Linux first. macOS should work but is not a target for the MVP.

## 8. Interfaces

### 8.1 MCP tools (indicative; the builder may rename or merge)

**Inventory:**
- `type_list`: built-in types and their settings shape.
- `resource_create`, `resource_update`, `resource_delete`, `resource_list`,
  `resource_get`. `resource_list` shows only visible scopes, with unit counts,
  how many are free, descriptions and tags, and filters by tags (§4.8).
  Create and update take a description, a usage template (§4.7) and tags.
- `unit_add`, `unit_update`, `unit_remove`, `unit_retire`. Adding or updating a
  unit returns duplicate results (§4.4).
- No separate promotion tool (my call): a `similar_in_other_project` warning
  already files the promotion request for the user (§4.4).
- `resource_create` defaults to the session's project scope; user scope only
  when asked (my call, matching the CLI).
- Never exposed to agents (enforced by the daemon): `view: all`, admin
  operations, the audit log, the promotion list and approval, holder lookups.

**Leasing:**
- `lease_acquire` (§5.1). Returns a grant at once, or a task / queued result.
  A grant carries the descriptions, the rendered usage (§4.7) and the
  resource's tags.
- `lease_status`: this session's leases, queue positions, pending notices.
- `lease_extend`.
- `lease_release`: starts cleanup.
- `lease_confirm`: "clean" or "yielded".
- `lease_set_interruptible`.
- `lease_yield_refuse`.
- `lease_reclaim`: a paused holder takes its unit back after the borrower
  overruns.
- `lease_cancel`: cancel a queued request.
- `lease_claim`: claim an offered unit (my call; `lease_wait` and a completed
  `lease_acquire` task claim offers themselves, since the waiting call proves
  the agent is present).
- `lease_wait`: blocks, or runs as a task, until notices exist, then returns
  them.

**Results** are short and structured, and always state the next step the clerk
expects ("call `lease_confirm` when clean"). Agents shouldn't need the skill
loaded to do the right thing.

### 8.2 Notice content

Every notice states:
- what happened;
- which lease or unit, and the other party's purpose text;
- the deadline, if any;
- the expected action.

Notices come from the clerk's own tools and hooks, never as cross-session
messages.

### 8.3 Skills

Two Claude Code skills ship with the integration package (user decision).

**Leasing (agent etiquette)** teaches agents:
- when to lease, and to declare honest durations;
- to mark long, pausable work as interruptible;
- to keep `lease_wait` armed;
- to confirm cleanup;
- to record resource and unit IDs in project docs or memory after creating
  them;
- to read the descriptions and usage on grant;
- to use `override` only when the user said so;
- never to coordinate resources by messaging other agents.

**Registering resources** covers both moments: the user asks an agent to
register things ("register the boards on my hub"), and an agent needs something
that isn't registered yet. It is generic across types; serial boards are one
example among services and generic pools. Discovery is the agent's prerogative:
the clerk has no scan tool (user decision). The steps:
1. Search first (`resource_list` by tags) and reuse what exists.
2. Pick the type: `device` (reached by a path), `service` (host and port,
   connect or listen), `generic` (anything else with an identity key).
3. Group interchangeable things into one resource, one unit each.
4. Fill in the context: ID, description, tags (what it is, its capabilities,
   its kind), and usage in Markdown with `{unit.*}` placeholders, gotchas and
   what "clean" means (§4.7, §4.8).
5. Create, add units, and act on duplicate warnings. A similar thing in another
   project means telling the user about the promotion, never re-registering
   it.
6. Scope: the agent's project by default; user scope only when the user says
   the thing is shared.
7. Record the IDs in project docs or memory.

### 8.4 CLI (for the user)

At minimum:
- `clerk run`, `clerk acquire`, `clerk attach`, `clerk release`, `clerk wait`:
  leases held by the user, scripts, smoke tests, or agents through their Bash
  tool (§7.3).
- **Every command works for people and for programs** (user decision):
  - interactive output and prompts when stdin/stdout is a TTY;
  - plain, structured output (e.g. JSON) with no prompts otherwise, so agents
    can call any command directly.
- **Inventory (user decision):** the CLI can do everything the MCP inventory
  tools can (§8.1):
  - `clerk resource create|update|delete|list|show`, with a `--scope` option
    for user or project;
  - `clerk unit add|update|remove|retire`;
  - `--description` on both, and `--usage <text>` / `--usage-file <path>` on
    resources (§4.7);
  - `--tag` on resource create, `--add-tag` / `--remove-tag` on update
    (§4.8);
  - **YAML input** (user decision): `resource create|update` and
    `unit add|update` take `-f <path>`, or `-f -` for stdin, holding the same
    fields as the MCP tools, e.g. a multi-line `usage` as a YAML block. The
    ID stays on the command line. My calls:
    - stdin is read only with `-f -`, never implicitly, so an agent's Bash
      call cannot hang on an input stream that never closes;
    - on update, only the keys present change;
    - a field set by both a flag and the YAML is refused, not merged;
    - unknown keys are refused, naming the key.

  The same rules apply: duplicate checks, warnings, and retiring a unit that is
  held.
- `clerk status`: units, holders, queues, last health result, dirty flags.
- `clerk resources [--tag <tag>…]`, `clerk leases`, `clerk queue <resource>`.
- `clerk release --force <lease>`: the operator override.
- `clerk promotions` and `clerk approve <promotion>`.
- `clerk log`: the audit log.
- `clerk daemon status|stop|restart`.
- **Hard management tools (user decision)**, for when things go wrong.
  - They live under `clerk admin …` and bypass the normal rules: no cleanup
    windows, no checks on whether a unit is held.
  - Every one of them goes into the audit log.
  - The set (names indicative):
    - `admin release <lease>`: end a lease now, with no cleanup window. The unit
      is marked dirty.
    - `admin evict <session|holder>`: drop every lease and queued request of a
      session or holder, e.g. a session that is stuck but still connected.
    - `admin dequeue <request>`, `admin clear-queue <resource>`.
    - `admin remove-unit --force`, `admin delete-resource --force`: remove even
      when held or queued for. Affected holders and waiters are notified.
    - `admin clean <unit>`: clear a dirty flag.
    - `admin link <unit> <unit>`, `admin unlink <unit> <unit>`: fix what the
      duplicate detection got wrong.
    - `admin move <resource> --to user|project:<path>`: move a resource
      between scopes, in either direction, without the promotion flow.
    - `admin rename <resource|unit> <new-id>`.
    - `admin edit-lease <lease>`: change end time, priority, or the
      interruptible flag.
    - `admin doctor`: check the state for consistency (orphaned leases, holders
      that no longer exist, units whose device has vanished) and offer fixes.
    - `admin export`, `admin import`: back up and restore the inventory as a
      file.
  - **Safety** (my call):
    - Interactive use asks for confirmation. Non-interactive use needs
      `--yes`.
    - Run from an agent session (`CLAUDE_CODE_SESSION_ID` set), every
      `admin` command also needs `--override "<reason>"`, the same rule
      as overrides (§5.6). Agents can therefore help with clean-up when you ask
      them to, and the log shows who did what.

The CLI sees all scopes. It is the user's view.

### 8.5 Audit log

The audit log records every inventory change, grant, handover (and whether
cleanup was confirmed), yield request and answer, override with its reason, and
promotion. It is kept in SQLite and shown by `clerk log`.

## 9. Claude Code integration package

My call: shipped as a **Claude Code plugin**, with an install command as an
alternative (e.g. `clerk install claude-code`). It contains:
- the MCP server registration: stdio, `clerk mcp`;
- the hooks from §6 (Stop reminder, PreToolUse notice injection, optional
  asyncRewake) and an optional SessionEnd notification;
- the two skills (§8.3).

Hooks call `clerk` CLI subcommands meant for hooks. They must fail open: if the
daemon is unreachable, the agent's work is never blocked. The scope the plugin
is installed in (user or project) is the user's choice at install time.

## 10. Success criteria

- An agent gets a matching free unit in **one tool call**. With several
  identical boards, all of them get used before anyone queues.
- No physical unit is ever held by two agents at once, including after
  duplicate registration across projects. Tests demonstrate this.
- An urgent short request against a pool fully held by interruptible leases is
  served after a holder yields. The holder gets its unit back and resumes.
- Idle, mid-turn and busy-in-background agents all receive expiry warnings and
  yield requests in time, using only documented mechanisms.
- Agents never need to message each other about resources.

## 11. Verify first (risks to settle at the start of implementation)

1. **Which Tasks version `rmcp` implements:** the older experimental one
   (SEP-1686) or the `io.modelcontextprotocol/tasks` extension (SEP-2663, MCP
   2026-07-28) that Claude Code 2.1.319 speaks. Also whether Claude Code needs
   feature-flag fetching for tasks; its docs and changelog disagree.
2. Whether a `lease_wait` call with progress notifications survives longer than
   the stdio idle limit (about 30 min) in Claude Code. The spike's attempt to
   shorten the limit had no effect, so this is still unverified.
3. **Behaviour of `CLAUDE_CODE_SESSION_ID`** after `--continue` / `--resume`,
   and whether subagents see the same value.
4. **Streamable HTTP over a Unix socket** with `rmcp` on both the server side
   and the client (proxy) side.
5. How Claude Code shows a completed MCP task when the session is mid-turn, as
   opposed to idle. Idle was verified for auto-backgrounded calls.

**If an item fails, these are the decided fallbacks** (my calls; none of them
stops the MVP):
- **Tasks mismatch (1):**
  - The MVP must be fully usable on the blocking `lease_wait` fallback alone.
    Tasks are an improvement on top of it.
  - If `rmcp`'s Tasks version doesn't match what Claude Code speaks, implement
    the extension's wire format directly on top of `rmcp` rather than forking
    it.
  - The stack is not reopened.
- **Unix socket (4):** loopback TCP on a random port. The port and an access
  token go in a file only the user can read, in the runtime directory.
- **Session ID (3):** see the §7.3 fallback.

**Testing approach** (my call):
- Automated tests at daemon and scheduler level cover every behaviour in §4–§5
  and §7. They follow the plan's BDD skeleton.
- Delivery to agents (§6, §10) is checked with a scripted end-to-end harness
  that drives a real Claude Code session in tmux, like the spike. It runs on
  demand, not in CI.

## 12. Decided by the user vs. left to the builder

**Decided by the user:**
- local only;
- Claude Code only for the MVP;
- documented mechanisms only;
- MCP Tasks as the main mechanism;
- the Type → Resource → Unit hierarchy;
- agent-chosen IDs recorded in docs and memory;
- user/system and project scopes, with project resources invisible elsewhere;
- per-type duplicate detection, with a warning and a promotion offer across
  projects;
- health checks defined per resource, run per unit, only when granting;
- types `device`, `service` (including listen addresses), and `generic`;
- a description per resource and unit, and usage instructions per resource
  with unit placeholders, handed over on grant;
- tags per resource, used by agents to search, never to lease "any match";
- two skills, leasing and registering resources; no scan tool, discovery is
  the agent's job;
- hard expiry even when nobody is waiting, marking the unit dirty and warning
  its next owner;
- CLI holders for the user, scripts and smoke tests;
- a cleanup phase on every handover, with unconfirmed cleanup reported to the
  new owner;
- expiry warnings and extension;
- interruptible leases where the holder controls yielding;
- user-directed override;
- a single `clerk` binary that auto-starts the daemon, with a stdio proxy to
  the daemon's SSE endpoint;
- Go or Rust (Rust chosen, §7.4);
- the name.

**My calls, flagged in place.** Each is reasonable but open to change:
- project identity from the git common directory;
- project IDs win over user/system IDs inside a project;
- promotion needs the user's approval;
- linked duplicates are mutually exclusive;
- health checks receive settings as environment variables;
- the priority scale and the exact interruption rule;
- which holder gets asked to yield;
- default windows (5 min warning, 2 min cleanup, 60 s reconnection);
- override ends the holder's lease rather than pausing it;
- one unit per request;
- scheduling per physical thing across linked units, and "held by an agent in
  another project" for holders a waiter can't see;
- an "offered" state with a claim window for grants made while the agent waits;
- the forms of CLI holder: `run` and `acquire`, non-interruptible;
- the session-ID fallback;
- the description and usage limits, the placeholder syntax and keys, and
  write-time placeholder checks;
- the tag format and limits, all-tags matching, exact matching, and
  replace-or-add/remove editing;
- YAML input through an explicit `-f -`, with flag/YAML conflicts refused;
- promotion edge cases;
- fallbacks if verify-first items fail;
- the testing approach;
- a Unix socket instead of TCP;
- a CLI admin API;
- shipping as a plugin;
- Rust.

**Left to the builder:**
- exact tool names and data shapes;
- the scoring of duplicate signals per type;
- the SQLite schema;
- the daemon start-up lock mechanism;
- idle shutdown;
- whether `asyncRewake` is in the MVP;
- CLI output formats;
- the bureaucratic tone of messages, within clarity.

## Appendix A: spike findings (2026-11-07, Claude Code 2.1.319)

- **Session inbox socket** at `/run/user/<uid>/cc-socks/<pid>.sock`:
  - An outside process could post into any session, and idle sessions woke.
  - The message format and the cross-session envelope are undocumented.
  - Receiving agents treated these messages as unverified peer requests and
    declined to act without context.
  - Leftover sockets from crashed sessions remain on disk.
  - **Rejected.**
- **Long foreground commands:** current Claude Code steers agents to run long
  work (e.g. stress tests) in the background, so a holder normally stays
  reachable.
- **`asyncRewake` hook:** woke an idle session at exactly the moment it fired.
  The output arrived as a trusted system reminder, and the agent acted on it.
  Hook timeouts are enforced: a hook killed by its timeout never wakes the
  session.
- **Long MCP tool call:** moved to the background as an "MCP task" after
  120 s. The session went idle, and the later result woke it and was acted on
  at once.
- **Idle-timeout control:** `CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT=60000` did not
  shorten the idle limit (a 100 s call without progress succeeded), so whether
  progress notifications keep a call alive remains unverified (§11.2).

## Appendix B: phase 3 verify-first findings (2026-11-10, Claude Code 2.1.321)

Spike code and logs: `docs/superpowers/spikes/2026-11-10-p3/` (own cargo
project, not built by the crate). `spike-server` is an rmcp Streamable HTTP
service nested at `/mcp` in an axum router on a Unix socket; `spike-relay` is a
raw stdio relay; `spike-client` is rmcp's own Unix-socket client. Runs used
`claude -p` and one interactive session in tmux, with a scrubbed environment
(`clean-claude.sh`). Docs read on 2026-11-10: code.claude.com/docs/en
`mcp.md`, `env-vars.md`, `hooks.md`, `plugins-reference.md`,
`plugin-marketplaces.md`.

**Results, one line per item:**

- **Pins:** `rmcp = "=3.8.0"` (2026-10-26; 3.9.0 is under two weeks old),
  with `rmcp-macros` held at 3.8.0 in `Cargo.lock`; features `server`,
  `macros`, `transport-streamable-http-server`, `client`,
  `transport-streamable-http-client-unix-socket`. `schemars = "1.2.3"`
  (rmcp needs `^1.1`).
- **§11.1 Tasks: verified.** rmcp 3.x implements the SEP-2663
  `io.modelcontextprotocol/tasks` extension (`TaskManager`,
  `CallToolResponse::Task`, `tasks/get|update|cancel`; SEP-1686 was replaced in
  3.0.0-beta.1). Claude Code 2.1.321 declares the same extension and needs no
  flag. In an interactive terminal session it got a task handle at once, polled
  `tasks/get` every `pollIntervalMs` (rmcp default 1 s), showed `MCP task <id>
  (spike/slow) completed.` and an idle session acted on the result unprompted
  (`logs/server.log`). Per the MCP docs ("What a call needs to become an MCP
  task"), tasks are **not** used for: one-shot `claude -p`, calls from
  subagents, the desktop app and VS Code, third-party providers, or sessions
  without feature-flag fetching. Those calls are plain blocking calls.
- **§11.2 Progress past the idle limit: verified.** With
  `CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT=20000` (observed as a 30 s window; whether 30 s is a floor is unconfirmed), a 40 s
  call without progress aborted with "sent no response or progress for 30s",
  and a 40 s call with progress every 5 s succeeded (`logs/run7-*.json`).
  Appendix A's failed attempt most likely lost the variable before Claude Code
  saw it. Documented defaults: 30 min for stdio, 5 min for HTTP.
  Observation: on that abort Claude Code sent no `notifications/cancelled`; the
  server call ran on and its result was dropped.
- **§11.3 `CLAUDE_CODE_SESSION_ID`: verified, with a documented gap.** Set for
  the stdio MCP process and equal to hook input `session_id` on a fresh start
  and on `--resume <id>` (both `-p` and interactive). On `--continue` the MCP
  process got a fresh start-up ID while hooks got the resumed ID (reproduced
  twice; env-vars.md says the same). `/clear` gives hooks a new ID (SessionEnd
  `reason: "clear"`, then SessionStart `source: "clear"`) while the running
  MCP process keeps the old one. Subagents share the parent's MCP process and
  ID; their hook input carries the parent `session_id` plus `agent_id` and
  `agent_type`. The MCP process's parent PID equals `CLAUDE_PID`, which
  Claude Code sets (documented) for hook commands but not for MCP servers.
- **§11.4 Streamable HTTP over a Unix socket: verified, both sides.**
  `StreamableHttpService` mounts with `Router::nest_service("/mcp", …)` served
  by `axum::serve(UnixListener)`. Client side: rmcp's
  `StreamableHttpClientTransport::from_unix_socket_with_config` with
  `custom_headers` (`logs/run8-*.log`) and a raw hyper relay. Custom headers
  reach handlers as `http::request::Parts` in `RequestContext::extensions`.
  Requests need `Host: localhost` (rmcp's default host allowlist).
- **MCP session and identity: plan assumption fails.** Claude Code 2.1.321
  speaks protocol 2026-07-28 to stdio servers: it opens with `server/discover`,
  sends no `initialize`, and every request carries `_meta`
  (`io.modelcontextprotocol/protocolVersion`, `clientInfo`,
  `clientCapabilities`, `progressToken`). Sessions were removed in that
  revision (SEP-2567), so rmcp serves it statelessly and no `Mcp-Session-Id`
  exists. Over HTTP rmcp also requires `MCP-Protocol-Version`, `Mcp-Method`
  and `Mcp-Name` (SEP-2243), which the relay derives from each message.
- **§11.5 Completed task while mid-turn: verified.** A task finished while a
  second, blocking call was running in the same turn. Claude Code saw it
  complete at once, but delivered it to the model only after the running call
  returned, as the same `MCP task … completed.` notice, still within the turn.
- **Documented schemas** (field names verbatim):
  - Plugin manifest `.claude-plugin/plugin.json`: `name` (only required,
    kebab-case, must not start with `claude-`), `version`, `description`,
    `author{name,email,url}`, `homepage`, `repository`, `license`, `keywords`,
    `skills`, `hooks`, `mcpServers`; components at the plugin root
    (`skills/<name>/SKILL.md`, `hooks/hooks.json`, `.mcp.json`);
    `${CLAUDE_PLUGIN_ROOT}` in commands. Check: `claude plugin validate <dir>`.
    plugins-reference.md.
  - Marketplace `.claude-plugin/marketplace.json`: `name`, `owner{name}`,
    `plugins[{name, source: "./relative/path", description}]`; entry name
    equals the manifest name; no `..` in `source`. Install:
    `claude plugin marketplace add <dir>`,
    `claude plugin install <plugin>@<marketplace>` (user scope by default).
    plugin-marketplaces.md.
  - Plugin `.mcp.json`: `{"mcpServers": {"<name>": {"command", "args",
    "env"}}}`. mcp.md "Plugin-provided MCP servers".
  - `hooks/hooks.json`: `{"hooks": {"<Event>": [{"matcher", "hooks": [{"type":
    "command", "command", "timeout"}]}]}}`, `timeout` in seconds.
    SessionEnd hooks get 1.5 s in total unless a settings-file hook raises it;
    whether a plugin hook's `timeout` raises that budget is inferred, not documented: design for 1.5 s. hooks.md.
  - Hook input, common: `session_id`, `transcript_path`, `cwd`,
    `permission_mode`, `hook_event_name`, plus `agent_id`/`agent_type` inside
    subagents. Stop: `stop_hook_active`, `last_assistant_message`,
    `background_tasks[{id,type,status,description,server,tool}]` (type
    `"MCP task"` for tasks), `session_crons`. PreToolUse: `tool_name`,
    `tool_input`, `tool_use_id` (observed `mcp_server` too). SessionEnd:
    `reason` (`clear`, `resume`, `logout`, `prompt_input_exit`, `other`).
    SessionStart: `source` (`startup`, `resume`, `clear`, `compact`, `fork`).
    hooks.md.
  - Hook output: Stop block is `{"decision": "block", "reason": "…"}`
    (a continuation cap is reported but not confirmed in the docs fetched); PreToolUse context is `{"hookSpecificOutput":
    {"hookEventName": "PreToolUse", "additionalContext": "…"}}` (10,000
    characters max). hooks.md.

**Plan impact** (contradictions with the phase 3 plan; recommended rulings,
for the user to decide):

1. **No MCP session to bind identity to.** "Bound once at `initialize`" and
   "later headers are ignored" cannot hold: Claude Code sends no `initialize`
   and 2026-07-28 has no MCP session. Recommended: the proxy sends the
   `X-Clerk-*` headers on every POST, and the daemon builds `McpIdentity` per
   request from `http::request::Parts`. A request without `X-Clerk-Session` is
   refused. Drop the "later requests with different headers" scenario. The
   socket is user-only and only the proxy speaks to it, so nothing is lost.
2. **The proxy must speak 2026-07-28 HTTP framing.** It must set
   `MCP-Protocol-Version`, `Mcp-Method` and `Mcp-Name` from each message, relay
   JSON and SSE responses, and send requests concurrently. The first message is
   `server/discover`, not `initialize`. Task 2 tests should cover a
   2026-07-28 client as well as rmcp's default client, which negotiates
   2025-11-25 through `initialize`.
3. **Tasks adopted, but the blocking path stays primary.** Tasks are
   compatible, but subagents, the desktop app, VS Code and `-p` never get
   them. Recommended: `lease_acquire` and `lease_wait` return a task only when
   the per-request `clientCapabilities` declare the extension. Use a
   `pollIntervalMs` of about 5 s. Count an open task as an open wait for the
   Stop hook.
4. **Hook `session_id` and proxy session can differ** (`--continue`, `/clear`).
   Recommended: the proxy also sends `X-Clerk-Claude-Pid` (its parent PID).
   Hooks send `CLAUDE_PID` alongside `session_id`, and the daemon resolves a
   hook to the live proxy session with that PID before falling back to
   `session_id`.
   - Add a SessionStart hook (`clerk hook session-start`) that, for
     `source: "resume"`, aliases the proxy's start-up ID to the resumed ID, so
     `--continue` takes over leases inside the grace.
   - SessionEnd with `reason` `clear` or `resume` must not end the session
     while its proxy is connected: the Claude Code process lives on.
5. **A dropped call is not cancelled.** After an idle abort the daemon's call
   keeps running. If it then hands notices to a result the client threw away,
   they are lost. Recommended: progress every 30 s, as planned. Optionally,
   delivery stays pending until the next `lease_wait` (or a hook) acknowledges
   it. At least, test that a disconnect mid-wait (Review Focus 5) and an
   abandoned result both leave the notices pending.
6. **SessionEnd hook budget:** the plugin's `clerk hook session-end` gets
   1.5 s in total, not the plan's 2 s. Its RPC must finish in about 1 s.
