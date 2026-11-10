# Le Clerk: Phase 3 (MCP, hooks, plugin and skills) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** First polish the CLI's human output (Task 0). Then agents use the clerk through MCP. The daemon serves the MCP tools
of spec §8.1 over Streamable HTTP on its Unix socket, `clerk mcp` bridges
Claude Code's stdio to it with the session's identity, session liveness ends
dead sessions' leases, hooks re-arm and surface notices, and a Claude Code
plugin ships the MCP registration, the hooks and two skills (leasing,
registering resources).

**Architecture:**
- **Daemon MCP endpoint (`src/mcp/`):** `rmcp`'s Streamable HTTP server mounted
  at `/mcp` on the existing axum router. Identity is bound once, at MCP
  `initialize`, from headers the proxy sets; tool arguments never carry
  identity. Each tool is a thin mapping onto an existing engine command, run as
  the bound `Caller` with view `caller`.
- **Proxy (`clerk mcp`, `src/cli/mcp.rs`):** a meaning-preserving JSON-RPC relay
  between stdio and the daemon's `/mcp`, plus one long-lived liveness stream.
  It never interprets tool calls, so progress notifications and Tasks pass
  through untouched.
- **Session liveness (`src/scheduler/session.rs`):** the daemon tracks liveness
  streams per session. When the last one closes, a reconnect grace starts; a
  new stream with the same session ID cancels it and takes over. Expiry ends
  the session's leases and requests through the existing holder-gone path.
- **Hooks (`clerk hook …`, `src/cli/hook.rs`):** Stop, PreToolUse and
  SessionEnd handlers. They read Claude Code's hook JSON from stdin, never
  start the daemon, finish within 2 s, and fail open.
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
  - The proxy sets `X-Clerk-Session` (`CLAUDE_CODE_SESSION_ID`, or
    `conn:<ULID>` when unset), `X-Clerk-Connection` (a ULID per proxy process)
    and `X-Clerk-Project` (the proxy's start working directory, absolute,
    percent-encoded).
  - The daemon binds them to the MCP session at `initialize`; later headers
    are ignored. Holder: `AgentSession { session_id }`; project from
    `ProjectResolver::resolve`.
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
- **Waiting:**
  - `lease_wait` returns one batch of the session's notices. With none
    pending it blocks, sending a progress notification every 30 s, for at most
    20 min, then returns an empty batch. Every result ends with "Call
    lease_wait again while you hold or wait for anything."
  - An `offered` notice delivered by `lease_wait` (or by a completed
    `lease_acquire` task) is claimed in the same call; the result carries the
    grant brief.
  - `lease_acquire` returns a grant, or a queued result whose next step is
    `lease_wait`. If Task 1 finds `rmcp`'s Tasks matching Claude Code's,
    `lease_acquire` and `lease_wait` declare optional task support; the
    blocking path stays fully usable on its own (spec §11 fallback).
- **Liveness:** a session is alive while at least one of its liveness streams
  is open. When the last closes, `reconnect_grace` (config, 60 s) starts. After
  a daemon restart every `AgentSession` holder gets the same grace from boot.
  Expiry ends leases with cause `holder_gone` and drops requests, as for CLI
  processes.
- **Hooks:**
  - `clerk hook stop|pre-tool-use|session-end` read one JSON document from
    stdin (Claude Code's documented hook input; this is the only stdin read
    besides `-f -` and TTY prompts).
  - Never start the daemon (probe only). Total budget 2 s. On any error,
    timeout or missing daemon: exit 0 with no output.
  - Stop: when the session holds a live lease or a queued request, has no open
    `lease_wait`, and `stop_hook_active` is false, output a block decision
    whose reason tells the agent to call `lease_wait`. Otherwise output
    nothing.
  - PreToolUse: inject the session's pending notices that no hook has shown
    yet as `additionalContext`, and mark them shown. They stay pending for
    `lease_wait`.
  - SessionEnd: end the session's liveness now (no grace).
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
   input. The hook must exit 0 within 2 s with no output. → Tests in Task 5.
3. **Stop-hook loop:** the reminder must never fire twice in a row
   (`stop_hook_active`), and never when a `lease_wait` is open. → Tests in
   Task 5.
4. **Agent identity spoofing through arguments:** a tool call with extra
   `holder`/`session`/`project` arguments is refused as an unknown field; two
   sessions never see each other's leases or notices. → Tests in Tasks 2 and 3.
5. **`lease_wait` outliving the client:** the proxy dies mid-wait; the daemon
   must drop the wait without losing undelivered notices. → Test in Task 3.

---

## BDD skeleton (the contract)

```
mcp::server (daemon /mcp)
    initialize
        On proxy headers with a session id
            - binds AgentSession(session id) and the resolved project
        On missing X-Clerk-Session
            - refuses initialize with an MCP error
        On later requests with different headers
            - keeps the identity bound at initialize
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

mcp::tools inventory (type_list, resource_*, unit_*)
    resource_create
        On no scope with a resolved project
            - creates in project scope
        On no project
            - creates in user scope
        On a similar resource in another project
            - result carries the similar_in_other_project warning and says to tell the user
    resource_list
        On tags
            - returns only visible resources carrying all of them
        On another project's resource
            - never returned
    resource_get / unit_*
        - map to the engine command with view caller; other projects read as not_found

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
            - returns them at once and marks them delivered
        On an offered notice
            - claims it in the same call and returns the grant brief
        On none pending
            - sends a progress notification every 30 s
            - returns an empty batch after 20 min
        On the proxy disconnecting mid-wait
            - the wait ends; undelivered notices stay pending
        - every result ends with the re-arm sentence
    lease_status
        - returns this session's leases, queue positions and pending notice count only
    lease_extend / lease_release / lease_confirm / lease_set_interruptible / lease_yield_refuse / lease_reclaim / lease_cancel / lease_claim
        - map 1:1 to the engine; the next step text matches the resulting state
    Tasks (only if Task 1 confirms compatibility)
        On a task-capable client calling lease_acquire on a held pool
            - returns a task handle at once; the task completes with the grant
        On a client without tasks
            - behaves exactly as the blocking path

proxy (clerk mcp)
    On start
        - opens the liveness stream with the identity headers
        - relays stdin JSON-RPC to /mcp and responses/notifications to stdout unchanged in meaning
    On CLAUDE_CODE_SESSION_ID unset
        - uses conn:<ULID> as the session id
    On stdin EOF
        - closes the liveness stream and exits 0
    On the daemon restarting
        - reconnects the liveness stream and keeps relaying (re-initialising the MCP session if the daemon lost it)
    On the daemon not running
        - starts it (ensure_running), as every clerk invocation does

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

cli::hook
    any hook
        On no daemon / daemon unresponsive / malformed input / slow daemon
            - exits 0 with no output within 2 s; never starts a daemon
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
        - ends the session's liveness now

plugin (integrations/claude-code)
    - manifest, .mcp.json, hooks.json and both SKILL.md files validate against the documented schemas (Task 1)
    - .mcp.json launches `clerk mcp` over stdio
    - hooks.json wires Stop, PreToolUse and SessionEnd to `clerk hook …` with a timeout of at most 5 s
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
src/cli/hook.rs         clerk hook stop|pre-tool-use|session-end
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
  formatting, tables via `comfy-table`)
- Modify: `Cargo.toml` (add `comfy-table`), `src/cli/{output,views,inventory,lease,live,admin,mod}.rs`
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
  `render::tags(&[String], &Style) -> String`, `render::table(...)`.
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
        - match the mock-ups in docs/cli-output.md (tables, columns, occupancy bar)
    status
        - matches the mock-up (header with bar, unit tree, queue line)
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
  - `Sessions::connect(session_id, connection) / disconnect(connection) / end_now(session_id)`
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
  (`session_brief`, `mark_notices_shown`, `session_end`), `src/store/notices.rs`
  (a `shown_by_hook` mark that does not affect delivery)
- Test: `tests/hook.rs`

**Interfaces:**
- Consumes: Task 1's documented hook input/output schemas; Task 3's open-wait
  tracking; Task 4's `Sessions::end_now`; `Client::probe` (never
  `ensure_running`).
- Produces:
  - RPC `session_brief { session_id }` →
    `{ leases: n, requests: n, open_waits: n, unshown: [notice…] }`;
    `mark_notices_shown { session_id, ids }`; `session_end { session_id }`.
    All three answer only about the session named in the hook input; they are
    not MCP tools.
  - `clerk hook stop|pre-tool-use|session-end` per Global Constraints.

- [ ] **Step 1:** Write the failing tests for the BDD subtree `cli::hook`,
  including Review Focus 2 (no daemon, wedged daemon via a socket that never
  answers, malformed JSON: exit 0, empty stdout, within 2 s, no daemon
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
