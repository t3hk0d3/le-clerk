# Le Clerk: Phase 1 (Core) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the core of Le Clerk: the library (inventory, dedup, health,
scheduler, notices, audit) and the resident daemon that serves it as a JSON RPC
API on a Unix socket. It includes the Rust client that starts the daemon
automatically.

**Architecture:**
- **One Rust package** (library plus a `clerk` binary).
- **The pure core** (model, dedup, health, scheduler) knows nothing about I/O
  except the `Store` and an injected `Clock`.
- **The daemon** owns one `Engine` actor: a single task that serialises every
  mutation through one SQLite connection, one transaction per decision. It
  serves a JSON RPC API over HTTP/1.1 on a Unix socket.
- **`client.rs`** (Task 13) is the thin client that starts the daemon
  automatically. Phase 2's CLI builds on it.

**Tech Stack:**
- Rust (edition 2024, stable toolchain 1.99 present locally)
- tokio
- axum (server on a `UnixListener`)
- hyper 1 + hyper-util (client over `UnixStream`)
- rusqlite (feature `bundled`) + rusqlite_migration
- serde / serde_json
- clap (derive)
- thiserror (library) / anyhow (binary only)
- tracing
- ulid
- crossterm (raw-mode keys)
- fs4 (file lock)
- Dev: assert_cmd, tempfile, predicates

**Spec:** `docs/superpowers/specs/2026-11-07-leclerk-design.md` (commit
`7cf9ae6`). Read it before starting. Section numbers below (§) refer to it.

**Phases** (one plan each):
1. **Core (this plan).**
2. **CLI:** `2026-11-07-leclerk-p2-cli.md`.
3. **MCP:** the MCP server on the daemon, the `clerk mcp` stdio proxy, MCP
   Tasks, `lease_wait`, session liveness through the proxy, and the
   verify-first spikes in §11. Planned after phase 2.
4. **Skills etc.:** the Claude Code plugin, hooks, skill, and the end-to-end
   tmux harness. Planned after phase 3.

**Deliverable of this phase:**
- `clerk daemon` (and `clerk daemon status|stop|restart`), plus `clerk
  --version`;
- every behaviour reachable through the RPC API and tested at library, engine
  and daemon level;
- the notice queue (`/v1/notices` long-poll), which phases 2 and 3 expose.

Output mode, exit codes and agent gating in Global Constraints are **contracts
for phase 2**. They are listed here so the API errors match them.

## Global Constraints

- **Binary name `clerk`, crate name `leclerk`.** Linux is the target platform.
  Code must not break the macOS build but is not tested there.
- **Documented mechanisms only.** Never write into Claude Code session sockets
  or rely on Claude Code internals (§6). In phases 1–2 the only Claude Code coupling is
  reading `CLAUDE_CODE_SESSION_ID` from the environment.
- **Paths:**
  - socket: `$XDG_RUNTIME_DIR/clerk/clerk.sock`, in a directory with mode 0700;
  - lock: `$XDG_RUNTIME_DIR/clerk/daemon.lock`;
  - database: `$XDG_STATE_HOME/clerk/clerk.db`
    (default `~/.local/state/clerk/clerk.db`);
  - daemon log: `$XDG_STATE_HOME/clerk/daemon.log`;
  - config: `$XDG_CONFIG_HOME/clerk/config.toml`;
  - if `CLERK_HOME` is set, all of these go under `$CLERK_HOME/{run,state,config}`
    instead. Tests always set it.
- **Default timings** (config keys in parentheses, values in seconds):
  - expiry warning lead: min(300 s, 20% of the declared duration)
    (`warning_lead_max` 300, `warning_lead_ratio` 0.2);
  - cleanup window: 120 s, with one extension of +120 s
    (`cleanup_window`, `cleanup_extension`);
  - claim window: 300 s (`claim_window`);
  - yield answer window: 120 s (`yield_window`);
  - health-check timeout: 10 s (`health_timeout`);
  - reconnection grace: 60 s (`reconnect_grace`). The value is reserved for phase 3 (MCP).
- **Timestamps** are unix milliseconds (`i64`) in storage and in the API. All
  time comes from the injected `Clock`; the core never reads system time
  directly.
- **ID formats:**
  - resource and unit IDs: `^[a-z0-9][a-z0-9._-]{0,62}$`;
  - lease IDs: `lse_<ULID>`; request IDs: `req_<ULID>`; promotion IDs:
    `prm_<ULID>`; notice IDs: `ntc_<ULID>`.
- **Output:**
  - human-readable when stdout is a TTY; JSON otherwise, or with `--json`;
  - never prompt or read stdin unless stdin is a TTY;
  - errors in JSON mode: `{"error":{"code":"<snake_case>","message":"…"}}`.
- **CLI exit codes:**
  - 0: ok
  - 2: usage error
  - 3: not found
  - 4: refused or conflict
  - 5: daemon unavailable
  - 6: queued, when called with `--no-wait`
  - `clerk run`: the child's exit code
- **Agent gating:** when `CLAUDE_CODE_SESSION_ID` is set, `clerk admin …`,
  `clerk approve` and `--override` need `--user-requested "<reason>"`. The
  reason is written to the audit log.
- **Tone:** messages may be playfully bureaucratic, but the first sentence must
  always state plainly what happened and what to do.

## Review Focus

1. **Unit settings or health commands containing shell metacharacters,** e.g. a
   path of `/tmp/x; touch /tmp/pwned` or `$(…)`. Nothing in the settings may
   ever be executed. → Add to the Task 4 env test: a setting of
   `$(touch <tmp>/pwned)`, asserting that `<tmp>/pwned` does not exist after the
   check.
2. **Two `clerk` invocations at once with no daemon running.** Exactly one
   daemon must start, and both commands must succeed. → Test in Task 13.
3. **The daemon is down while lease deadlines pass.** On restart, the overdue
   expiries, cleanup windows and claim windows must be processed straight away,
   in deadline order. → Test in Task 13.
4. **The same board under two names:** `/dev/ttyUSB1` and its
   `/dev/serial/by-id/…` symlink, or a board re-plugged as `/dev/ttyUSB2`. It
   must be detected as a duplicate. → Tests in Task 5.
5. **A `CliProcess` holder (in phase 2, `clerk run`) killed with SIGKILL.**
   The lease must still end, marked dirty with reason `holder_gone`, and the
   queue must move on. → Test in Task 11.

---

## BDD skeleton (the contract)

This tree is the behaviour contract. Each task below owns one subtree and
writes **one test per effect**, grouped by scenario. Test naming is set by the
styleguide (Task 1).

```
Ids
    ResourceId::parse / UnitId::parse
        On valid id ("esp32-c6", "a", 63 chars)
            - returns the id unchanged
        On invalid id (empty, 64 chars, uppercase, leading "-", "/", unicode)
            - returns IdError::Invalid naming the rule

Priority
    ordering
        On comparison
            - High > Normal > Low

ProjectResolver
    resolve(cwd)
        On cwd inside a git repo
            - returns ProjectKey of the repo's common git dir (canonical)
        On cwd inside a linked worktree of that repo
            - returns the same ProjectKey as the main checkout
        On cwd outside any git repo
            - returns ProjectKey of the canonical cwd
        On nonexistent cwd
            - returns ScopeError::NoSuchDirectory

Inventory
    create_resource
        On new id in scope
            - persists resource with type, scope, description, health check
            - writes audit entry "resource_created"
        On id already used in same scope
            - returns InventoryError::AlreadyExists
        On health check not offered by the type (HealthSpec::validate fails)
            - returns InventoryError::InvalidSettings { field: "health" }
        On project resource whose id equals a user resource id
            - succeeds with Warning::ShadowsUserResource
        On user resource whose id equals an existing project resource id
            - succeeds with Warning::ShadowedByProject
    list_resources / get_resource
        On caller in project P
            - returns user resources and P's resources, never other projects'
            - marks P resources that shadow a user resource
            - resolves a shadowed id to the project resource
        On caller with no project (plain CLI outside a repo)
            - returns user resources and that directory's project resources
        On admin view (all scopes)
            - returns every resource with its scope
    update_resource / delete_resource
        On visible resource
            - applies change and writes audit entry
        On resource invisible to caller
            - returns InventoryError::NotFound
        On delete of resource with held or queued units
            - returns InventoryError::Busy; resource unchanged
    add_unit / update_unit
        On settings valid for the resource type
            - persists unit; returns Created with dedup warnings (see Dedup)
        On settings invalid for type (missing path, port out of range)
            - returns InventoryError::InvalidSettings naming the field
    remove_unit
        On free unit
            - deletes unit and its links
        On held or queued unit
            - returns InventoryError::Busy
    retire_unit
        On free unit
            - removes unit immediately
        On held unit
            - marks retiring; no new grants; removed when lease ends

DeviceIdentity::from_settings
    On a symlink (/dev/serial/by-id/X -> /dev/ttyUSB1)
        - canonical_path is the symlink target
    On a char device with sysfs USB ancestry
        - captures rdev and USB (vendor, product, serial)
    On a regular file or directory
        - captures canonical_path and (st_dev, st_ino)
    On missing path
        - still returns identity with canonical_path = given path (registration
          of an unplugged board is allowed)
ServiceIdentity::from_settings
    On host "localhost", "127.0.0.1", "::1"
        - normalises all to the loopback class
    On listen 0.0.0.0:3000 vs connect/listen 127.0.0.1:3000
        - reports overlap
    On same host, different port
        - reports no match
    On equal service_id with different host/port
        - reports a match on service_id
GenericIdentity
    On equal identity_key
        - match
    On different identity_key
        - no match
Dedup::matches(a, b)
    On canonical path equal, or rdev equal, or USB serial triple equal, or
    inode equal
        - returns Match with the signal that hit
    On device renumbered (ttyUSB1 -> ttyUSB2) with same USB serial
        - returns Match(UsbSerial)
    On different types
        - returns NoMatch

Inventory duplicate handling (add_unit / update_unit)
    On match in same scope
        - returns InventoryError::DuplicateInScope { resource, unit }
    On project unit matching a user-scope unit
        - succeeds with Warning::DuplicateOfUserUnit { resource, unit }
        - links the two units into one identity group
    On match in another project's scope
        - succeeds with Warning::SimilarInOtherProject { project, promotion_id }
        - creates a pending promotion of the other resource to user scope
        - links the two units
        - discloses only the other project's path, not settings or notes
Promotions
    approve(promotion_id, new_id?)
        On no id clash
            - moves the resource to user scope keeping its id
            - keeps existing links; writes audit entry
        On id clash with a user resource and no new id
            - returns PromotionError::IdClash
        On id clash with new id given
            - moves and renames
    list
        - returns pending promotions with requester and reason

HealthSpec::validate(type)
    On a built-in check offered by the type
        - accepted
    On a built-in check not offered by the type (e.g. device + http)
        - returns InvalidHealthSpec naming the allowed checks
    On no spec given
        - resolves to the type default (device: [exists, not-open]; service:
          [tcp-connect] for connect units, [port-free] for listen units;
          generic: [none])
    On a list containing a check not offered by the type
        - returns InvalidHealthSpec naming the offending entry

HealthCheck::run(resource, unit)
    On device "exists" and path exists
        - Healthy
    On device "exists" and path missing
        - Unhealthy("path missing")
    On device "accessible" and path not openable read/write by this user
        - Unhealthy("not accessible")
    On service "tcp-connect" with listening port
        - Healthy
    On service "tcp-connect" with nothing listening
        - Unhealthy("connect failed")
    On service "port-free" and port already bound
        - Unhealthy("port in use")
    On service "http" returning 2xx
        - Healthy
    On service "http" returning non-2xx or no answer
        - Unhealthy with status or error
    On device "not-open" and another process of this user holds the path open
        - Unhealthy("open by pid <pid> (<comm>)")
    On device "not-open" and no process holds it open
        - Healthy
    On "none"
        - Healthy
    On list [exists, not-open] where exists fails
        - Unhealthy("path missing"); not-open is not evaluated
    On list where every entry passes
        - Healthy
    On custom command exiting 0
        - Healthy; command received CLERK_UNIT_<SETTING> env vars, not argv
    On custom command exiting non-zero
        - Unhealthy with captured stderr (truncated to 1 KiB)
    On custom command exceeding health_timeout
        - Unhealthy("timeout"); process killed
    After any run
        - unit's last_health and last_health_at are recorded

Scheduler
    acquire
        On free healthy unit in target resource
            - returns Granted with unit settings and ends_at = now + duration
            - lease state Held; audit "granted"
        On specific unit target
            - only that unit (or its group) is considered
        On several free units
            - grants one; a second acquire gets a different unit
        On free unit failing health check
            - skips it, grants another healthy unit
        On no healthy free unit
            - returns Queued { request_id, position, estimated_wait }
        On unit dirty
            - Grant carries dirty { reason, at, previous_holder }; flag cleared
        On linked unit held through another resource
            - treats the group as held
        On retiring unit
            - never grants it
        On caller already holding this exact unit
            - returns SchedulerError::AlreadyHeld
    queue ordering and dispatch
        On unit freed with waiters
            - offers to highest priority, then earliest request
            - waiters of all resources containing a group member compete
        On offer
            - lease state Offered; notice Offered with claim deadline
        On claim within claim_window
            - state Held; ends_at = claim time + duration
        On claim window passing
            - offer withdrawn; request dropped; notice QueueDropped; next waiter
        On cancel of queued request
            - request removed; later positions decrease; notices PositionChanged
        On waiter blocked by invisible holder
            - status shows "held by an agent in another project" only

LeaseLifecycle
    extend
        On held lease, no higher-priority waiter
            - ends_at updated; warning re-armed; audit "extended"
        On non-interruptible lease with strictly higher-priority waiter
            - returns SchedulerError::ExtendRefused naming waiting purpose
    timers (tick(now))
        On now >= ends_at - warning_lead
            - one ExpiryWarning notice per lease (not repeated)
        On now >= ends_at without extension
            - state Cleaning (cause Expired); notice CleanupWindowStarted
    release
        On held lease
            - state Cleaning (cause Released); notice to holder with deadline
    confirm(Clean)
        On cleaning lease
            - lease Ended; unit not dirty; next dispatch
    request_cleanup_extension
        On first request
            - deadline += cleanup_extension; audit with reason
        On second request
            - returns SchedulerError::ExtensionUsed
    cleanup window passing
        On no confirmation
            - lease Ended; unit dirty with cause (expired|window_passed); dispatch
        On nobody waiting
            - unit returns to free pool, still dirty

Interruption
    acquire against fully held resource
        On interruptible holder with lower priority than request
            - sends YieldRequested to that holder; request waits at head
        On equal priority and request duration < holder remaining time
            - sends YieldRequested
        On several interruptible holders
            - asks the one with most remaining time, then lowest priority
        On no interruptible holder
            - plain queue
    holder answers
        On confirm(Yielded)
            - holder lease Paused (clock stopped, remaining time stored)
            - unit offered to the requester as borrower
        On refuse_yield or yield_window passing
            - next interruptible holder asked; else request stays at head
    borrower finishes
        On borrower release + cleanup (confirmed or not)
            - unit returns to paused holder before any queue; clock resumes
            - notice UnitReturned { cleanup_confirmed }
    borrower overruns its declared duration
        - paused holder gets BorrowerOverran
        On reclaim by paused holder
            - borrower lease enters Cleaning (cause Reclaimed); then unit returns
    override
        On acquire with override_reason against non-interruptible holder
            - holder lease Cleaning (cause Override); notice Overridden
            - after window, handed to overrider regardless; dirty if unconfirmed
            - holder lease Ended (not paused)
            - audit "override" with reason

HolderLiveness
    holder_gone(holder)
        On holder with queued requests
            - requests dropped
        On holder with held lease
            - lease Cleaning→Ended immediately, unit dirty(holder_gone), dispatch
        On paused holder
            - pause claim forfeited; unit stays with borrower then normal queue
    CliProcess liveness probe
        On pid no longer alive, or pid reused (start time differs)
            - holder_gone fired within one probe interval
    AgentSession / User holders (phase 1)
        - only lease expiry ends them (no liveness probe until phase 3)

Notices
    publish
        On scheduler event
            - notice stored for its holder with kind, lease/unit, other party's
              purpose (omitted if invisible), deadline, action text
    take(holder, wait)
        On pending notices
            - returns them oldest first and marks delivered
        On none pending
            - waits up to `wait`, returns empty on timeout

Audit
    record / list
        - every inventory change, grant, handover (cleanup confirmed?), yield
          ask/answer, override (+reason), promotion, admin action is listed
          with time, actor holder, user_requested reason if any

Daemon
    ensure_running (client side)
        On daemon running and same version
            - connects without spawning
        On no daemon
            - spawns `clerk daemon` detached, waits until socket answers (≤5 s)
        On two concurrent ensure_running calls
            - exactly one daemon process; both connect
        On daemon version differs from client
            - returns DaemonError::VersionMismatch with both versions
    startup
        On existing database with leases
            - rebuilds timers; processes overdue deadlines in deadline order
        On stale socket file and no live daemon
            - removes it and binds
    shutdown (`clerk daemon stop`)
        - stops accepting, finishes current transaction, removes socket

AdminOps (engine operations; all audit "admin_<op>" with actor + user_requested reason)
    admin_release(lease)
        - lease Ended now; unit dirty(admin_forced); dispatch
    evict(holder)
        - all leases of holder ended dirty(admin_forced); its requests dropped
    dequeue(request) / clear_queue(resource)
        - requests removed; their holders get QueueDropped
    remove_unit_force / delete_resource_force
        - removed even if held or queued; holders and waiters get ResourceRemoved
    clean(unit)
        - dirty flag cleared
    link(a, b) / unlink(a, b)
        - identity groups updated; dispatch re-run
    move(resource, user | project:<path>)
        - scope changed without promotion flow
        On id clash in target scope
            - returns AdminError::IdClash; nothing changed
    rename(resource|unit, new_id)
        - id changed; existing leases and requests keep working
    edit_lease(lease, ends_at?, priority?, interruptible?)
        - lease updated; timers re-armed
    doctor(fix)
        - reports leases on missing units, requests for missing resources, dead
          CliProcess holders, device units whose path vanished (warning only),
          links to missing units
        On fix = true
            - repairs each finding and audits it
    export
        - returns JSON {version: 1, resources[units], links}
    import(doc, replace)
        On id clash and replace = false
            - returns AdminError::ImportClash listing ids; nothing changed
        On replace = true or no clash
            - creates or overwrites; dedup and links still run
        On export -> wipe -> import
            - ids, scopes, settings, notes and links are identical
rehome_holder(lease, new_holder)
    On caller is the lease's current holder
        - lease and its notices now belong to new_holder
    On caller not the holder
        - returns SchedulerError::NotHolder
```

---

## File structure

```
Cargo.toml, rustfmt.toml, clippy.toml, STYLEGUIDE.md
src/main.rs                  binary entry: clap dispatch only
src/lib.rs                   module wiring, re-exports
src/error.rs                 shared error enums (thiserror)
src/config.rs                Config (timings) + Paths (XDG / CLERK_HOME)
src/clock.rs                 Clock trait, SystemClock, ManualClock (tests)
src/model/{mod,ids,types}.rs ids, enums, domain structs
src/scope.rs                 ProjectResolver
src/store/mod.rs             Store (connection, migrations, tx helper)
src/store/migrations/0001_init.sql
src/store/{inventory,leases,notices,audit}.rs   SQL per aggregate
src/dedup/{mod,device,service,generic}.rs
src/health.rs
src/inventory.rs             Inventory service (rules + warnings + dedup + promotions)
src/scheduler/{mod,dispatch,lifecycle,interrupt,liveness}.rs
src/engine.rs                actor owning Store+Inventory+Scheduler; command enum
src/admin.rs                 admin operations (rules)
src/daemon/{mod,server,lifecycle}.rs
src/client.rs                RPC client + ensure_running
tests/common/mod.rs          temp CLERK_HOME, ManualClock helpers, fixtures
tests/{model,inventory,health,dedup,scheduler,lifecycle,interrupt,notices,liveness,engine,daemon,admin_ops}.rs
```

Split rule: SQL lives only in `store/*`. Business rules live only in
`inventory.rs` and `scheduler/*`. I/O (sockets, processes, TTY) lives only in
`daemon/*`, `client.rs`, `health.rs`, `scheduler/liveness.rs`, and phase 2's `cli/*`.

---

### Task 1: Project scaffold and styleguide

**Files:**
- Create: `Cargo.toml`, `rustfmt.toml`, `clippy.toml`, `STYLEGUIDE.md`,
  `src/main.rs`, `src/lib.rs`, `tests/common/mod.rs`, `tests/smoke.rs`,
  `.gitignore`

**Interfaces:**
- Produces:
  - `leclerk::VERSION: &str` (from `CARGO_PKG_VERSION`);
  - `tests/common::TestEnv::new() -> TestEnv`, which creates a tempdir and
    sets `CLERK_HOME`, and has `cmd(&self) -> assert_cmd::Command` for
    `clerk`.

**STYLEGUIDE.md must state** (the project's single styleguide; every later task
follows it):
- **Formatting:** `cargo fmt` with the default config plus
  `group_imports = "StdExternalCrate"` and `imports_granularity = "Module"`.
- **Lints:** `cargo clippy --all-targets -- -D warnings`, with
  `clippy::pedantic` warned and allow-listed per item only with a reason
  comment.
- **Naming:** Rust API Guidelines; modules singular nouns; constructors
  `new` / `open` / `parse`; boolean fields read as statements
  (`interruptible`, `retiring`).
- **Errors:**
  - library modules use `thiserror` enums in the module that owns them;
  - only `main.rs` and `cli/*` use `anyhow`;
  - no `unwrap()` / `expect()` outside tests, except for proven invariants with
    a comment;
  - error codes for the API are `snake_case` strings derived from the enum
    variant.
- **Time:** never call `SystemTime::now()` outside `clock.rs`.
- **Async:** tokio. Blocking SQLite work runs only on the engine's thread, never
  in async handlers.
- **Logging:** `tracing`; the daemon logs to its log file and the CLI to
  stderr; nothing on stdout except command output.
- **Tests:**
  - unit tests in a `mod tests` in the same file for pure functions;
  - behaviour tests in `tests/<area>.rs`, one `mod` per BDD
    module/method, one `mod` per scenario, one `#[test]` per effect;
  - test names describe the effect, e.g.
    `scheduler::acquire::on_free_healthy_unit::returns_granted_with_settings`.
- **Commits:** Conventional Commits (`feat:`, `fix:`, `test:`, `docs:`,
  `chore:`, `refactor:`), one task per commit or fewer.

- [ ] **Step 1:** Create the files above and declare the dependencies listed in
  Tech Stack. `main.rs` prints `clerk <VERSION>` on `--version` through clap.
- [ ] **Step 2:** Write `tests/smoke.rs::cli::version::prints_version`. It
  asserts that stdout contains `env!("CARGO_PKG_VERSION")` and that the exit
  code is 0.
- [ ] **Step 3:** Run
  `cargo test 2>&1 | tee /tmp/claude-1000/leclerk-test.log`. Expected: PASS.
- [ ] **Step 4:** Run
  `bash -c 'cargo fmt --check && cargo clippy --all-targets -- -D warnings' 2>&1 | tee /tmp/claude-1000/leclerk-lint.log`.
  Expected: clean.
- [ ] **Step 5:** Commit:
  `git add -A && git commit -m "chore: scaffold leclerk crate and styleguide"`

### Task 2: Domain model, clock, config

**Files:**
- Create: `src/model/{mod,ids,types}.rs`, `src/clock.rs`, `src/config.rs`,
  `src/error.rs`
- Test: unit tests in place, plus `tests/model.rs`

**Interfaces:**
- **IDs:**
  - `ResourceId::parse(&str) -> Result<ResourceId, IdError>`; `UnitId` is the
    same.
  - `LeaseId`, `RequestId`, `PromotionId`, `NoticeId` have `new() -> Self`
    (ULID with prefix) and `parse`.
- **Enums:**
  - `enum ResourceType { Device, Service, Generic }`, serde `snake_case`.
  - `enum Priority { Low, Normal, High }`, with `Ord`.
  - `enum Scope { User, Project(ProjectKey) }`; `struct ProjectKey(PathBuf)`.
- **Settings:**
  - `enum UnitSettings { Device { path: PathBuf }, Service { mode: ServiceMode, scheme: Option<String>, host: String, port: u16, service_id: Option<String> }, Generic { identity_key: String } }`
  - `enum ServiceMode { Connect, Listen }`
  - `UnitSettings::validate(ResourceType) -> Result<(), InvalidSettings>`
  - `UnitSettings::env_vars() -> Vec<(String, String)>`, producing
    `CLERK_UNIT_PATH`, `CLERK_UNIT_HOST`, `CLERK_UNIT_PORT`, …
- **Holders and callers:**
  - `enum HolderRef { AgentSession { session_id: String }, CliProcess { pid: u32, start_time: u64 }, User { tag: String } }`
  - `struct Caller { holder: HolderRef, project: Option<ProjectKey>, user_requested: Option<String> }`
- **Leases:**
  - `enum LeaseState { Offered, Held, Paused, Cleaning, Ended }`
  - `enum EndCause { Released, Expired, Yielded, Reclaimed, Override, HolderGone, CommandFailed, AdminForced }`
  - `enum DirtyReason { WindowPassed, Expired, HolderGone, Override, CommandFailed, AdminForced }`
  - `struct DirtyInfo { reason: DirtyReason, at: i64, previous_holder: HolderRef }`
- **Health spec** (behaviour tested in Task 4):
  - `struct HealthSpec { checks: Vec<Check> }`. All checks must pass, and they
    are evaluated in order.
  - `enum Check { Builtin(BuiltinCheck), Custom { command: String } }`
  - `enum BuiltinCheck { Exists, Accessible, NotOpen, TcpConnect, PortFree, Http, None }`,
    serde `kebab-case`
  - `HealthSpec::validate(&self, ResourceType) -> Result<(), InvalidHealthSpec>`
  - `HealthSpec::default_for(ResourceType, Option<ServiceMode>) -> HealthSpec`.
    `None` is `Option::None`, used when the unit's mode is unknown.
  - Stored on the resource as `health: Option<HealthSpec>`, where `None` means the
    type default resolved per unit. This replaces `health_command` in Task 3's
    `NewResource` and in Task 14's export format (`"health": [ {"builtin": "…"} |
    {"custom": "…"}, … ] | null`).
- **Clock:**
  - `trait Clock: Send + Sync { fn now_ms(&self) -> i64; }`
  - `SystemClock`; `ManualClock { set(i64), advance(Duration) }`
- **Config:**
  - `Config::load(&Paths) -> Result<Config, ConfigError>`, with the defaults
    from Global Constraints.
  - `Paths::resolve() -> Paths`, with fields `socket`, `lock`, `db`, `log`,
    `config`, honouring `CLERK_HOME`.

- [ ] **Step 1:** Write the failing tests for BDD `Ids` and `Priority`, plus:
  - `Config` defaults equal the constants;
  - `Paths` with `CLERK_HOME` puts everything under it;
  - `UnitSettings::validate` rejects port 0 and an empty path;
  - `env_vars` names are upper-case with the `CLERK_UNIT_` prefix.
- [ ] **Step 2:** Run `cargo test --test model`. Expected: FAIL (compile
  errors).
- [ ] **Step 3:** Implement the types above.
- [ ] **Step 4:** Run `cargo test`. Expected: PASS.
- [ ] **Step 5:** Commit: `feat: domain model, clock and config`

### Task 3: Store and inventory CRUD with scopes

**Files:**
- Create: `src/store/{mod,inventory,audit}.rs`,
  `src/store/migrations/0001_init.sql`, `src/inventory.rs`, `src/scope.rs`
- Test: `tests/inventory.rs`

**Interfaces:**
- **Store:**
  - `Store::open(&Path) -> Result<Store, StoreError>`; `Store::open_in_memory()`
  - `Store::tx<T>(&mut self, f: impl FnOnce(&Tx) -> Result<T, E>) -> Result<T, E>`
  - WAL mode and `foreign_keys = ON` set on open.
- **ProjectResolver:** `ProjectResolver::resolve(cwd: &Path) -> Result<ProjectKey, ScopeError>`.
  Resolution: run `git rev-parse --git-common-dir` in `cwd`, then canonicalise;
  fall back to the canonical `cwd`.
- **Inventory:**
  - `Inventory::new(clock: Arc<dyn Clock>)`
  - Methods take `(&Tx, &Caller, …)` and return
    `Result<Created<T>, InventoryError>` or a value:
    - `create_resource(NewResource { id, ty, scope, description, notes, health: Option<HealthSpec> })`
    - `update_resource(ResourceId, ResourcePatch)`
    - `delete_resource(ResourceId)`
    - `list_resources(View)`, where `View::{Caller, All}`
    - `get_resource(ResourceId)`
    - `add_unit(ResourceId, NewUnit { id, settings, notes })`
    - `update_unit(ResourceId, UnitId, UnitPatch)`
    - `remove_unit(ResourceId, UnitId)`
    - `retire_unit(ResourceId, UnitId)`
  - `struct Created<T> { value: T, warnings: Vec<Warning> }`
  - `enum Warning { ShadowsUserResource { id }, ShadowedByProject { id, project }, DuplicateOfUserUnit { resource, unit }, SimilarInOtherProject { project, promotion_id } }`
    The last two are produced in Task 6.
  - `enum InventoryError { InvalidId, NotFound, AlreadyExists, InvalidSettings { field }, Busy, DuplicateInScope { resource, unit } }`
- **Audit:** `audit::record(&Tx, actor: &Caller, action: &str, detail: serde_json::Value)`
- **Schema** (0001): tables `resources`, `units`, `unit_links`, `promotions`,
  `leases`, `requests`, `notices`, `audit`. Columns follow the model types.
  `units` has `dirty_reason`, `dirty_at`, `dirty_prev_holder`, `retiring`,
  `last_health`, `last_health_at`, and an `identity` JSON column (filled in
  Task 5). The scope is stored as `scope_kind` plus `project_key`, with a
  unique index on `(scope_kind, project_key, id)`.
  - Leases and requests reference units and resources by internal integer row
    IDs, so the renames in Task 14 don't break them.

- [ ] **Step 1:** Write the failing tests for the BDD subtrees
  `ProjectResolver` and `Inventory` (`create_resource`, `list/get`,
  `update/delete`, `add_unit/update_unit` without dedup, `remove_unit`,
  `retire_unit`). Also test that each mutation writes an audit row.
  - Use real temp git repos (`git init`, `git worktree add`) for the resolver
    tests.
  - Busy and retire cases: insert a lease row directly through a test helper
    in `tests/common`, since the scheduler doesn't exist yet.
- [ ] **Step 2:** Run `cargo test --test inventory`. Expected: FAIL.
- [ ] **Step 3:** Implement the store, migration, resolver and Inventory
  rules. The shadowing rule: lookup by ID tries the caller's project scope
  first, then user scope.
- [ ] **Step 4:** Run `cargo test`. Expected: PASS.
- [ ] **Step 5:** Commit: `feat: sqlite store and scoped inventory`

### Task 4: Health checks

**Files:**
- Create: `src/health.rs`
- Test: `tests/health.rs`

**Interfaces:**
- Consumes `HealthSpec` and `BuiltinCheck` from Task 2.
- `HealthChecker::new(timeout: Duration)`
- `HealthChecker::check(&self, spec: &HealthSpec, settings: &UnitSettings) -> HealthResult`
- `enum HealthResult { Healthy, Unhealthy(String) }`
- `check` runs `spec.checks` in order and stops at the first failure.
- `not-open`: scan `/proc/*/fd/*`. An entry whose target metadata matches the
  unit path's `(st_dev, st_ino)`, or for device nodes its `rdev`, is a holder.
  Unreadable `/proc/<pid>/fd` directories (other users) are skipped silently.
  The reported pid and `comm` come from `/proc/<pid>/comm`.
- `http`: a plain HTTP/1.1 GET to `scheme://host:port/` through the hyper
  client already in the stack (`scheme` defaults to `http`), with the health
  timeout.
- Custom commands run as `sh -c <command>`, with `UnitSettings::env_vars()`
  plus `CLERK_RESOURCE` and `CLERK_UNIT`, a null stdin, and stderr captured.
- Recording `last_health` happens in the caller (the scheduler, Task 7). The
  checker stays pure I/O.

- [ ] **Step 1:** Write the failing tests for BDD `HealthSpec::validate` and `HealthCheck::run`.
  - For `not-open`, hold a tempfile open in a spawned child (e.g.
    `sh -c 'exec 3<file; sleep 30'`), assert Unhealthy naming that pid, then
    kill the child and assert Healthy.
  - Use a tempfile for the device cases and a `TcpListener` on port 0 for the
    service cases, plus a minimal hyper server on port 0 for `http`.
  - The timeout case uses a command that waits longer than the configured
    1 s test timeout. Assert that it returns within 2 s and that the child is
    gone.
  - The env case uses a command that writes `$CLERK_UNIT_PATH` to a file.
    Assert the file content, and that the path never appeared in argv.
- [ ] **Step 2:** Run `cargo test --test health`. Expected: FAIL.
- [ ] **Step 3:** Implement. Listen check: try to bind `host:port`, and an
  `AddrInUse` error means unhealthy. Connect check: TCP connect with the
  timeout.
- [ ] **Step 4:** Run `cargo test --test health`. Expected: PASS.
- [ ] **Step 5:** Commit: `feat: per-type and custom health checks`

### Task 5: Identity extraction and matching

**Files:**
- Create: `src/dedup/{mod,device,service,generic}.rs`
- Test: `tests/dedup.rs`, plus unit tests for normalisation

**Interfaces:**
- `enum Identity { Device(DeviceIdentity), Service(ServiceIdentity), Generic(GenericIdentity) }`,
  serialisable (stored in `units.identity`).
- `DeviceIdentity { canonical_path, rdev: Option<u64>, inode: Option<(u64,u64)>, usb: Option<UsbId { vendor, product, serial }> }`
- `ServiceIdentity { mode, scheme, host_class: HostClass, port, service_id }`,
  where `enum HostClass { Loopback, Wildcard, Name(String), Ip(IpAddr) }`
- `Identity::from_settings(&UnitSettings, sysfs_root: &Path) -> Identity`.
  The sysfs root is injectable for tests (default `/sys`).
- `dedup::matches(&Identity, &Identity) -> Match`, where
  `enum Match { None, Hit(Signal) }` and
  `enum Signal { CanonicalPath, Rdev, Inode, UsbSerial, Endpoint, ServiceId, IdentityKey }`

**USB lookup:**
- Find `/sys/dev/char/<major>:<minor>`, follow the symlink, and walk up the
  parents to the first directory that has `idVendor`, `idProduct` and
  `serial`.
- Missing files mean `usb: None`, which is not an error.

**Service overlap rule:**
- An endpoint matches when the ports are equal and the hosts overlap.
- `Wildcard` overlaps everything.
- `Loopback` matches `Loopback`.
- Otherwise the hosts must be equal after lower-casing.
- A `service_id` that is equal and present on both sides is a hit on its own.

- [ ] **Step 1:** Write the failing tests for the BDD `*Identity` and
  `Dedup::matches` subtrees.
  - Build a fake sysfs tree in a tempdir for the USB and renumbering cases
    (Review Focus 4).
  - Use a real symlink in a tempdir pointing at a regular file for the
    canonical-path case.
- [ ] **Step 2:** Run `cargo test --test dedup`. Expected: FAIL.
- [ ] **Step 3:** Implement. Device `rdev` and inode come from `std::fs::metadata`
  (`MetadataExt`).
- [ ] **Step 4:** Run `cargo test --test dedup`. Expected: PASS.
- [ ] **Step 5:** Commit: `feat: per-type identity extraction and matching`

### Task 6: Duplicate handling, links, promotions

**Files:**
- Modify: `src/inventory.rs`, `src/store/inventory.rs`
- Test: `tests/inventory.rs` (new modules)

**Interfaces:**
- Consumes `Identity`, `dedup::matches` (Task 5) and `Warning` (Task 3).
- Produces:
  - `Inventory::identity_group(&Tx, unit_row: i64) -> Vec<i64>`: the transitive
    closure over `unit_links`. Task 7 uses it.
  - `Inventory::link(&Tx, a, b)` and `Inventory::unlink(&Tx, a, b)`
  - `Promotions::list(&Tx)`
  - `Promotions::approve(&Tx, &Caller, PromotionId, new_id: Option<ResourceId>) -> Result<Resource, PromotionError>`
  - `enum PromotionError { NotFound, IdClash { id } }`

- [ ] **Step 1:** Write the failing tests for BDD
  `Inventory duplicate handling` and `Promotions`.
  - Assert that the `SimilarInOtherProject` warning serialisation contains no
    settings or notes from the other project.
- [ ] **Step 2:** Run `cargo test --test inventory`. Expected: FAIL in the new
  modules.
- [ ] **Step 3:** Implement. On add or update, compute the identity, then
  compare against every stored unit's identity of the same type, across all
  scopes. Apply the BDD rules, store the links, and create the promotion row.
  - Approving clears the promotion. A same-ID clash in the target scope is
    checked first.
- [ ] **Step 4:** Run `cargo test`. Expected: PASS.
- [ ] **Step 5:** Commit: `feat: cross-scope duplicate detection, links and promotions`

### Task 7: Scheduler: acquire, queue, dispatch, claim

**Files:**
- Create: `src/scheduler/{mod,dispatch}.rs`, `src/store/{leases,notices}.rs`
- Test: `tests/scheduler.rs`

**Interfaces:**
- Consumes `Inventory::identity_group`, `HealthChecker`, `Clock` and `Config`.
- Produces:
  - `Scheduler::new(clock, config, health: Arc<HealthChecker>)`
  - `acquire(&Tx, &Caller, AcquireRequest) -> Result<AcquireOutcome, SchedulerError>`
  - `claim(&Tx, &Caller, LeaseId) -> Result<Lease, SchedulerError>`
  - `cancel(&Tx, &Caller, RequestId)`
  - `dispatch(&Tx)`: idempotent; offers every free group to its best waiter
  - `status(&Tx, View) -> StatusView`
  - Types:
    - `AcquireRequest { target: Target, duration: Duration, interruptible: bool, priority: Priority, purpose: String, override_reason: Option<String> }`
    - `enum Target { Resource(ResourceId), Unit(ResourceId, UnitId) }`
    - `enum AcquireOutcome { Granted(Grant), Queued { request_id, position: u32, estimated_wait: Option<Duration> } }`
    - `Grant { lease_id, resource, unit, settings, ends_at, dirty: Option<DirtyInfo> }`
    - `enum SchedulerError { NotFound, AlreadyHeld, ExtendRefused { waiting_purpose }, ExtensionUsed, NotHolder, BadState { state } }`
  - **Notices:**
    - `notices::publish(&Tx, Notice)`
    - `Notice { id, holder, kind: NoticeKind, lease_id, resource, unit, other_purpose: Option<String>, deadline: Option<i64>, action: String, created_at }`
    - `enum NoticeKind { Offered, QueueDropped, PositionChanged, ExpiryWarning, CleanupWindowStarted, CleanupEnding, YieldRequested, UnitReturned, BorrowerOverran, Overridden, LeaseEnded, ResourceRemoved }`

**Dispatch** (the tricky algorithm; pseudocode):

```
dispatch():
  for each identity group G that has no Offered/Held/Paused/Cleaning lease
      and contains at least one non-retiring unit:
    waiters = queued requests whose target resource contains a member of G
              (or whose specific unit is in G), ordered by (priority desc, created_at asc)
    for w in waiters:
      unit = first member of G inside w's target resource, non-retiring,
             health check Healthy (record last_health on every check)
      if unit: create lease Offered for w (claim deadline = now + claim_window),
               publish Offered, remove w from queue, recompute positions → PositionChanged; break
```

An immediate `acquire` grant follows the same rules for the caller, but its
lease starts `Held`, because the caller is answered synchronously. The queue
estimate is the sum, over waiters ahead, of the remaining time of the leases on
the target, divided by the unit count. It is `None` if unknown.

- [ ] **Step 1:** Write the failing tests for BDD `Scheduler/acquire` and
  `queue ordering and dispatch`.
  - Use `ManualClock`, and a `HealthChecker` stub where needed. The
    `HealthChecker` takes a trait object, so define
    `trait HealthProbe { fn check(...) -> HealthResult }`, implemented by
    `HealthChecker`.
- [ ] **Step 2:** Run `cargo test --test scheduler`. Expected: FAIL.
- [ ] **Step 3:** Implement acquire, dispatch, claim and cancel, and expire
  offers past their claim deadline inside `dispatch`.
- [ ] **Step 4:** Run `cargo test --test scheduler`. Expected: PASS.
- [ ] **Step 5:** Commit: `feat: scheduler acquire, queue and dispatch`

### Task 8: Lease lifecycle: timers, extend, release, cleanup, dirty

**Files:**
- Create: `src/scheduler/lifecycle.rs`
- Test: `tests/lifecycle.rs`

**Interfaces:**
- Produces, on `Scheduler`:
  - `extend(&Tx, &Caller, LeaseId, new_ends_at: i64)`
  - `release(&Tx, &Caller, LeaseId)`
  - `confirm(&Tx, &Caller, LeaseId, Confirm)`, where
    `enum Confirm { Clean, Yielded }`
  - `request_cleanup_extension(&Tx, &Caller, LeaseId, reason: &str)`
  - `tick(&Tx) -> Option<i64>`: processes everything due at `clock.now_ms()` in
    deadline order and returns the next deadline. The daemon sleeps until then.
  - `end_lease(&Tx, LeaseId, EndCause, confirmed: bool)`: shared by later tasks.
    It sets dirty when `!confirmed`, publishes `LeaseEnded`, and calls
    `dispatch`.

- [ ] **Step 1:** Write the failing tests for BDD `LeaseLifecycle`. These
  cover:
  - the warning lead formula: a 10 min lease warns at 8 min; a 2 h lease warns
    at 1 h 55 min;
  - exactly one warning per lease;
  - the dirty flag being delivered on the next grant and then cleared.
- [ ] **Step 2:** Run `cargo test --test lifecycle`. Expected: FAIL.
- [ ] **Step 3:** Implement. The cleanup deadline is stored on the lease row,
  and `tick` processes deadlines in this order: claim, warning, expiry,
  cleanup.
- [ ] **Step 4:** Run `cargo test`. Expected: PASS.
- [ ] **Step 5:** Commit: `feat: lease lifecycle with cleanup handshake and dirty units`

### Task 9: Interruption, borrowing, reclaim, override

**Files:**
- Create: `src/scheduler/interrupt.rs`
- Test: `tests/interrupt.rs`

**Interfaces:**
- Produces:
  - `set_interruptible(&Tx, &Caller, LeaseId, bool)`
  - `refuse_yield(&Tx, &Caller, LeaseId)`
  - `reclaim(&Tx, &Caller, LeaseId /* paused lease */)`
  - Internal: `maybe_request_yield(&Tx, RequestId)`, called by `acquire` when
    it queues.
  - The lease row gains `paused_remaining_ms`, `borrower_lease`,
    `paused_for_lease`, and `yield_asked_at` / `yield_deadline`.

**Interrupt rule:**
- A request R may interrupt an interruptible held lease L when
  `R.priority > L.priority`, or when
  `R.priority == L.priority && R.duration < L.ends_at - now`.
- Choose the candidate with the largest `ends_at - now`, then the lowest
  priority.
- Ask one holder at a time. If it refuses or misses `yield_window`, mark it
  asked and move to the next.
- The return of a paused unit takes precedence over every queue: `dispatch`
  checks for a paused owner first.

- [ ] **Step 1:** Write the failing tests for BDD `Interruption`, including:
  - the paused clock: remaining time is preserved exactly across a 30 min
    borrow;
  - the override path ending the holder's lease rather than pausing it.
- [ ] **Step 2:** Run `cargo test --test interrupt`. Expected: FAIL.
- [ ] **Step 3:** Implement. An override reuses `end_lease` with
  `EndCause::Override` after the cleanup window. Agent callers need
  `user_requested`; that is enforced in the CLI (phase 2), and the scheduler
  only records the reason.
- [ ] **Step 4:** Run `cargo test`. Expected: PASS.
- [ ] **Step 5:** Commit: `feat: interruptible leases, borrowing, reclaim and override`

### Task 10: Notices queue and audit listing

**Files:**
- Modify: `src/store/notices.rs`, `src/store/audit.rs`
- Test: `tests/notices.rs`

**Interfaces:**
- `notices::take(&Tx, &HolderRef, limit) -> Vec<Notice>`: returns undelivered
  notices oldest first and marks them delivered.
- `audit::list(&Tx, AuditFilter { since, actor, action, limit }) -> Vec<AuditEntry>`
- Waiting with a timeout is done by the engine (Task 12), not the store.

- [ ] **Step 1:** Write the failing tests for BDD `Notices/take` (store level)
  and `Audit`.
  - For a holder that can't see the other party's resource, assert that
    `other_purpose` is `None`. This is the cross-project rule.
- [ ] **Step 2:** Run `cargo test --test notices`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test`. Expected: PASS.
- [ ] **Step 5:** Commit: `feat: notice queue and audit listing`

### Task 11: Holder liveness

**Files:**
- Create: `src/scheduler/liveness.rs`
- Test: `tests/liveness.rs`

**Interfaces:**
- `Scheduler::holder_gone(&Tx, &HolderRef)`
- `liveness::process_alive(pid: u32, start_time: u64) -> bool`. It reads
  `/proc/<pid>/stat` field 22, and a missing file or a different start time
  means dead.
- `liveness::probe_all(&Tx, &Scheduler)`: checks every `CliProcess` holder that
  has leases or requests. The engine calls it every 2 s.
- `liveness::self_start_time() -> u64`: used by the CLI to build its
  `HolderRef`.

- [ ] **Step 1:** Write the failing tests for BDD `HolderLiveness`.
  - Spawn a real child (`sleep 1000` via `std::process::Command`), register it
    as the holder, SIGKILL it, run `probe_all`, and assert
    dirty(`holder_gone`) and that the next waiter was offered the unit
    (Review Focus 5).
  - A PID-reuse test fakes the start time.
- [ ] **Step 2:** Run `cargo test --test liveness`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test`. Expected: PASS.
- [ ] **Step 5:** Commit: `feat: holder liveness and holder_gone handling`

### Task 12: Engine actor and RPC surface

**Files:**
- Create: `src/engine.rs`, `src/daemon/server.rs`
- Test: `tests/engine.rs`

**Interfaces:**
- **Engine:**
  - `Engine::spawn(store: Store, clock, config) -> EngineHandle`. It runs on a
    dedicated OS thread, owns the `Store`, `Inventory` and `Scheduler`, and
    receives `Command`s over a channel. Every command runs in one transaction.
  - After every command, and on timer wake-up, it calls `scheduler.tick` and
    `dispatch`, then sleeps until the next deadline. It also wakes every 2 s
    for liveness.
  - `EngineHandle::call(Command) -> Result<serde_json::Value, ApiError>` (async)
  - `EngineHandle::wait_notices(HolderRef, timeout) -> Vec<Notice>` (async,
    woken through a per-holder `tokio::sync::Notify`)
- **Command:** an enum with one variant per operation in Tasks 3–11 and 14. It
  carries the `Caller` and its args.
- **RPC over HTTP** (axum, Unix socket):
  - `GET /v1/version` returns `{ "version": "<semver>" }`
  - `POST /v1/rpc/<method>`: body `{ "caller": Caller, "args": {…} }`;
    response `200 { "result": … }` or `4xx { "error": { "code", "message" } }`
  - `GET /v1/notices?holder=<json-urlencoded HolderRef>&wait=<secs, max 60>`:
    the long-poll
  - Method names are the snake_case `Command` variants (`create_resource`,
    `acquire`, `admin_release`, …).
- `ApiError` maps every domain error to a `code` and an HTTP status: 404 not
  found, 409 conflict, busy or refused, 422 invalid.

- [ ] **Step 1:** Write the failing tests:
  - `engine::call::on_concurrent_acquires::grants_each_unit_once`: 20 parallel
    acquires against 2 units give exactly 2 grants and 18 queued.
  - `engine::wait_notices::on_offer_while_waiting::returns_within_100ms`
  - `server::rpc::on_unknown_method::returns_404_error_json`
  - `server::rpc::on_domain_error::maps_code_and_status`
- [ ] **Step 2:** Run `cargo test --test engine`. Expected: FAIL.
- [ ] **Step 3:** Implement the engine and the axum router. Serve on any
  listener; the socket binding comes in Task 13.
- [ ] **Step 4:** Run `cargo test`. Expected: PASS.
- [ ] **Step 5:** Commit: `feat: engine actor and JSON RPC surface`

### Task 13: Daemon lifecycle and client

**Files:**
- Create: `src/daemon/{mod,lifecycle}.rs`, `src/client.rs`
- Modify: `src/main.rs` (subcommand `daemon` / `daemon status|stop|restart`)
- Test: `tests/daemon.rs`

**Interfaces:**
- **Daemon:**
  - `daemon::run(paths, config) -> anyhow::Result<()>`:
    1. take the exclusive `fs4` lock on `paths.lock`, or exit 0 quietly if it
       is held;
    2. remove a stale socket;
    3. create the run directory with mode 0700;
    4. bind the socket with mode 0600;
    5. open the DB;
    6. spawn the engine, which runs a startup `tick` straight away;
    7. serve until SIGTERM or `/v1/rpc/daemon_stop`.
- **Client:**
  - `Client::connect(paths) -> Result<Client, DaemonError>`
  - `Client::ensure_running(paths) -> Result<Client, DaemonError>`: connect; if
    that fails, spawn `current_exe() daemon` with `setsid`, stdio going to the
    daemon log, then retry with backoff for up to 5 s. Afterwards it checks the
    version.
  - `Client::call(method, &Caller, args) -> Result<Value, ApiError>`
  - `Client::wait_notices(&HolderRef, secs)`
  - `enum DaemonError { Unavailable, VersionMismatch { client, daemon } }`

- [ ] **Step 1:** Write the failing tests for BDD `Daemon`, using real
  processes under `TestEnv`. They include:
  - the concurrent `ensure_running` race: 8 threads, then `pgrep` the daemon
    count under that `CLERK_HOME`, expecting 1 (Review Focus 2);
  - restart with overdue deadlines: create a lease with a short duration, stop
    the daemon, let the deadline pass, start it, and assert the expiry was
    processed and the unit offered (Review Focus 3);
  - a stale socket file.
- [ ] **Step 2:** Run `cargo test --test daemon`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test`. Expected: PASS.
- [ ] **Step 5:** Commit: `feat: auto-started daemon over unix socket`

### Task 14: Admin operations and holder re-homing

**Files:**
- Create: `src/admin.rs` (rules, using `store/*`, `inventory.rs` and
  `scheduler/*`)
- Modify: `src/engine.rs` (admin `Command` variants and `rehome_holder`)
- Test: `tests/admin_ops.rs`

**Interfaces:**
- Consumes:
  - `Scheduler::end_lease`, `Scheduler::holder_gone` and `dispatch` (Tasks 8,
    11, 7)
  - `Inventory::link` / `unlink` (Task 6)
  - `liveness::process_alive` (Task 11)
- Produces:
  - Engine RPC methods `admin_release`, `admin_evict`, `admin_dequeue`,
    `admin_clear_queue`, `admin_remove_unit`, `admin_delete_resource`,
    `admin_clean`, `admin_link`, `admin_unlink`, `admin_move`, `admin_rename`,
    `admin_edit_lease`, `admin_doctor`, `admin_export`, `admin_import`,
    `rehome_holder`.
  - `enum HolderSpec { Session(String), Pid(u32), User(String) }`, parsed from
    `session:<id>`, `pid:<pid>` and `user:<tag>`.
  - `enum AdminError { IdClash { id }, ImportClash { ids: Vec<String> }, NotFound }`
  - `DoctorReport { findings: Vec<Finding { kind, subject, fixed: bool }> }`
  - **Export format** (the contract):
    `{ "version": 1, "resources": [ { "id", "type", "scope": "user" | {"project": "<path>"}, "description", "notes", "health", "units": [ { "id", "settings", "notes" } ] } ], "links": [ [ ["<scope>","<resource>","<unit>"], [...] ] ] }`
- The rename must work because leases reference internal row IDs (Task 3).
  The engine performs no gating; the confirmation and `--user-requested` rules
  are enforced by the CLI in phase 2. The engine only records
  `caller.user_requested`.

- [ ] **Step 1:** Write the failing tests for BDD `AdminOps` and
  `rehome_holder`, one module per operation, at engine level.
- [ ] **Step 2:** Run `cargo test --test admin_ops`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the full verification:
  `bash -c 'cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test' 2>&1 | tee /tmp/claude-1000/leclerk-p1-full.log`.
  Expected: all clean, all PASS.
- [ ] **Step 5:** Commit: `feat: admin operations and holder re-homing`

## Self-review notes

- **Spec coverage** (phase 1 scope):
  - §4.1–4.6: Tasks 3–6
  - §4.5: Task 4 (and recorded in Task 7)
  - §5.1–5.4: Tasks 7–8
  - §5.5–5.6: Task 9
  - §5.7 and CLI-holder liveness (§7.3): Task 11
  - §7.1–7.2 daemon and persistence: Tasks 12–13
  - admin operations behind §8.4: Task 14
  - §8.5 audit: Tasks 3 and 10
- **Deferred by design:**
  - phase 2: CLI commands, TTY behaviour and gates (§7.3 CLI forms, §8.4);
  - phase 3: §6, §7.3 proxy liveness and reconnect grace, §8.1–8.3, §11;
  - phase 4: §9 and the end-to-end harness.
