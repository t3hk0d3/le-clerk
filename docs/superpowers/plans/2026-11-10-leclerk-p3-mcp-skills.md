# Le Clerk: Phase 3 (agent interface: MCP, then CLI-only) Implementation Plan

> **2026-11-10 revisions:** Part 3 (Tasks 14–16) gives agents text output. MCP is dropped. Part 2 (Tasks 8–13, at the end) replaces Tasks 2–4's MCP code with the CLI, one skill and hooks; its Global Constraints override Part 1's where they differ.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** First polish the CLI's human output (Task 0). Then agents use the clerk through MCP. The daemon serves the MCP tools
of spec §8.1 over Streamable HTTP on its Unix socket, `clerk mcp` bridges
Claude Code's stdio to it with the session's identity, session liveness ends
dead sessions' leases, hooks re-arm and surface notices, and a Claude Code
plugin ships the MCP registration, the hooks and two skills (leasing,
registering resources).

**Architecture:**
- **Daemon MCP endpoint (`src/mcp/`):** `rmcp`'s Streamable HTTP server mounted
  at `/mcp` on the existing axum router. Claude Code speaks MCP 2026-07-28
  (no `initialize`, no MCP session), so identity comes from headers the proxy
  sets on every request; tool arguments never carry identity. Each tool is a
  thin mapping onto an existing engine command, run as the request's `Caller`
  with view `caller`.
- **Proxy (`clerk mcp`, `src/cli/mcp.rs`):** a meaning-preserving JSON-RPC relay
  between stdio and the daemon's `/mcp`, plus one long-lived liveness stream.
  It never interprets tool calls, so progress notifications and Tasks pass
  through untouched.
- **Session liveness (`src/scheduler/session.rs`):** the daemon tracks liveness
  streams per session. When the last one closes, a reconnect grace starts; a
  new stream with the same session ID cancels it and takes over. Expiry ends
  the session's leases and requests through the existing holder-gone path.
- **Hooks (`clerk hook …`, `src/cli/hook.rs`):** SessionStart, Stop,
  PreToolUse and SessionEnd handlers. They read Claude Code's hook JSON from
  stdin, resolve the session by `CLAUDE_PID`, never start the daemon, finish
  within their budget (2 s; SessionEnd 1.5 s), and fail open.
- **Plugin (`integrations/claude-code/`):** plugin manifest, `.mcp.json`,
  `hooks/hooks.json`, `skills/leasing/SKILL.md`,
  `skills/registering-resources/SKILL.md`, and a local marketplace manifest so
  the user can install it from the repo.

**Tech Stack:** as phases 1 and 2, plus `rmcp` (3.x, server +
streamable-http features) and `schemars` for tool input schemas, both pinned in
Task 1.

**Spec:** `docs/superpowers/specs/2026-11-07-leclerk-design.md` — §6 (notices),
§7.1–7.3 (roles, liveness, identity), §8.1–8.3 (MCP tools, notice content,
skills), §9 (integration package), §10 (success criteria), §11 (verify first).
**Prerequisite:** phase 2 is complete (main at `85719c8` or later).

**Styleguide:** `STYLEGUIDE.md` (formatting, clippy pedantic, `thiserror` in
library modules and `anyhow` only in `main.rs`/`cli/*`, snake_case error codes,
one `mod` per BDD module/method/scenario and one `#[test]` per effect,
Conventional Commits). Builds use
`CARGO_TARGET_DIR=/tmp/claude-1000/leclerk-target`.

## Global Constraints

- **Documented mechanisms only** (spec §6): MCP (tools, progress
  notifications, Tasks), Claude Code stdio MCP servers, `CLAUDE_CODE_SESSION_ID`,
  and documented hook input/output. Never the session inbox socket or forged
  envelopes.
- **Identity:**
  - The proxy sets, on **every** request, `X-Clerk-Session`
    (`CLAUDE_CODE_SESSION_ID`, or `conn:<ULID>` when unset),
    `X-Clerk-Connection` (a ULID per proxy process), `X-Clerk-Project` (the
    proxy's start working directory, absolute, percent-encoded) and
    `X-Clerk-Claude-Pid` (the proxy's parent PID, i.e. the Claude Code
    process).
  - Claude Code speaks MCP 2026-07-28: no `initialize`, no MCP session (spec
    Appendix B). The daemon builds the identity per request from these
    headers; a request without `X-Clerk-Session` is refused. Holder:
    `AgentSession { session_id }` (after resolving any SessionStart alias);
    project from `ProjectResolver::resolve`.
  - The daemon also serves clients that negotiate 2025-11-25 through
    `initialize` (rmcp's default client, used in tests).
  - No tool takes a holder, session or project argument.
- **Agent visibility:** every tool runs with view `caller`. MCP never exposes
  `view: all`, admin operations, `list_audit`, `list_promotions`,
  `approve_promotion`, `lease_holder`, `request_holder`, or another holder's
  notices. The daemon already refuses the first and the holder lookups for
  `AgentSession` callers.
- **Tools** (names fixed): `type_list`, `resource_create`, `resource_update`,
  `resource_delete`, `resource_list`, `resource_get`, `unit_add`,
  `unit_update`, `unit_remove`, `unit_retire`, `lease_acquire`,
  `lease_status`, `lease_extend`, `lease_release`, `lease_confirm`,
  `lease_set_interruptible`, `lease_yield_refuse`, `lease_reclaim`,
  `lease_cancel`, `lease_claim`, `lease_wait`.
  - Arguments use the engine's wire names (`description`, `usage`, `tags`,
    `add_tags`, `remove_tags`, `settings`, `for` as a duration string, …).
  - `resource_create` without `scope` uses the session's project, or user
    scope when no project resolves.
- **Results:** structured content (the engine's JSON) plus one short text
  block whose last sentence is the next step the clerk expects (e.g. "Call
  lease_confirm when the unit is clean."). Errors are MCP tool errors carrying
  the engine's `code`, `message` and `details`.
  - Final-review amendment: Claude Code shows the model only the structured
    content, so every result, task result and tool error also carries
    `next` (the text's last sentence) and `summary` (its other lines) there;
    `lease_wait` results and completed `lease_acquire` task results carry
    `ack` (the notice ids to pass to the next `lease_wait`).
- **Waiting:**
  - `lease_wait` returns one batch of the session's notices. With none
    pending it blocks, sending a progress notification every 30 s, for at most
    20 min, then returns an empty batch. Every result ends with "Call
    lease_wait again with ack=[…] while you hold or wait for anything."
  - **Acknowledged delivery** (user decision, Appendix B item 5): Claude Code
    never cancels a call it dropped, so returning a notice does not mark it
    delivered. `lease_wait` takes `ack: [notice id]` (the ids from the previous
    result); only acknowledged notices are marked delivered. Notices returned
    but not acknowledged are returned again by the next `lease_wait`.
    Notice ids in a result are listed in the text block as well as the
    structured content (its `notices` and its `ack`). An `ack` entry that is
    not a notice id is ignored and reported in the `summary`.
  - An `offered` notice delivered by `lease_wait` (or by a completed
    `lease_acquire` task) is claimed in the same call; the result carries the
    grant brief.
  - `lease_acquire` returns a grant, or a queued result whose next step is
    `lease_wait`.
  - **Tasks** (verified compatible, SEP-2663 `io.modelcontextprotocol/tasks`):
    `lease_acquire` (when queued) and `lease_wait` return a task only when the
    request's client capabilities declare the extension, with
    `pollIntervalMs` 5000. Otherwise the blocking path; it stays primary
    (subagents, `claude -p`, desktop and VS Code never get tasks). An open
    task counts as an open wait for the Stop hook.
- **Liveness:** a session is alive while at least one of its liveness streams
  is open. When the last closes, `reconnect_grace` (config, 60 s) starts. After
  a daemon restart every `AgentSession` holder gets the same grace from boot.
  Expiry ends leases with cause `holder_gone` and drops requests, as for CLI
  processes.
- **Hooks:**
  - `clerk hook session-start|stop|pre-tool-use|session-end` read one JSON
    document from stdin (Claude Code's documented hook input; this is the
    only stdin read besides `-f -` and TTY prompts).
  - **Agent detection (Task 8):** `clerk hook` works out which agent ran it
    from the environment: Claude Code when `CLAUDE_CODE_SESSION_ID` or
    `CLAUDE_PID` is set (non-empty). Claude Code is the only agent so far;
    with no agent detected the hook exits 0 with no output and never
    contacts the daemon. The hook names stay agent-neutral, so another
    agent's hooks can reuse `clerk hook <name>` later.
  - Never start the daemon (probe only). Budget: 2 s total for session-start,
    stop and pre-tool-use; 1.5 s total for session-end (Claude Code's limit),
    so its RPC must finish within 1 s. On any error, timeout or missing
    daemon: exit 0 with no output.
  - **Session resolution:** hooks send the hook input's `session_id` and
    `CLAUDE_PID` (the hook's Claude Code process). The daemon resolves the
    session as the live proxy session whose `X-Clerk-Claude-Pid` matches, and
    falls back to `session_id`.
  - SessionStart with `source: "resume"`: alias the proxy's start-up session
    id to the resumed id, so `--continue` takes over the resumed session's
    leases inside the grace.
  - **Link (Task 8):** SessionStart with any other source (`clear`, and also
    `startup`, `compact`, `fork`, where it is usually a no-op), and every
    `session_brief` (Stop, PreToolUse) after its pending resume, links the
    hook's `session_id` N to the live proxy session U of the same Claude PID
    when N is not already U (after aliases), N has no open liveness stream,
    N holds nothing (no live lease, queued request or pending notice) and N
    is not a pending resume of that PID. Linking persists the alias N → U
    (as a resume alias does) and audits `session_linked { from: N, to: U }`;
    nothing moves. Then the CLI run from Bash after `/clear` acts as U, and a
    later `--continue` naming N takes over U's leases through the chain.
    Leases held at `/clear` stay with the session (user decision: keep).
  - Stop: when the session holds a live lease or a queued request, has no open
    `lease_wait`, and `stop_hook_active` is false, output a block decision
    whose reason tells the agent to call `lease_wait`. Otherwise output
    nothing.
  - PreToolUse: inject the session's pending notices that no hook has shown
    yet as `additionalContext`, and mark them shown. They stay pending for
    `lease_wait`.
  - SessionEnd: end the session's liveness now (no grace), except when
    `reason` is `clear` or `resume` and the session's proxy is still
    connected (the Claude Code process lives on): then nothing.
- **Plugin:** installable from the repo with Claude Code's documented plugin
  commands; installs at user or project scope at the user's choice. `clerk`
  must be on `PATH`.
- **Out of scope:** `asyncRewake` (spec §6 layer 5, deferred until layers
  1–3 are proven), `clerk install claude-code`, scan or discovery tools.

## Review Focus

1. **A dead session keeps a board forever:** Claude Code is killed (SIGKILL,
   laptop sleep). After `reconnect_grace` its leases end and the next waiter
   gets the unit; a resumed session reconnecting inside the grace keeps them.
   → Tests in Task 4.
2. **A hook blocks the agent:** daemon down, wedged or slow; malformed hook
   input. The hook must exit 0 within its budget (2 s; SessionEnd 1.5 s) with
   no output. → Tests in Task 5.
3. **Stop-hook loop:** the reminder must never fire twice in a row
   (`stop_hook_active`), and never when a `lease_wait` is open. → Tests in
   Task 5.
4. **Agent identity spoofing through arguments:** a tool call with extra
   `holder`/`session`/`project` arguments is refused as an unknown field; two
   sessions never see each other's leases or notices. → Tests in Tasks 2 and 3.
5. **`lease_wait` outliving the client:** the proxy dies mid-wait, or Claude
   Code drops a call whose result already carried notices; no notice is
   lost — unacknowledged notices come back on the next `lease_wait`. → Tests
   in Task 3.
6. **`--continue` loses its boards:** the resumed session gets a new MCP
   session id; the SessionStart alias must keep its leases. → Test in
   Tasks 4–5.

---

## BDD skeleton (the contract)

```
mcp::server (daemon /mcp)
    any request
        On X-Clerk-* headers
            - acts as AgentSession(session id, after alias resolution) with the resolved project
        On missing X-Clerk-Session
            - refuses the request with an MCP error; nothing changes
    server/discover (2026-07-28)
        - answers with the server's tools capability and the tasks extension
    initialize (2025-11-25 clients)
        - succeeds; later requests still take identity from their own headers
    tools/list
        - lists exactly the tool names in Global Constraints, each with an input schema
        - lists no admin, audit, promotion-list or holder-lookup tool
    any tool
        On an unknown argument (e.g. holder, session, project)
            - returns a tool error; nothing changes
        On an engine error
            - returns a tool error carrying code, message and details
        On success
            - returns structured content plus a text block ending in the next step
        On any result (every tool, a tool error, a task result)
            - structured content carries next equal to the text's last sentence
            - structured content carries summary: the text's other lines

mcp::tools inventory (type_list, resource_*, unit_*)
    resource_create
        On no scope with a resolved project
            - creates in project scope
        On no project
            - creates in user scope
    resource_list
        On tags
            - returns only visible resources carrying all of them
        On another project's resource
            - never returned
    resource_get / unit_*
        - map to the engine command with view caller; other projects read as not_found
    unit_add
        On a similar unit in another project
            - result carries the similar_in_other_project warning and says to tell the user
            - its structured summary says to tell the user (clerk approve)
    unit_add / unit_update
        On an unknown key inside settings
            - returns a tool error; nothing changes

mcp::tools leasing
    lease_acquire
        On a free unit
            - returns the grant with descriptions, tags and rendered usage; next step names lease_release
        On a held pool
            - returns queued with position; next step is lease_wait
        On a resource with no units
            - tool error no_units
    lease_wait
        On pending notices
            - returns them at once, listing their ids in the text block
            - does not mark them delivered
        On ack with ids from the previous result
            - marks exactly those delivered; they are not returned again
        On a previous result never acknowledged
            - returns its notices again
        On any result
            - structured ack lists exactly its notice ids
        On a malformed ack id
            - ignores it and acknowledges the valid ones
            - reports it in the summary
        On an offered notice
            - claims it in the same call and returns the grant brief
        On none pending
            - sends a progress notification every 30 s
            - returns an empty batch after 20 min
        On the proxy disconnecting mid-wait
            - the wait ends; unacknowledged notices stay pending
        - every result ends with the re-arm sentence
    lease_status
        - returns this session's leases, queue positions and pending notice count only
    lease_extend / lease_release / lease_confirm / lease_set_interruptible / lease_yield_refuse / lease_reclaim / lease_cancel / lease_claim
        - map 1:1 to the engine; the next step text matches the resulting state
    Tasks
        On a request whose client capabilities declare io.modelcontextprotocol/tasks
            - lease_acquire on a held pool returns a task handle at once with pollIntervalMs 5000; the task completes with the grant
            - an open task counts as an open wait (session_brief)
        On a lease_acquire task completed by an offer
            - structured ack names the offered notice
            - passing that ack to lease_wait returns no second grant
        On a request without the capability
            - behaves exactly as the blocking path

proxy (clerk mcp)
    On start
        - opens the liveness stream with the identity headers (incl. X-Clerk-Claude-Pid)
        - relays stdin JSON-RPC to /mcp and responses/notifications to stdout unchanged in meaning
    On each message (2026-07-28 framing)
        - sets MCP-Protocol-Version, Mcp-Method and Mcp-Name from the message, plus the X-Clerk-* headers
        - relays JSON and SSE responses
        - sends requests concurrently; a slow lease_wait never blocks other calls
    On CLAUDE_CODE_SESSION_ID unset
        - uses conn:<ULID> as the session id
    On stdin EOF
        - closes the liveness stream and exits 0
    On the daemon restarting
        - reconnects the liveness stream and keeps relaying
    On the daemon not running
        - starts it (ensure_running), as every clerk invocation does
    On a daemon of another version or protocol (an older build still running)
        - answers each request with an error naming `clerk daemon restart`
        - relays nothing to it
        - opens no liveness stream while it stays
        - opens the liveness stream and relays once it changes

client (every clerk client: CLI, proxy, hooks)
    connect
        On a daemon of the same version without protocol 3
            - returns protocol_mismatch (code version_mismatch) naming `clerk daemon restart`
    GET /v1/version
        - reports version, protocol 3 and pid

scheduler::session
    connect(session, connection)
        On a new session
            - marks it alive
        On a session inside its grace
            - cancels the grace; the new connection takes over
    disconnect(connection)
        On the session's last open connection
            - starts reconnect_grace
        On another connection still open
            - nothing changes
    on grace expiry
        - ends the session's leases (holder_gone) and drops its requests; next waiters are dispatched
    on daemon start
        - every AgentSession holder with live leases or requests gets reconnect_grace from boot
    end_now(session) (SessionEnd)
        - ends the session's leases and requests without grace
    alias(from start-up id, to resumed id) (SessionStart resume)
        - leases and requests of the resumed id are now held by the live proxy session; its grace is cancelled
    link(claude_pid, session_id) (SessionStart other than resume; session_brief) (Task 8)
        On a new id (after /clear) with a live proxy session of that Claude PID
            - returns Linked; the id resolves to the proxy session from now on, across a daemon restart
            - audits session_linked {from, to}; no lease, request or notice moves
        On the id already standing for the proxy session (start-up, compact, already linked)
            - returns None; writes nothing
        On no live proxy session of that Claude PID, or no Claude PID
            - returns None; writes nothing
        On the id holding a live lease, queued request or pending notice
            - returns None; writes nothing (a resume's job)
        On the id being a pending resume of that Claude PID
            - returns None; the pending resume stands
        On the id having an open liveness stream of its own
            - returns None; writes nothing
    after /clear, a crash and --continue (Task 8)
        - the SessionStart resume naming the post-/clear id takes over the leases held at /clear
    resolve(claude_pid, session_id)
        On a live proxy session with that Claude PID
            - returns it
        On none
            - returns session_id
    transact(store, f) (alias, start_resume, end_now, expiry, boot, hooks)
        On the transaction failing after an alias
            - the resumed id still resolves to itself; its grace stands; a later alias still takes it over
        On the transaction committing
            - keeps the changes

engine
    any command of an aliased agent session (CLI from Bash after --continue, RPC, MCP)
        - acts on the live session's leases
        - a new lease is held by the live session
        - reads the live session's notices

cli (agent sessions, CLAUDE_CODE_SESSION_ID set)
    leases --mine / release after /clear (Task 8)
        - acts on the leases the session's MCP tools took before /clear
    acquire --wait / attach
        On an offer the session already claimed (e.g. through lease_wait)
            - exits 0 and prints the held lease
    wait
        - exits 2 naming the MCP lease_wait tool; takes no notice
    any command
        On git failing inside a git checkout
            - exits 1 with git_failed
        On git missing outside any git checkout
            - uses the working directory as the project

scope::ProjectResolver
    resolve
        On git failing (missing or erroring) inside a git repo
            - returns ScopeError::Git (code git_failed)
        On git failing outside any git repo
            - returns the canonical cwd
mcp::ProjectCache
    On git failing for a directory never resolved
        - refuses the request naming git
    On git failing for a directory resolved before
        - keeps its last project

cli::hook
    any hook
        On no agent detected (neither CLAUDE_CODE_SESSION_ID nor CLAUDE_PID set) (Task 8)
            - exits 0 with no output; never contacts the daemon
        On no daemon / daemon unresponsive / malformed input / slow daemon
            - exits 0 with no output within its budget (2 s; session-end 1.5 s); never starts a daemon
        - resolves the session by CLAUDE_PID first, then session_id
    session-start
        On source resume
            - aliases the proxy's start-up id to the resumed id
        On source resume with the daemon not answering yet (the proxy starting it)
            - retries the connection within its budget, never starting a daemon, then aliases
        On source clear with the proxy connected (Task 8)
            - outputs nothing; links the new id to the proxy session
        On any other source
            - outputs nothing; changes nothing beyond a link
    stop / pre-tool-use (Task 8)
        On a new id the session-start hook did not link (it gave up)
            - links it, then answers for the proxy session
    stop
        On a live lease or queued request, no open lease_wait, stop_hook_active false
            - outputs a block decision whose reason names lease_wait
        On an open lease_wait
            - outputs nothing
        On stop_hook_active true
            - outputs nothing
        On nothing held or queued
            - outputs nothing
    pre-tool-use
        On unshown pending notices
            - outputs them as additionalContext and marks them shown
            - they remain pending for lease_wait
        On none
            - outputs nothing
    session-end
        On reason clear or resume with the proxy still connected
            - changes nothing
        On any other reason
            - ends the session's liveness now

plugin (integrations/claude-code)
    - manifest, .mcp.json, hooks.json and both SKILL.md files validate against the documented schemas (Task 1)
    - .mcp.json launches `clerk mcp` over stdio
    - hooks.json wires SessionStart, Stop, PreToolUse and SessionEnd to `clerk hook …` with a timeout of at most 5 s
```

The two `SKILL.md` files are prose; their contract is the content list in
spec §8.3, checked by review, not by tests.

## File structure

```
src/mcp/mod.rs          rmcp server, /mcp mount, identity binding
src/mcp/tools.rs        tool table: names, input schemas, engine mapping, next-step text
src/mcp/wait.rs         lease_wait blocking/progress/auto-claim (and Tasks if adopted)
src/scheduler/session.rs  session liveness: connections, grace, takeover, end
src/daemon/server.rs    (modify) mount /mcp and /v1/session/live
src/cli/mcp.rs          clerk mcp proxy
src/cli/hook.rs         clerk hook session-start|stop|pre-tool-use|session-end
src/engine/command.rs   (modify) session_brief, mark_notices_shown, session_end RPCs
integrations/claude-code/.claude-plugin/plugin.json
integrations/claude-code/.mcp.json
integrations/claude-code/hooks/hooks.json
integrations/claude-code/skills/leasing/SKILL.md
integrations/claude-code/skills/registering-resources/SKILL.md
.claude-plugin/marketplace.json   local marketplace pointing at integrations/claude-code
tests/mcp.rs, tests/session.rs, tests/hook.rs, tests/plugin.rs
docs/claude-code.md     install and manual verification checklist
```

---

### Task 0: CLI human output and smoke-test fixes

Before MCP: the fixes from the 2026-11-10 CLI smoke test, and the human output
format agreed with the user in `docs/cli-output.md` (the reference; its rules
and mock-ups are the contract for this task).

**Files:**
- Create: `src/cli/render.rs` (shared styling, glyphs, holder/project/time
  formatting, block lists; no tables)
- Modify: `src/cli/{output,views,inventory,lease,live,admin,mod}.rs`
- Modify (daemon, only if needed): the queued `acquire` result gains a
  `blocked_by` field describing what blocks the request (unhealthy units with
  their check message, held units with the earliest-ending holder and purpose,
  or a linked unit busy); `not_found` errors gain details naming the missing
  thing (`{"kind": "resource", "id": "nope"}`)
- Test: `tests/cli.rs`, `tests/cli_tty.rs`, unit tests in `src/cli/render.rs`

**Interfaces:**
- `render::Style { color: bool, unicode: bool }` from `Style::detect(json: bool, ascii_flag: bool)`:
  colour only on a TTY without `NO_COLOR`; Unicode unless `--ascii` or a
  non-UTF-8 locale (`LC_ALL`/`LC_CTYPE`/`LANG`).
- `render::holder(&Value) -> String`, `render::project(&ProjectKey) -> String`
  (repo folder, `~` for home), `render::when(ms) -> String`,
  `render::tags(&[String], &Style) -> String`.
- Global flag `--ascii`; `clerk log --wide`.

**BDD skeleton:**
```
cli human output (stdout is a TTY)
    any list or detail view
        - never shortens resource, unit, lease, request or promotion IDs, or settings
        - wraps free text instead of truncating it
        - shows projects as the repo folder with ~ for home, never .../.git
        - shows holders as cli / pid N / agent <8 chars> / agent in another project
        - omits empty fields
    style
        On a TTY without NO_COLOR
            - tags are bold cyan, resource IDs bold, ⚠ lines yellow; nothing else coloured
        On NO_COLOR set
            - no escape sequences
        On --ascii or a non-UTF-8 locale
            - no non-ASCII characters
    resources / leases / queue / log / promotions
        - match the mock-ups in docs/cli-output.md (block lists, occupancy bar; no tables)
    status
        - matches the mock-up (header with bar, indented units, queue line)
    resource show / grant / queued
        - match the mock-ups; usage indented under its label
    queued acquire
        On every free unit unhealthy
            - names the units and their check message and says they are re-checked
        On units held
            - names the holder and purpose of the one ending first
    errors
        On not_found
            - names the missing kind and id
        On a clap usage error in JSON mode
            - message carries no "try '--help'" tail
cli JSON output
    - unchanged except additive fields (blocked_by, not_found details)
```

- [ ] **Step 1:** Write the failing tests (TTY views under `script`; style and
  ASCII checks on raw bytes; the queued-reason and not_found cases).
- [ ] **Step 2:** Run them; expected FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run fmt, clippy and the full suite; expected PASS. Re-run
  the smoke scenarios by hand and compare against `docs/cli-output.md`.
- [ ] **Step 5:** Commit: `feat(cli): human output format, tags styling, smoke-test fixes`

### Task 1: Verify first (spec §11) and pin dependencies

**Files:**
- Modify: `Cargo.toml` (add `rmcp` and `schemars` at the versions this task
  pins)
- Modify: `docs/superpowers/specs/2026-11-07-leclerk-design.md` (new
  Appendix B: findings)
- Create: `docs/superpowers/spikes/2026-11-10-p3/` (throwaway spike code and
  logs; not built by the crate)

**Interfaces:** Produces findings later tasks consume:
- `rmcp` version; whether its server Tasks implementation matches what Claude
  Code 2.1.321 speaks (SEP-1686 vs the `io.modelcontextprotocol/tasks`
  extension), and whether Claude Code needs a flag for tasks;
- whether `rmcp`'s Streamable HTTP server mounts inside the existing axum
  router served over the Unix socket, and how its session header maps to our
  identity binding;
- `CLAUDE_CODE_SESSION_ID` after `--continue`/`--resume` and in subagents
  (§11.3), and whether hook input's `session_id` equals it;
- the current documented schemas for the plugin manifest, marketplace
  manifest, `.mcp.json`, `hooks.json`, hook input/output (Stop block decision,
  PreToolUse `additionalContext`, SessionEnd);
- §11.2 (progress keeps a long call alive past the idle limit) and §11.5
  (completed task while mid-turn) if they can be observed with a throwaway
  stdio server under `claude -p`; otherwise mark them for Task 7's manual
  checklist.

- [ ] **Step 1:** Read the `rmcp` 3.x docs and source for Tasks and
  Streamable HTTP; read Claude Code's plugin, hooks and MCP docs. Record
  sources (URLs, versions) in Appendix B.
- [ ] **Step 2:** Spike in `docs/superpowers/spikes/2026-11-10-p3/`: a minimal
  rmcp server on a Unix socket behind axum with one slow tool, and a stdio
  relay; run it under `claude -p` with `--mcp-config`. Log each §11 item's
  outcome.
- [ ] **Step 3:** Write Appendix B: one line per item — verified / failed
  (with the decided fallback from §11) / deferred to Task 7. If Tasks are
  incompatible, record "blocking `lease_wait` only for the MVP".
- [ ] **Step 4:** Pin `rmcp` and `schemars` in `Cargo.toml`; `cargo build`
  passes.
- [ ] **Step 5:** Commit: `docs(spec): phase 3 verify-first findings` (spike
  directory included; it is excluded from the crate).

### Task 2: Daemon MCP endpoint and inventory tools

**Files:**
- Create: `src/mcp/mod.rs`, `src/mcp/tools.rs`
- Modify: `src/daemon/server.rs` (mount `/mcp`), `src/lib.rs`
- Test: `tests/mcp.rs`

**Interfaces:**
- Consumes: `EngineHandle` and the engine commands in `src/engine/command.rs`;
  `ProjectResolver::resolve`; Task 1's findings on mounting.
- Produces:
  - `mcp::router(engine: EngineHandle) -> axum::Router` mounted at `/mcp`;
  - `struct McpIdentity { session_id: String, connection: Ulid, project: Option<ProjectKey> }`
    bound per MCP session, and `McpIdentity::caller(&self) -> Caller`;
  - a tool table entry per tool: name, input schema, engine method, and a
    `next_step(&Value) -> String` per result state;
  - inventory tools of Global Constraints with their default-scope rule.
- Tests drive the endpoint with an `rmcp` client over the Unix socket (no
  proxy yet), sending identity headers directly.

- [ ] **Step 1:** Write the failing tests for the BDD subtrees `mcp::server`
  and `mcp::tools inventory`, including Review Focus 4 (unknown identity
  arguments refused; two sessions isolated).
- [ ] **Step 2:** Run `cargo test --test mcp`; expected FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run fmt, clippy and the full suite; expected PASS.
- [ ] **Step 5:** Commit: `feat(mcp): daemon endpoint with identity binding and inventory tools`

### Task 3: Leasing tools and `lease_wait`

**Files:**
- Create: `src/mcp/wait.rs`
- Modify: `src/mcp/tools.rs`, `src/engine/command.rs` (if an engine helper
  is needed for "open wait" tracking — see below)
- Test: `tests/mcp.rs` (new modules)

**Interfaces:**
- Consumes: Task 2's tool table and identity; engine `acquire`, `claim`,
  `cancel`, `extend`, `release`, `confirm`, `set_interruptible`,
  `refuse_yield`, `reclaim`, `get_lease`, `list_my_leases`; notice waits
  (`wait_notices`, unfiltered for the session: MCP owns the session's
  notices).
- Produces:
  - all leasing tools of Global Constraints;
  - `lease_wait` per Global Constraints (30 s progress, 20 min cap,
    auto-claim of offers, re-arm sentence);
  - open-wait tracking the Stop hook reads: the engine records, per
    `AgentSession`, how many `lease_wait` calls (or tasks) are open, exposed
    through Task 5's `session_brief` RPC;
  - Tasks support for `lease_acquire`/`lease_wait` only if Task 1 verified it.
- Durations in tests are shortened through config (progress interval, wait
  cap), never by sleeping 20 minutes.

- [ ] **Step 1:** Write the failing tests for the BDD subtree
  `mcp::tools leasing`, including Review Focus 5 (client drops mid-wait;
  notices stay pending) and the auto-claim of an offer.
- [ ] **Step 2:** Run them; expected FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run fmt, clippy and the full suite; expected PASS.
- [ ] **Step 5:** Commit: `feat(mcp): leasing tools and lease_wait`

### Task 4: Session liveness and the `clerk mcp` proxy

**Files:**
- Create: `src/scheduler/session.rs`, `src/cli/mcp.rs`
- Modify: `src/daemon/server.rs` (`GET /v1/session/live`), `src/cli/mod.rs`
  (subcommand `mcp`), `src/scheduler/liveness.rs` (reuse the holder-gone
  ending for sessions), engine timer wiring
- Test: `tests/session.rs`, `tests/mcp.rs` (proxy modules)

**Interfaces:**
- Consumes: Task 2's identity headers; `Config.reconnect_grace`; the
  existing holder-gone ending in `liveness.rs`; `Client::ensure_running`.
- Produces:
  - `Sessions::connect(session_id, connection, claude_pid) / disconnect(connection) / end_now(session_id) / alias(from, to) / resolve(claude_pid, session_id)`
    and grace expiry driven by the engine's clock and timers;
  - `GET /v1/session/live` with the identity headers: holds the response open
    (periodic keep-alive chunks) for as long as the proxy lives;
  - `clerk mcp`: the relay of Global Constraints. Line-delimited JSON-RPC on
    stdin/stdout; POSTs to `/mcp` with the identity headers; streams SSE
    responses and server notifications back; reconnects after a daemon
    restart.
- **Grace algorithm (pseudocode):**

  ```
  on connect(s, c):   open[s] += {c}; cancel grace[s]
  on disconnect(c):   s = owner(c); open[s] -= {c}
                      if open[s] empty: grace[s] = now + reconnect_grace
  on tick(now):       for s with grace[s] <= now: end_holder(AgentSession(s)); clear s
  on daemon start:    for each AgentSession with live leases or requests:
                          grace[s] = boot + reconnect_grace
  ```

- [ ] **Step 1:** Write the failing tests for the BDD subtrees
  `scheduler::session` and `proxy`, including Review Focus 1 (SIGKILLed
  proxy: leases end after grace and the waiter is dispatched; reconnect
  inside grace keeps them). Proxy tests spawn the real binary with
  `CLAUDE_CODE_SESSION_ID` set and speak MCP over its stdio.
- [ ] **Step 2:** Run them; expected FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run fmt, clippy and the full suite; expected PASS.
- [ ] **Step 5:** Commit: `feat(mcp): session liveness and stdio proxy`

### Task 5: Hooks

**Files:**
- Create: `src/cli/hook.rs`
- Modify: `src/cli/mod.rs` (subcommand `hook`), `src/engine/command.rs`
  (`session_brief`, `mark_notices_shown`, `session_start`, `session_end`), `src/store/notices.rs`
  (a `shown_by_hook` mark that does not affect delivery)
- Test: `tests/hook.rs`

**Interfaces:**
- Consumes: Task 1's documented hook input/output schemas; Task 3's open-wait
  tracking; Task 4's `Sessions::end_now`; `Client::probe` (never
  `ensure_running`).
- Produces:
  - RPCs, each taking `{ session_id, claude_pid }` from the hook and resolving
    the session per Global Constraints: `session_brief` →
    `{ leases: n, requests: n, open_waits: n, unshown: [notice…] }` (open
    waits include open tasks); `mark_notices_shown { ids }`;
    `session_start { source }` (alias on resume); `session_end { reason }`.
    They answer only about the resolved session; they are not MCP tools.
  - `clerk hook session-start|stop|pre-tool-use|session-end` per Global Constraints.

- [ ] **Step 1:** Write the failing tests for the BDD subtree `cli::hook`,
  including Review Focus 2 (no daemon, wedged daemon via a socket that never
  answers, malformed JSON: exit 0, empty stdout, within the budget, no daemon
  started) and Review Focus 3 (no double reminder).
- [ ] **Step 2:** Run them; expected FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run fmt, clippy and the full suite; expected PASS.
- [ ] **Step 5:** Commit: `feat(cli): Claude Code hooks for re-arming and notices`

### Task 6: Plugin and skills

**Files:**
- Create: everything under `integrations/claude-code/` and
  `.claude-plugin/marketplace.json` (see File structure)
- Test: `tests/plugin.rs`

**Interfaces:**
- Consumes: Task 1's documented schemas; the tool names in Global
  Constraints; `clerk hook …` and `clerk mcp` from Tasks 4–5.
- Produces:
  - **`skills/leasing/SKILL.md`:** frontmatter `name: leasing` and a
    description that triggers when an agent needs a board, port, service or
    other shared resource. Body: the leasing bullets of spec §8.3, written for
    the tool names above.
  - **`skills/registering-resources/SKILL.md`:** frontmatter
    `name: registering-resources` and a description that triggers both when
    the user asks to register things and when an agent needs something not
    registered. Body: the seven steps of spec §8.3, generic across types, with
    one worked example each for a device pool, a listen-port service and a
    generic pool. No scanning tool is mentioned or assumed.
  - `tests/plugin.rs`: validates every JSON file against the Task 1 schemas
    (required keys and types), that `.mcp.json` runs `clerk mcp`, that
    `hooks.json` wires the three events to `clerk hook …` with timeouts ≤ 5 s,
    and that each `SKILL.md` has valid frontmatter (`name`, `description`) and
    mentions only tool names from Global Constraints.

- [ ] **Step 1:** Write the failing `tests/plugin.rs` for the BDD subtree
  `plugin`.
- [ ] **Step 2:** Run it; expected FAIL.
- [ ] **Step 3:** Write the files.
- [ ] **Step 4:** Run fmt, clippy and the full suite; expected PASS.
- [ ] **Step 5:** Commit: `feat(plugin): Claude Code plugin with leasing and registration skills`

### Task 7: Install docs and end-to-end verification with Claude Code

**Files:**
- Create: `docs/claude-code.md`
- Modify: spec Appendix B (results of the manual items)

**Interfaces:**
- Consumes: everything above.
- Produces: `docs/claude-code.md` with install steps (build, put `clerk` on
  `PATH`, add the local marketplace, install the plugin at user or project
  scope) and a manual checklist of spec §10 and the deferred §11 items:
  1. two `claude` sessions on one board pool: the second queues, then is woken
     by its `lease_wait` result when the first releases;
  2. an idle session is woken by an expiry warning;
  3. killing a session frees its board after the grace;
  4. `--resume` keeps the session's leases;
  5. the Stop hook reminder appears once when a lease is held without a wait;
  6. a session asked to "register the boards on my hub" follows the
     registration skill (search first, descriptions, usage, tags, scope);
  7. §11.2 and §11.5 if Task 1 deferred them.

- [ ] **Step 1:** Write `docs/claude-code.md`.
- [ ] **Step 2:** Run the checklist with real `claude` sessions where this
  environment allows (`claude -p`, tmux for interactive sessions); record each
  result in Appendix B. Items that need the user's hardware or interaction are
  listed for the user, not faked.
- [ ] **Step 3:** Commit: `docs: Claude Code install and verification checklist`

## Part 2 (2026-11-10): CLI-only agent interface, no MCP

The user dropped MCP after the CLI-only spike (spec §6, Appendix C): agents use
the `clerk` CLI from Bash, one `clerk` skill and the plugin's hooks. Tasks 2–4's
MCP code is removed (Task 9) and the old Task 8 (`/clear` link) is superseded:
with the session ID as holder and the process only as its host, `/clear`
releases through SessionEnd (user decision) and needs no link. Tasks 0–7
stay as the record of what was built. Spec sections: §6, §7.1, §7.3, §8.1, §8.3,
§9, §11, Appendix C.

**Styleguide:** `STYLEGUIDE.md`, unchanged, for every task below.

### Part 2 Global Constraints

- **No MCP anywhere:** no `src/mcp`, no `clerk mcp`, no `rmcp`, `schemars` or
  MCP-only crates, no `/mcp` route, no `.mcp.json`. Protocol bumps to 4.
- **Agent detection and holder** (user decision: the session ID as is): a
  CLI or hook caller is an agent when `CLAUDE_CODE_SESSION_ID` is set and
  non-empty. The holder stays `HolderRef::AgentSession { session_id }`. Every
  agent call also carries `host: { pid, start_time }` from `CLAUDE_PID`, with
  `start_time` read from `/proc/<pid>/stat` (as for `CliProcess`).
  - No `CLAUDE_PID`, or one whose start time cannot be read: commands that
    would hold or queue for the agent fail with code `agent_unidentified`,
    naming Claude Code 2.1.214 as the minimum; read-only commands still run
    as the agent.
  - `clerk hook` with neither variable set: exit 0, no output, no daemon
    contact.
- **Hosts:** the daemon records each session's host (`agent_hosts`:
  session_id, pid, start_time, last_seen), replacing `session_aliases`
  (dropped by a migration). A session has a set of hosts. Hooks report
  (session, host) and move the process: every other session it ran loses
  it. Every other agent call and notice wait only touches: it adds the host
  unless that process runs another session now, and never moves it (final
  review). SessionEnd removes its host and ends the session when none is
  left (below).
- **Liveness:** a session is alive while it has a host whose process runs.
  When it stops being alive, `reconnect_grace` (60 s) starts; a report of the
  same session from any process within it re-hosts the session and cancels
  the grace (this is all `--continue`/`--resume`/`/resume` need). At the end
  of the grace its leases end with cause `holder_gone` and its requests drop
  (§5.7). After a daemon start every session with live leases or requests
  gets the grace from boot (sessions from the MCP era have no host and end
  then).
- **SessionEnd releases at once** (user decision): `session_end { reason }`
  from the session's current host ends the session with no grace, whatever
  the reason: leases end with cause `session_ended` (cleanup not confirmed,
  §5.4), requests drop, audited `session_ended { reason }`. From a process
  that is not the current host: nothing changes. Crashes run no hook, so the
  grace covers them; without hooks, `/clear` is noticed by the next report
  and released after the grace.
- **`clerk wait` for agents:**
  - returns one batch: blocks until the agent has pending notices, claims any
    `offered` lease in the batch (the result carries the grant brief), prints
    the batch and exits 0;
  - notices are marked delivered only after the batch is written and flushed
    (the CLI acks with the existing ack mechanism); a wait killed before that
    leaves them pending;
  - exits 0 with an empty batch, `summary` saying why, when its session is no
    longer alive (host gone or unhosted; checked at least every 2 s);
  - one armed wait per agent: while one is open, another exits 0 at once with
    `summary` "a clerk wait is already armed for this agent" and `next` none;
  - **armed** means a wait long-poll from the holder is open (the CLI
    re-polls with gaps under 2 s); the daemon tracks it in memory;
  - no time limit of its own; `--timeout <duration>` ends it with an empty
    batch (the §11 fallback).
  - **`--follow`** (Monitor, spec §6 layer 1): prints one JSON line per
    notice as it arrives (claiming offers first), each acknowledged after
    its line is flushed, and runs until the session is no longer alive or
    SIGTERM; never a batch. The armed rule and the one-armed-wait rule
    apply to it too.
  - SIGTERM/SIGINT (a background time limit, Appendix C item 8): exits 0
    after printing an empty batch whose `next` says to start it again.
- **Agent results:** run by an agent, every command prints JSON (no TTY
  view, no prompts) with `summary` and `next`; errors carry `code`,
  `message`, `details` and end their message with the next step. The
  next-step texts move from `src/mcp/tools.rs` into `src/cli/next.rs` and name
  CLI commands. Every wait result's `next` tells the agent to start
  `clerk wait` again in the background while it holds or waits for anything.
- **Agent commands** (names fixed for this plan): `clerk types`,
  `clerk status --mine`, `clerk confirm <lease> clean|yielded`,
  `clerk interruptible <lease> on|off`, `clerk yield <lease> --refuse`,
  `clerk reclaim <lease>`, `clerk claim <lease>`, `clerk release <lease>
  [--cleanup]`, plus the existing inventory, `acquire`, `extend`, `cancel`,
  `wait`. `acquire` by an agent never blocks (`--wait` refused for agents,
  pointing to `clerk wait`); queued exit code stays 6.
- **Hooks** (budgets unchanged: 2 s; SessionEnd 1.5 s; fail open):
  - session-start: reports the host; for `clear`, `compact` and `resume`,
    when the session holds or waits for anything, or (for `clear`) the
    previous session of the same process released something, prints a
    brief as `additionalContext`: its leases and requests (IDs, resource,
    unit, ends), whether a wait is armed, what the previous session of the
    same process released (for `clear`), "use the clerk skill", and,
    when no wait is armed but something is held, the exact `clerk wait`
    command to run in the background. Nothing for `startup` or `fork`, or
    when there is nothing to say.
  - stop: reminder (block decision) when the agent holds or queues anything,
    no wait is armed and `stop_hook_active` is false; the reason names the
    background `clerk wait` command.
  - pre-tool-use: unchanged (pending notices not yet shown by a hook).
  - session-end: `session_end { reason }` (above); 1 s RPC budget.
- **Plugin:** `integrations/claude-code/` holds `.claude-plugin/plugin.json`,
  `hooks/hooks.json` and `skills/clerk/{SKILL.md,leasing.md,registering.md}`;
  the `leasing` and `registering-resources` skills and `.mcp.json` are
  deleted. `SKILL.md` stays under 120 lines; its description covers leasing,
  notices and registering in one paragraph.

### Part 2 Review Focus

1. **A killed Claude Code keeps its boards:** `kill -9` of the agent's process
   (its background `clerk wait` keeps running, Appendix C). The wait must
   exit and the leases end after the grace. → Tasks 10, 11.
2. **Boards kept or lost at the wrong time:** `/clear` and quitting release
   at once; a crash keeps the leases for the grace, so `--continue` after a
   crash keeps them; a second process on the same session is not released
   by the first one's SessionEnd. → Tasks 10, 12.
3. **Notices lost between two waits:** a wait killed mid-print, two waits
   racing, a wait woken by an offer it fails to claim. Nothing is lost or
   delivered twice. → Task 11.
4. **PID reuse:** a new process with the old `CLAUDE_PID` but another start
   time never counts as the host or unhosts anything. → Task 10.
5. **A hook or wait blocks a non-agent shell:** the plugin installed while
   running `clerk` outside Claude Code; `clerk wait` run by a person. People
   keep today's behaviour. → Tasks 11, 12.

### Part 2 BDD skeleton

```
cli::caller
    current
        On CLAUDE_CODE_SESSION_ID and CLAUDE_PID with a readable start time
            - holder is AgentSession(session id); host is { pid, start_time }
        On CLAUDE_CODE_SESSION_ID without CLAUDE_PID, or its process gone
            - holding/queueing commands fail with agent_unidentified naming 2.1.214
            - read-only commands run as the agent
        On neither variable
            - unchanged (CliProcess / User per kind)

engine host reports (every agent call and hook)
    On a session's first report
        - records its host
    On session N reported by a process that hosted U
        - U becomes unhosted; its grace starts; its leases stay held during it
    On U reported again within its grace (any process)
        - U is re-hosted by that process; the grace is cancelled; nothing ends
    On a report from a reused pid with another start time
        - counts as a different process; unhosts nothing of the old one

scheduler::liveness (AgentSession holders)
    sweep
        On the host process running
            - session stays alive
        On the host process gone, or the session unhosted
            - grace starts
        On the grace ending
            - leases end holder_gone, units go through §5.4, requests drop
    daemon start
        On sessions with live leases or requests
            - grace from boot; a session re-reported within it keeps its leases
        On a session with no recorded host (MCP era)
            - ends after the grace

cli wait (agent)
    On pending notices
        - prints one JSON batch with summary and next, exits 0
        - marks them delivered after the flush
    On an offered lease in the batch
        - claims it; the batch carries the grant brief
        On the claim failing (offer expired)
            - the batch says so; next says to run clerk wait again
    On no notices yet
        - blocks; the agent counts as armed meanwhile
    On the Claude Code process exiting
        - exits 0 with an empty batch within 2 s
    On another wait already armed for the agent
        - exits 0 at once; summary says one is armed; nothing consumed
    On being killed before the flush
        - its notices are returned by the next wait
    On --timeout elapsing
        - exits 0 with an empty batch whose next says to start it again
cli wait (person)
    - unchanged

cli agent commands (run with CLAUDE_PID)
    any command
        - prints JSON with summary and next; never prompts
        On an error
            - JSON error with code, message, details; message ends with the next step
    acquire
        On a free unit
            - prints the grant (descriptions, rendered usage, tags), claimed; next names confirm/release
        On queued
            - exits 6 with position; next names the background clerk wait
        On --wait
            - refused; points to clerk wait
    status --mine
        - lists the agent's leases, requests, pending notices and armed: true|false
        - never lists another holder's
    confirm / interruptible / yield --refuse / reclaim / claim / release [--cleanup] / types
        - map to the engine command for the agent; results carry summary and next
    admin / log / promotions / --view all
        - refused as for agents today

cli::hook
    any event
        On neither CLAUDE_PID nor CLAUDE_CODE_SESSION_ID
            - exits 0, no output, no daemon contact
    session-start
        On clear | compact | resume while holding or queued
            - prints the brief; with no armed wait it includes the background clerk wait command
        On clear after the previous session released leases
            - the brief names what was released
        On startup | fork, or nothing to say
            - prints nothing
    stop
        On holding, no armed wait, stop_hook_active false
            - block decision naming the background clerk wait
        On a wait armed, or stop_hook_active true, or holding nothing
            - no output
    session-end
        On any reason from the session's current host
            - leases end at once with cause session_ended, cleanup not confirmed; requests drop
            - audits session_ended { reason }
        From a process that is not the current host
            - nothing changes
        On the daemon down or slow
            - exits 0 within 1.5 s; the grace covers it

plugin
    files
        - plugin.json and hooks.json valid; hooks run clerk hook <event> with timeouts ≤ 5 s
        - no .mcp.json; no leasing or registering-resources skill
    skills/clerk
        - SKILL.md frontmatter name clerk, one description; under 120 lines
        - links leasing.md and registering.md; mentions only commands in Part 2 Global Constraints
        - tells the agent to arm Monitor on clerk wait --follow (timeout_ms 1800000) and re-arm it on expiry
        - gives the background clerk wait (run_in_background, 2 h timeout, read the output file) as the fallback
```

### Task 8: Verify first for the CLI-only interface (spec §11)

**Files:**
- Modify: spec Appendix C (results), `docs/superpowers/spikes/2026-11-10-cli-only/`
  (scripts for the new checks, evidence)

**Interfaces:**
- Consumes: the spike helpers (`env.sh`, `tm.sh`, `run-claude.sh`,
  `waiter.sh`), isolated as before: a folder Claude Code already trusts,
  `--setting-sources project --strict-mcp-config`, a private tmux server; never
  the user's real config or clerk state.
- Produces: a result per spec §11 item 1–6 in Appendix C, and, for each that
  fails, the fallback applied to the later tasks (ledger ruling).

- [x] **Step 1:** §11.1: a background waiter released after 35 min wakes the
  idle agent: passed 2026-11-10 (Appendix C item 6).
- [x] **Step 2:** §11.2: release the waiter while the agent is mid-turn (a
  prompt that runs `sleep 60` in the foreground); record what the agent sees
  and when.
- [x] **Step 3:** §11.3: `claude -p` with `BASH_DEFAULT_TIMEOUT_MS` unset and a
  waiter started with `timeout: 60000`; record whether the kill wakes it.
- [x] **Step 4:** §11.4–11.5: `/clear` then `SessionEnd` input (a hook that
  logs its stdin); `kill -9` then `claude --continue`: the SessionStart hook
  input (`source`, `session_id`) and `CLAUDE_PID`.
- [x] **Step 5:** §11.6: list for the user (desktop app, VS Code); not faked.
- [ ] **Step 6:** Commit: `docs: CLI-only verify-first results`

### Task 9: Remove MCP

**Files:**
- Delete: `src/mcp/`, `src/cli/mcp.rs`, `tests/mcp.rs`, `tests/proxy.rs`,
  `integrations/claude-code/.mcp.json`
- Modify: `src/lib.rs`, `src/cli/mod.rs` (no `Mcp` command),
  `src/daemon/server.rs` (no `/mcp`, no `/v1/session/live`), `Cargo.toml`
  (drop `rmcp`, `schemars` and any crate only MCP used), `src/engine/wire.rs`
  (PROTOCOL 4), `src/cli/lease.rs` (drop the "use lease_wait" refusal)
- Test: the full suite; `tests/cli.rs` gains `clerk mcp` → usage error

**Interfaces:**
- Consumes: nothing new.
- Produces: a daemon and CLI with no MCP. `src/scheduler/session.rs` stays
  until Task 10 replaces it. CLI agents temporarily keep `AgentSession`
  identity (no liveness source: they end after the grace) until Task 10.

- [ ] **Step 1:** Delete and unwire; build.
- [ ] **Step 2:** `cargo tree | grep -i mcp` prints nothing.
- [ ] **Step 3:** Run fmt, clippy and the full suite; expected PASS (tests of
  removed code removed with it, nothing else changed).
- [ ] **Step 4:** Commit: `refactor!: remove the MCP server and clerk mcp`

### Task 10: Session hosts and liveness

**Files:**
- Modify: `src/cli/caller.rs` (host from `CLAUDE_PID`, `agent_unidentified`),
  `src/model/types.rs` (`Caller.host: Option<Host>`, `Host { pid: u32,
  start_time: u64 }`), `src/scheduler/session.rs` (replaced by
  `src/scheduler/host.rs`: host reports, unhosting, grace),
  `src/scheduler/liveness.rs` (sweep `AgentSession` hosts like
  `CliProcess`), `src/store/` (migration: `agent_hosts`, drop
  `session_aliases`), `src/engine/command.rs` (every agent call and hook
  reports its host; `session_start`/`session_end` RPCs)
- Test: `tests/liveness.rs`, `tests/host.rs` (new, replaces
  `tests/session.rs`), `tests/engine.rs`, unit tests in `caller.rs`

**Interfaces:**
- Consumes: `liveness::start_time_of(pid)` (add if only `self_start_time`
  exists), the holder-gone path.
- Produces: `model::Host`; `Caller.host`; `scheduler::host::Hosts::report(&mut
  self, tx: &Tx, session_id: &str, host: Host) -> Result<Reported,
  SchedulerError>` (`Reported { unhosted: Vec<String> }`, the sessions this
  report left); `Hosts::unhost(&mut self, session_id: &str)`; error code
  `agent_unidentified`.

- [ ] **Step 1:** Write the failing tests for `cli::caller`,
  `engine host reports` and `scheduler::liveness (AgentSession holders)`,
  including Review Focus 1, 2 and 4.
- [ ] **Step 2:** Run them; expected FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run fmt, clippy and the full suite; expected PASS.
- [ ] **Step 5:** Commit: `feat: agent sessions live while their Claude Code process hosts them`

### Task 11: Agent commands and `clerk wait`

**Files:**
- Create: `src/cli/next.rs` (summary and next-step texts for every agent
  result and error)
- Modify: `src/cli/{mod,lease,views,inventory,output}.rs`, the daemon's wait
  RPC (armed-wait tracking, one per holder)
- Test: `tests/cli.rs`, `tests/notices.rs`

**Interfaces:**
- Consumes: Task 10's host reports; the engine's existing confirm, set
  interruptible, refuse yield, reclaim, claim, cancel and ack commands.
- Produces: the commands in Part 2 Global Constraints; RPC `wait_notices`
  gains `armed` tracking and an `already_armed` answer; `status --mine`
  reports `armed`.

- [ ] **Step 1:** Write the failing tests for `cli wait (agent)`,
  `cli wait (person)` and `cli agent commands`, including Review Focus 3 and
  5 (a person's `clerk wait` unchanged).
- [ ] **Step 2:** Run them; expected FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run fmt, clippy and the full suite; expected PASS.
- [ ] **Step 5:** Commit: `feat(cli): agent commands and a one-batch clerk wait`

### Task 12: Hooks for the CLI-only interface

**Files:**
- Modify: `src/cli/hook.rs`, `src/engine/command.rs` (`session_brief` with
  `armed`; `session_start` brief), `integrations/claude-code/hooks/hooks.json`
  (description text)
- Test: `tests/hook.rs`

**Interfaces:**
- Consumes: Tasks 10–11 (host reports, armed tracking).
- Produces: the hook behaviour in Part 2 Global Constraints. Tests set
  `CLAUDE_CODE_SESSION_ID` and `CLAUDE_PID` (a live child process they own).

- [ ] **Step 1:** Write the failing tests for `cli::hook`, including Review
  Focus 5 (no agent variables: no daemon started, no output).
- [ ] **Step 2:** Run them; expected FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run fmt, clippy and the full suite; expected PASS.
- [ ] **Step 5:** Commit: `feat(hooks): briefs after /clear and resume, wait reminders`

### Task 13: The `clerk` skill, docs and end-to-end check

**Files:**
- Create: `integrations/claude-code/skills/clerk/{SKILL.md,leasing.md,registering.md}`
- Delete: `integrations/claude-code/skills/{leasing,registering-resources}/`
- Modify: `tests/plugin.rs`, `docs/claude-code.md`, spec Appendix C,
  `README` if it mentions MCP
- Test: `tests/plugin.rs`

**Interfaces:**
- Consumes: the commands, wait and hooks of Tasks 11–12; the content of the
  two old skills (rewritten for CLI commands, not copied).
- Produces: the plugin per Part 2 Global Constraints; `docs/claude-code.md`
  install steps without MCP and a checklist: two agents on one pool (the
  second woken by its background wait), an idle agent woken by an expiry
  warning, `/clear` and quitting release at once and the brief says what,
  `kill -9` frees the board after the grace, `--continue` keeps it, the Stop reminder fires once,
  "register the boards on my hub" follows `registering.md`.

- [ ] **Step 1:** Write the failing `tests/plugin.rs` for the BDD subtree
  `plugin`.
- [ ] **Step 2:** Run it; expected FAIL.
- [ ] **Step 3:** Write the skill files and docs.
- [ ] **Step 4:** Run fmt, clippy and the full suite; expected PASS.
- [ ] **Step 5:** Run the checklist with isolated real `claude` sessions
  (spike helpers, plugin loaded with `--plugin-dir`, a private `CLERK_HOME`);
  record results in Appendix C; list what needs the user.
- [ ] **Step 6:** Commit: `feat(plugin): one clerk skill for the CLI-only interface`


## Part 3 (2026-11-10): text output for agents

The user asked that agents get Markdown-like text instead of JSON, to save
tokens and read better. Agents read results, they don't parse them. The CLI
already has a person-facing view of every result (block lists, IDs never
shortened, usage indented; `docs/cli-output.md`). Agents get that view
without colour, plus the next step. `--json` stays for scripts and tests.
Spec sections: §8.1 (results), §8.3 (the skill), §8.4.

**Styleguide:** `STYLEGUIDE.md`, unchanged. The output format reference
stays `docs/cli-output.md`; Task 14 adds an "Agent sessions" section there.

### Part 3 Global Constraints

- **Agent text mode:** a command run in an agent session (`CLAUDE_CODE_SESSION_ID`
  set) without `--json` prints the person view of its result, with:
  - no ANSI escapes, whether or not stdout is a terminal;
  - no wrapping (width unlimited), since agents gain nothing from it;
  - Unicode glyphs as for people (`--ascii` still works);
  - for an object result, a last line `Next: <next>`, where `<next>` is the
    text `src/cli/next.rs` gives today. The `summary` lines are not printed
    (the view already says what happened), except lines that tell the agent
    to tell the user something (`Tell the user …`, e.g. a promotion), which
    come before `Next:`;
  - for a list result (`resources`, `log`, …), the list view and no `Next:`.
- **`--json`** (global flag) prints exactly today's JSON, `summary` and
  `next` included. Unchanged for people and scripts.
- **Errors in agent text mode:** one line on **stdout** (Monitor streams
  stdout only): `error <code>: <message>`, the message ending with the next
  step as today. Exit codes unchanged. People's errors stay on stderr.
- **Notices in text:** every notice renders as a block:
  `[<kind>] <resource>/<unit> · lease <id> | request <id> · until <time> (<in …>)`
  then the action, indented. Fields that are absent are left out. The
  claimed grant of an offer follows as the grant view.
- **`clerk wait` (agent, text):**
  - one batch: the notice blocks, blank-line separated, then `Next:`;
  - `--follow`: one notice block per event, flushed per notice, each ending
    with its own `Next:` line (a Monitor event carries one notice);
  - timeout, ended, armed, stopped: one sentence, then `Next:`.
- **Hooks are unchanged:** their output is Claude Code's hook JSON; the
  context they add is already text.
- **Token check:** for the standard test grant (a device unit with a
  three-line usage), the agent text is at most 60 % of the JSON's bytes.

### Part 3 Review Focus

1. **Something the agent needs is missing from the text:** a lease ID, a
   request ID, a deadline, the usage, a "tell the user" line. Every field
   the skill tells the agent to use must be in the text. → Tasks 14, 15.
2. **Output mode leaks:** colour codes in an agent's Bash output when it runs
   in a pseudo-terminal; JSON for a person at a terminal; text where a
   script asked for `--json`. → Task 14.
3. **Monitor sees nothing on error:** an error from `clerk wait --follow`
   written to stderr never reaches Monitor. → Tasks 14, 15.

### Part 3 BDD skeleton

```
cli output (agent session, no --json)
    any object result
        - prints the person view with no ANSI escapes and no wrapping
        - ends with "Next: " followed by next.rs's next step
        - prints no JSON
        On a "tell the user" summary line (promotion filed)
            - that line is printed before "Next:"
    any list result
        - prints the list view, no "Next:" line
    with --json
        - prints today's JSON with summary and next
    on an error
        - prints "error <code>: <message>" on stdout, the message ending with the next step
        - exit code unchanged
    acquire
        On a grant
            - shows lease id, resource/unit, end time, descriptions, settings, usage unescaped
            - is at most 60 % of the JSON's size for the test grant
        On queued
            - shows request id, position and what blocks it; "Next:" names the wait
cli output (person)
    - unchanged: human at a terminal, JSON otherwise, --json forces JSON

notice text
    - "[<kind>] <resource>/<unit> · lease <id> · until <time> (<in …>)", then the action indented
    - leaves absent fields out

cli wait (agent, text)
    one batch
        - one notice block per notice, then the claimed grants, then "Next:"
    --follow
        - one block per notice ending with its own "Next:", flushed per notice
    On timeout | ended | armed | stopped
        - one sentence, then "Next:"
    with --json
        - today's JSON lines

plugin (the clerk skill)
    - says results are text ending with a "Next:" line; follow it
    - never says results are JSON
```

### Task 14: Agent text mode

**Files:**
- Modify: `src/cli/output.rs` (`Mode::Agent`: person view, no colour, no
  wrap, `Next:` line; agent errors on stdout), `src/cli/mod.rs` (agent →
  `Mode::Agent` unless `--json`), `src/cli/render.rs` (width unlimited in
  agent mode), `src/cli/next.rs` (`tell_user` lines kept for text),
  `docs/cli-output.md` (an "Agent sessions" section with a mock-up of a
  grant and a queued result)
- Test: `tests/cli.rs` (new `mod agent_text`), and the test helper
  `Env::agent` passes `--json` so today's agent tests keep reading JSON

**Interfaces:**
- Consumes: `next::annotate`'s note (split into `summary`, `next`).
- Produces: `Mode::Agent`; `next::note(command, &Value) -> (Vec<String>, String)`
  made public to the output layer; `next::tell_user(&[String]) -> Vec<String>`.

- [ ] **Step 1:** Write the failing tests for `cli output (agent session, no
  --json)` and `cli output (person)`, including Review Focus 2 (run under
  `script` for a pseudo-terminal: no escapes) and the 60 % size check.
- [ ] **Step 2:** Run them; expected FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run fmt, clippy and the full suite; expected PASS.
- [ ] **Step 5:** Commit: `feat(cli): agents get text with a Next line; --json for scripts`

### Task 15: Notices and waits in text

**Files:**
- Modify: `src/cli/lease.rs` (`notice_text` with IDs and deadline),
  `src/cli/wait.rs` (text batch and `--follow` blocks; `--json` keeps
  today's lines)
- Test: `tests/cli.rs` (`mod wait` text cases)

**Interfaces:**
- Consumes: Task 14's `Mode::Agent`, `granted_text`.
- Produces: `notice_block(&Value) -> String` (notice JSON → text block).

- [ ] **Step 1:** Write the failing tests for `notice text` and `cli wait
  (agent, text)`, including Review Focus 3 (an error in `--follow` reaches
  stdout).
- [ ] **Step 2:** Run them; expected FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run fmt, clippy and the full suite; expected PASS.
- [ ] **Step 5:** Commit: `feat(cli): notices and clerk wait in text for agents`

### Task 16: Skill, spec and a live check

**Files:**
- Modify: `integrations/claude-code/skills/clerk/{SKILL.md,leasing.md}`
  (results are text ending with `Next:`; `--json` exists for scripts),
  `tests/plugin.rs` (the skill never says results are JSON), spec §8.1
  "Results" and §8.4, `docs/claude-code.md` ("What the model sees")
- Test: `tests/plugin.rs`

- [ ] **Step 1:** Write the failing `tests/plugin.rs` check.
- [ ] **Step 2:** Update the skill, spec and docs; run the suite; expected
  PASS.
- [ ] **Step 3:** Live check, isolated as before: one session leases, queues
  behind another and is woken by Monitor; read its transcript for any
  confusion about the text. Record it in spec Appendix C.
- [ ] **Step 4:** Commit: `docs: agents read text results`

---

## Self-review notes

- **Spec coverage:** §6 layers 1–4 (Tasks 3, 5; layer 5 out of scope), §7.1
  daemon MCP and proxy (Tasks 2, 4), §7.3 identity, liveness, reconnect,
  takeover, fallback (Tasks 2, 4), §8.1 tools (Tasks 2, 3), §8.2 notice
  content (Task 3 results carry the engine notices unchanged), §8.3 skills
  (Task 6), §9 plugin (Task 6), §10 and §11 (Tasks 1, 7).
- **Deferred:** `asyncRewake`; `clerk install claude-code`; phase 2 deferred
  minors (SIGKILL escalation in `clerk run`, `clerk wait` and lent leases,
  unit update read-modify-write, splitting `src/cli/lease.rs`).
- **Part 2 coverage (2026-11-10):** §6 layers 1–4 (Tasks 11, 12), §7.1 (Task 9), §7.3 holder, hosts, liveness, detection (Tasks 10, 12), §8.1 (Task 11), §8.3 one skill (Task 13), §9 (Tasks 9, 13), §11 (Task 8). Deferred: `asyncRewake`, `clerk install claude-code`.
- **Part 3 coverage (2026-11-10):** §8.1 results as text (Tasks 14, 15), §8.3 skill wording (Task 16), §8.4 `--json` (Task 14). Self-review: no implementation code; one BDD subtree per public behaviour; Review Focus lines each owned by a task.
