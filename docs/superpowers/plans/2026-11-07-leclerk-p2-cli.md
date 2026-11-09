# Le Clerk: Phase 2 (CLI) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the `clerk` command-line interface on top of the phase 1 core.
It covers inventory, views, leasing (interactive and non-interactive) and the
gated admin tools, for people, scripts and agents calling it through Bash.

**Architecture:**
- Every command is a thin client: build a `Caller`, call the daemon through
  `leclerk::client::Client::ensure_running`, render the result.
- **No business rules in `cli/*`.** The CLI owns only:
  - TTY detection and rendering;
  - the interactive live view (raw-mode keys);
  - `clerk run` process supervision;
  - the safety gates (confirmation, `--yes`, `--override`).

**Tech Stack:** as phase 1, plus `script` (util-linux) in tests to get a PTY.

**Spec:** `docs/superpowers/specs/2026-11-07-leclerk-design.md`. **Prerequisite:**
phase 1 is complete (`docs/superpowers/plans/2026-11-07-leclerk-p1-core.md`).
Its Interfaces blocks give the RPC method names and types used here. Its
`STYLEGUIDE.md` applies.

## Global Constraints

All of phase 1's Global Constraints apply. The ones this phase implements:

- **Output:**
  - human-readable when stdout is a TTY; JSON otherwise, or with `--json`;
  - never prompt or read stdin unless stdin is a TTY, with one exception:
    `-f -` on the inventory commands reads a YAML document from stdin because
    the caller asked for it explicitly;
  - errors in JSON mode: `{"error":{"code":"<snake_case>","message":"…"}}`.
- **Exit codes:**
  - 0: ok
  - 2: usage error
  - 3: not found
  - 4: refused or conflict
  - 5: daemon unavailable
  - 6: queued, when called with `--no-wait`
  - `clerk run`: the child's exit code
- **Agent gating:** when `CLAUDE_CODE_SESSION_ID` is set, `clerk admin …`
  and `clerk approve` need `--override "<reason>"` (the user told the agent
  to do it, and why). `clerk acquire --override "<reason>"` is the same flag:
  it both grabs the resource regardless and records the reason. The reason
  is passed to the daemon in `Caller.override_reason`.
- **Keys:** Ctrl+C (`0x03`) cancels or releases; Ctrl+] (`0x1d`) detaches.
- **Durations:** `--for` accepts `<n>s|m|h|d` with n ≥ 1, up to 30 days.
- **Tone:** messages may be playfully bureaucratic, but the first sentence must
  always state plainly what happened and what to do.

## Review Focus

1. **An agent or script runs `clerk acquire` or an admin command without a
   TTY.** It must never wait for keyboard input or hang on a prompt. → Tests in
   Tasks 1, 2 and 3.
2. **The interactive view exits abnormally** (an error from the daemon, a
   SIGTERM, a panic). The terminal must come back in cooked mode. → Test in
   Task 2: kill `clerk acquire` with SIGTERM under `script`, then assert that
   `stty -a` shows `icanon`.
3. **Bad durations:** `--for 0`, `-5m`, `abc` or `100000d`. Each must exit 2
   with a message naming the accepted format, and nothing may reach the daemon.
   → Tests in Task 2.
4. **The daemon can't start** (e.g. the state directory isn't writable). The
   command must exit 5 within 6 s with a plain message, never hang. → Test in
   Task 1.
5. **The working directory has spaces or unicode in its path,** or is outside
   any repo. The project must resolve and commands must work. → Test in Task 1.

---

## BDD skeleton (the contract)

```
CLI
    output mode
        On stdout TTY
            - human tables / text
        On non-TTY or --json
            - JSON only on stdout; logs never on stdout
    inventory commands (resource create|update|delete|list|show, unit add|update|remove|retire)
        - map 1:1 to daemon calls, print warnings, exit codes per constraints
    YAML input (-f PATH | -f -) on resource create|update, unit add|update
        On a document with the command's fields
            - sends the same daemon call as the equivalent flags
            - a multi-line usage block arrives byte-for-byte (trailing newline per YAML rules)
        On -f - with a heredoc
            - reads stdin to EOF and proceeds
        On no -f with stdin a pipe that never closes
            - never reads stdin (existing non-TTY guarantee)
        On update with a subset of keys
            - only those fields are in the patch
        On a field given both as a flag and in the YAML
            - exit 2 naming the field; no daemon call
        On an unknown key
            - exit 2 naming the key; no daemon call
        On malformed YAML, a non-mapping document, or more than one document
            - exit 2 with the parser's line and column; no daemon call
        On input over 64 KiB
            - exit 2; no daemon call
        On a scalar tag that YAML reads as a number or boolean (tags: [esp32, 2024, no])
            - exit 2 naming the entry and saying to quote it; never coerced silently
        On -f PATH that does not exist
            - exit 2 naming the path
    caller identity
        On CLAUDE_CODE_SESSION_ID set
            - holder AgentSession(id); project from cwd
        On interactive acquire / run
            - holder CliProcess(pid, start_time)
        On non-interactive acquire without session id, or after detach
            - holder User(lease-scoped)
    daemon unavailable
        - exit 5 within 6 s with plain message
    acquire
        On stdin TTY
            - shows queue position, then grant, then notices live
            - Ctrl+C while queued cancels; while held releases with confirm(Clean)
            - Ctrl+] detaches: lease kept, holder becomes User, prints lease id
            - terminal restored to cooked mode on every exit path
        On stdin not TTY
            - prints Granted or Queued JSON and exits (0 / 6) without reading stdin
            - with --wait blocks until granted, then prints and exits 0
        On invalid --for
            - exit 2 naming the format; no daemon call
    attach <lease>
        - shows live view; Ctrl+C releases; Ctrl+] detaches again
    release <lease>
        - starts cleanup and confirms Clean (CLI release is deliberate)
    extend <lease> --for D
        - new end = now + D; ExtendRefused shown with waiting purpose, exit 4
    run <target> -- cmd
        On grant
            - runs cmd with CLERK_LEASE_ID, CLERK_RESOURCE, CLERK_UNIT and
              CLERK_UNIT_<SETTING> env vars
        On cmd exit 0
            - release with confirm(Clean); exits 0
        On cmd exit non-zero
            - release; unit dirty(command_failed); exits with cmd's code
        On override / admin release of its lease
            - SIGTERM to cmd at cleanup deadline
    wait
        - prints notices for the caller as they arrive (long-poll loop)
        - --once returns after the first batch
    status / resources / leases / queue / log / promotions / approve
        - read views; invisible holders shown as "held by an agent in another project"
        - approve gated for agents
    acquire --override "<reason>"
        On empty reason
            - refused, exit 2
admin (all subcommands)
    On TTY
        - asks confirmation naming the effect; "n" aborts with nothing changed
    On non-TTY without --yes
        - refused, exit 4, nothing changed
    On agent caller without --override "<reason>"
        - refused, exit 4
    On success
        - calls the matching phase-1 admin_* method; prints its result
```

## File structure

```
src/cli/mod.rs        clap command tree, dispatch
src/cli/output.rs     Mode detection, JSON/human rendering, error → exit code
src/cli/caller.rs     Caller construction (session env, pid/start_time, project)
src/cli/inventory.rs  resource / unit commands
src/cli/input.rs      -f PATH / -f - YAML loading and flag merge
src/cli/views.rs      status, resources, leases, queue, log, promotions, approve
src/cli/lease.rs      acquire, attach, release, extend, run, wait, live view
src/cli/gate.rs       confirmation / --yes / --override gate
src/cli/admin.rs      admin subcommands
src/main.rs           (modify) delegate to cli::run
tests/cli.rs, tests/cli_tty.rs, tests/cli_admin.rs
```

---

### Task 1: CLI framework, inventory and view commands

**Files:**
- Create: `src/cli/{mod,output,caller,inventory,input,views,gate}.rs`
- Modify: `Cargo.toml` (add `serde-saphyr = "2"`)
- Modify: `src/main.rs`
- Test: `tests/cli.rs`

**Interfaces:**
- Consumes `Client::ensure_running`, `Client::call` and `DaemonError` (phase 1,
  Task 13), plus the RPC method names from phase 1 Tasks 12 and 14.
- Produces:
  - `cli::caller::current(kind: CallerKind, override_reason: Option<String>) -> anyhow::Result<Caller>`,
    where `enum CallerKind { Interactive, Detached, Run }`:
    - `CLAUDE_CODE_SESSION_ID` gives `AgentSession`;
    - otherwise `CliProcess { pid, start_time }` for `Interactive` and `Run`,
      and `User { tag: <ULID> }` for `Detached`;
    - project from `ProjectResolver::resolve(cwd)`.
  - `cli::output::Mode::detect(json_flag: bool) -> Mode` (`Human` | `Json`),
    based on `std::io::IsTerminal` for stdout
  - `Output::emit<T: Serialize + HumanRender>(&T)`
  - `Output::fail(ApiError | DaemonError) -> ExitCode`
  - `trait HumanRender { fn render(&self) -> String; }`
  - `cli::gate::require(effect: &str, yes: bool, caller: &Caller) -> Result<(), GateRefusal>`.
    It prompts only when stdin is a TTY.
- **Commands:**
  - `clerk resource create <id> --type device|service|generic [--scope user|project] [--description TEXT] [--usage TEXT | --usage-file PATH] [--tag TAG[,TAG…]]… [--health CHECK[,CHECK…]] [--health-command SCRIPT]` (CHECK ∈ exists|accessible|not-open|tcp-connect|port-free|http|none; `--health-command` appends a custom check; omit both for the type default)
  - `resource update|delete|list|show`; `update` takes `--add-tag TAG[,TAG…]` and `--remove-tag TAG[,TAG…]` (repeatable), sent as the atomic `add_tags`/`remove_tags` patch (spec §4.8)
  - `clerk unit add <resource> <unit> (--path P | --host H --port N [--listen] [--scheme S] [--service-id X] | --key K) [--description TEXT]`
  - `resource show` prints the raw usage template; `acquire`/`attach` print both descriptions and the rendered usage on grant (spec §4.7)
  - `unit update|remove|retire`
  - **YAML input:** `resource create|update` and `unit add|update` take
    `-f PATH` or `-f -` (stdin, read to EOF only when given). The document is
    a mapping whose keys are the RPC arg names for that command minus the ID:
    `type`, `scope`, `description`, `usage`, `tags`, `health` for create;
    `description`, `usage`, `tags`, `add_tags`, `remove_tags`, `health` for
    update; the `UnitSettings` fields plus `description` for units. It is
    parsed with `serde-saphyr` into per-command structs with
    `deny_unknown_fields`, then merged with the flags; a field present in both
    is a usage error (exit 2). Input is capped at 64 KiB. String fields reject
    non-string scalars rather than coercing them.
    `cli::input::load(spec: &InputSpec) -> Result<Option<T>, InputError>`
    where `InputSpec` is `None | Stdin | Path(PathBuf)`.
  - Views: `status`, `resources [--tag TAG[,TAG…]]…` (all tags must match; tags shown in both list and status), `leases`, `queue <resource>`,
    `log [--since --actor --action --limit]`, `promotions`,
    `approve <promotion> [--as <id>]`
  - Global flags: `--json`, `--override <reason>`

- [ ] **Step 1:** Write the failing tests for the BDD `CLI` subtrees
  `output mode`, `inventory commands`, `caller identity`,
  `daemon unavailable`, and `status…` (with the `approve` gate). Run them
  through `assert_cmd` under `TestEnv`, where stdout is never a TTY. They
  include:
  - `cli::output::on_non_tty::emits_json_only_on_stdout`
  - `cli::inventory::on_duplicate_in_scope::exits_4_with_error_json`
  - `cli::inventory::on_similar_in_other_project::prints_warning_with_promotion_id`
  - `cli::views::on_stdin_pipe_never_closed::exits_without_reading_stdin`: run
    under a 5 s timeout (Review Focus 1)
  - `cli::daemon_unavailable::on_unwritable_state_dir::exits_5_within_6s`
    (Review Focus 4)
  - `cli::caller::on_cwd_with_spaces_and_unicode::resolves_project`
    (Review Focus 5)
  - `cli::approve::on_agent_without_override_reason::exits_4`
  - `cli::yaml_input::on_heredoc_usage_block::matches_flag_equivalent`
  - `cli::yaml_input::on_flag_and_key_both_set::exits_2_naming_field`
  - `cli::yaml_input::on_numeric_tag::exits_2_asking_to_quote`
- [ ] **Step 2:** Run `cargo test --test cli 2>&1 | tee /tmp/claude-1000/leclerk-cli.log`.
  Expected: FAIL.
- [ ] **Step 3:** Implement. Human rendering is plain aligned text, with no
  colour library.
- [ ] **Step 4:** Run `cargo test`. Expected: PASS.
- [ ] **Step 5:** Commit: `feat(cli): framework, inventory and views`

### Task 2: Leasing commands: acquire, attach, release, extend, run, wait

**Files:**
- Create: `src/cli/lease.rs`
- Test: `tests/cli.rs` (new modules) and `tests/cli_tty.rs`

**Interfaces:**
- Consumes the RPCs `acquire`, `claim`, `cancel`, `release`, `confirm`,
  `extend` and `rehome_holder`, and `Client::wait_notices` (phase 1).
- **Commands:**
  - `clerk acquire <resource>[/<unit>] --for <duration> [--interruptible] [--priority low|normal|high] [--purpose TEXT] [--override REASON] [--wait] [--no-wait]`
  - `clerk attach <lease>`
  - `clerk release <lease>`
  - `clerk extend <lease> --for <duration>`
  - `clerk run <target> --for <duration> [lease flags] -- <cmd…>`
  - `clerk wait [--once]`
- **Duration parsing:** `parse_duration(&str) -> Result<Duration, DurationError>`,
  per Global Constraints.
- **Interactive mode** applies only when stdin is a TTY:
  - raw mode through crossterm, restored by a guard on every exit path,
    including a panic hook and a SIGTERM/SIGINT handler;
  - the live view loops on `wait_notices` (30 s polls) and keypresses;
  - an `Offered` notice is claimed immediately;
  - Ctrl+C cancels or releases (confirm Clean);
  - Ctrl+] calls `rehome_holder` with `User { tag }`, prints the lease ID, and
    exits 0.
- **Non-interactive mode** never enables raw mode or reads stdin. `--no-wait`
  is the default (exit 6 when queued). With `--wait`, it polls notices and
  claims on `Offered`.
- **`clerk run`:**
  - holder `CliProcess`;
  - waits in the foreground and claims on offer;
  - spawns the child with the env vars from the BDD;
  - forwards SIGINT;
  - on `Overridden` or an admin release notice, sends SIGTERM to the child at
    the notice's deadline;
  - on child exit, releases with confirm Clean (exit 0), or ends the lease as
    `command_failed`.

- [ ] **Step 1:** Write the failing tests for the BDD `CLI` subtrees `acquire`,
  `attach`, `release`, `extend`, `run`, `wait` and the `--override` agent gate.
  They include:
  - `cli::acquire::on_stdin_not_tty::prints_queued_and_exits_6_without_reading_stdin`
    (Review Focus 1)
  - `cli::acquire::on_invalid_for::exits_2_for_each_of_0_neg_abc_100000d`
    (Review Focus 3)
  - `cli::run::on_cmd_exit_3::unit_dirty_command_failed_and_exit_3`
  - `cli::run::on_cmd::receives_clerk_unit_env`
  - `cli::acquire::on_session_env::lease_owned_by_agent_session`
  - `tests/cli_tty.rs` runs `clerk` under `script -qfc` and writes `\x03` and
    `\x1d`. It covers Ctrl+C releasing, Ctrl+] detaching with the lease kept
    and its holder `User`, and SIGTERM restoring `icanon` (Review Focus 2).
- [ ] **Step 2:** Run `cargo test --test cli --test cli_tty`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test`. Expected: PASS.
- [ ] **Step 5:** Commit: `feat(cli): leasing commands with interactive view and run`

### Task 3: Admin commands behind the gate

**Files:**
- Create: `src/cli/admin.rs`
- Test: `tests/cli_admin.rs`

**Interfaces:**
- Consumes `cli::gate::require` (Task 1) and the phase 1 `admin_*` RPC methods
  and `HolderSpec` (phase 1, Task 14).
- **Commands:**
  `clerk admin <release|evict|dequeue|clear-queue|remove-unit|delete-resource|clean|link|unlink|move|rename|edit-lease|doctor|export|import> … [--yes] [--override REASON]`
  - `remove-unit` and `delete-resource` call the `_force` variants.
  - `export` writes to stdout or `--out FILE`.
  - `import` reads `--in FILE`, with `--replace`.
  - `doctor` takes `--fix`.
- The gate runs before any RPC. `effect` is a plain sentence, e.g.
  "end lease lse_… now and mark unit esp32-c6/b1 dirty".

- [ ] **Step 1:** Write the failing tests for the BDD `admin` subtree (the
  three gate scenarios plus `On success`), and one smoke test per subcommand
  checking that it reaches the daemon and prints the result. They include:
  - `cli_admin::gate::on_non_tty_without_yes::exits_4_and_nothing_changed`
    (Review Focus 1)
  - `cli_admin::gate::on_tty_answer_n::aborts`, under `script`
  - `cli_admin::export_import::round_trip_via_files`
- [ ] **Step 2:** Run `cargo test --test cli_admin`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the full verification:
  `bash -c 'cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test' 2>&1 | tee /tmp/claude-1000/leclerk-p2-full.log`.
  Expected: all clean, all PASS.
- [ ] **Step 5:** Commit: `feat(cli): gated admin commands`

---

## Self-review notes

- **Spec coverage:**
  - §7.3 CLI holder forms (`run`, `acquire`, attach/detach, agent-session
    attribution): Task 2
  - §8.4 CLI (inventory, views, approve): Task 1
  - Admin tools and gates: Task 3
  - Exit codes and output modes: Task 1
- **Deferred:** MCP (phase 3) and the plugin and skills (phase 4).
