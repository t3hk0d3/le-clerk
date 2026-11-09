# Le Clerk: Phase 2 (tags and CLI) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the `clerk` command-line interface on top of the phase 1 core.
It covers inventory, views, leasing (interactive and non-interactive) and the
gated admin tools, for people, scripts and agents calling it through Bash.
First, Task 1 fixes how a spawned daemon resolves its paths, and Tasks 2–4
add resource tags to the core (spec §4.8) and put descriptions and tags in
the status view, so the CLI ships with them from the start.

**Architecture:**
- **Daemon spawn fix (Task 1):** the launcher hands the spawned daemon
  absolute path variables, so client and daemon agree on the socket and
  database whatever the client's working directory.
- **Tags core (Tasks 2–4):** a pure `src/model/tag.rs` owns the format;
  tags live in their own table keyed by the resource row, so rename, move and
  promotion keep them; the inventory validates them on every write and on the
  `list_resources` filter; the scheduler's `brief` copies them into grants and
  leases. Wire args carry plain strings so a bad tag surfaces as
  `invalid_tag`.
- Every command is a thin client: build a `Caller`, call the daemon through
  `leclerk::client::Client::ensure_running`, render the result.
- **No business rules in `cli/*`.** The CLI owns only:
  - TTY detection and rendering;
  - the interactive live view (raw-mode keys);
  - `clerk run` process supervision;
  - the safety gates (confirmation, `--yes`, `--override`).

**Tech Stack:** as phase 1, plus `script` (util-linux) in tests to get a PTY.

**Spec:** `docs/superpowers/specs/2026-11-07-leclerk-design.md`. **Prerequisite:**
phase 1 and phase 1b are complete (main at `63e927c` or later;
`docs/superpowers/plans/2026-11-07-leclerk-p1-core.md`).
Its Interfaces blocks give the RPC method names and types used here. Its
`STYLEGUIDE.md` applies.

## Global Constraints

All of phase 1's Global Constraints apply. The ones this phase implements:

- **Output:**
  - human-readable when stdout is a TTY; JSON otherwise, or with `--json`;
  - never prompt or read stdin unless stdin is a TTY, with one exception:
    `-f -` on the inventory commands reads a YAML document from stdin because
    the caller asked for it explicitly;
  - errors in JSON mode: `{"error":{"code":"<snake_case>","message":"…","details":{…}}}`;
    `details` is present exactly when the daemon's error carries details
    (e.g. `unknown_placeholder`, `duplicate_in_scope`, `invalid_tag`), copied
    unchanged; human mode prints the message, then each detail as
    `  key: value` lines.
- **Exit codes:**
  - 0: ok
  - 2: usage error
  - 3: not found
  - 4: refused or conflict (including `no_units`: the resource has no units;
    the message says to add one)
  - 5: daemon unavailable, unresponsive (`daemon_unresponsive`, message names
    the pid) or a different version (`version_mismatch`)
  - 6: queued, when called with `--no-wait`
  - 130: an interactive `acquire` or `attach` ended by Ctrl+C (request
    cancelled, offer withdrawn, or lease released)
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

**Tags core (spec §4.8; Tasks 2–4):**

- **Tag format** (spec §4.8):
  - lowercase ASCII letters, digits and `-`;
  - first character a letter or digit;
  - 1 to 32 characters;
  - anything else is refused, uppercase included (never lowercased).
- **Tag set:**
  - at most 16 tags per resource, counted after deduplication;
  - duplicates collapse to one;
  - stored and returned sorted;
  - empty is allowed and is the default.
- **Search:** `list_resources` with tags returns the resources in the caller's
  view that carry **all** of them; no tags means no filter. Matching is exact.
  A malformed filter tag is refused with `invalid_tag` (it could never match).
- **Editing:**
  - create takes `tags`;
  - update takes either `tags` (replace the whole set; `[]` clears) or
    `add_tags` / `remove_tags`, never both kinds in one call;
  - a tag in both `add_tags` and `remove_tags` is refused;
  - removing a tag that is not there is a no-op;
  - the 16 limit applies to the resulting set; on any refusal nothing changes.
- **Wire names:**
  - resource view, export: `tags` (array of strings, sorted);
  - create args: `tags`; update args: `tags`, `add_tags`, `remove_tags`;
  - `list_resources` args: `view`, `tags`;
  - grant and lease: `resource_tags`.
- **Error codes:**
  - `invalid_tag`, details `{"tag": "<as given>"}`;
  - `too_many_tags`, details `{"max": 16, "count": <n>}`;
  - `conflicting_tag_edit`, details `{"reason": "replace_with_add_or_remove" | "added_and_removed", "tag"?: "<tag>"}`.
- **Visibility:** the filter runs inside the caller's view; other projects'
  resources never match (spec §4.2). Duplicate warnings and errors carry no
  tags.
- **Audit:** `resource_created` and `resource_updated` details include the
  resulting `tags`.
- **Migration** `0011_resource_tags.sql`, append-only: table
  `resource_tags (resource_row INTEGER NOT NULL REFERENCES resources (row_id) ON DELETE CASCADE, tag TEXT NOT NULL, PRIMARY KEY (resource_row, tag))`
  plus an index on `tag`. Existing resources get no tags.
- **Export format:** `ExportResource.tags` with `#[serde(default)]`, so older
  exports import with no tags. Imported tags are validated like any other
  write.

## Review Focus

1. **An agent or script runs `clerk acquire` or an admin command without a
   TTY.** It must never wait for keyboard input or hang on a prompt. → Tests in
   Tasks 5, 6 and 7.
2. **The interactive view exits abnormally** (an error from the daemon, a
   SIGTERM, a panic). The terminal must come back in cooked mode. → Test in
   Task 6: kill `clerk acquire` with SIGTERM under `script`, then assert that
   `stty -a` shows `icanon`.
3. **Bad durations:** `--for 0`, `-5m`, `abc` or `100000d`. Each must exit 2
   with a message naming the accepted format, and nothing may reach the daemon.
   → Tests in Task 6.
4. **The daemon can't start** (e.g. the state directory isn't writable). The
   command must exit 5 within 6 s with a plain message, never hang. → Test in
   Task 5.
5. **The working directory has spaces or unicode in its path,** or is outside
   any repo. The project must resolve and commands must work. → Test in Task 5.
6. **An agent guessing the tag format:** `Thread`, `esp32 c6`,
   `zigbee,thread` as one string, `-x`, or a 33-character tag must be refused
   with `invalid_tag` naming the tag, on write and as a search filter, never
   stored or silently matching nothing. → Tests in Tasks 2 and 3.
7. **Lost update on add:** `add_tags: ["thread"]` on a resource tagged
   `esp32, zigbee` must leave all three. → Test in Task 3.
8. **Cross-project leak via search:** a project resource tagged `thread` in
   project B never appears in project A's search. → Test in Task 3.
9. **A relative `CLERK_HOME` or XDG directory:** `CLERK_HOME=state clerk
   status` run from `/tmp/x` must start a daemon that listens under
   `/tmp/x/state/run`, and a second call from the same directory must reach
   it, not spawn another. → Test in Task 1.

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
        On an unresponsive daemon (lock held, no answer)
            - exit 5, code daemon_unresponsive, message names the pid
        On a version mismatch
            - exit 5, code version_mismatch, message says `clerk daemon restart`
    error output
        On a daemon error with details (e.g. unknown_placeholder)
            - JSON: error.details equals the daemon's details
            - human: message, then one `key: value` line per detail
        On a daemon error without details
            - JSON has no details key
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
        On a resource with no units
            - exit 4, code no_units, message says to add a unit
        On grant (any mode)
            - prints resource and unit descriptions, tags and the rendered usage
        On Ctrl+C after an Offered notice arrived but before the claim answered
            - calls cancel(request): the offer is withdrawn (lease ends Unclaimed), never release
            - exits 130 with nothing held
    attach <lease>
        - calls get_lease first and prints the brief (descriptions, tags, rendered usage)
        - shows live view; Ctrl+C releases; Ctrl+] detaches again
        On an unknown, ended or someone else's lease
            - exit 3; nothing changes
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
            - release { failed: true }: lease ends now, unit dirty(command_failed),
              no cleanup window; exits with cmd's code
        On override / admin release of its lease
            - SIGTERM to cmd at cleanup deadline
    wait
        - prints notices for the caller as they arrive (long-poll loop)
        - --once returns after the first batch
    status / resources / leases / queue / log / promotions / approve
        - read views; invisible holders shown as "held by an agent in another project"
        - status shows each resource's description and tags and each unit's description
          (human mode truncates descriptions to the terminal width; JSON is complete)
    leases --mine
        - calls list_my_leases for the caller's holder and prints each lease with its brief
        - without --mine: all leases from the status view (the user's view)
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

model::tag
    Tag::parse(text)
        On a valid tag (esp32c6, thread, 2-4ghz, 32 chars)
            - returns the Tag; as_str() equals the input
        On empty, 33 chars, uppercase, space, comma, underscore, leading '-', non-ASCII
            - returns TagError::Invalid { tag } carrying the input as given
    TagSet::parse(texts)
        On an empty list
            - returns an empty set
        On duplicates and unsorted input
            - returns each tag once, sorted
        On 16 distinct tags (with duplicates in the input)
            - accepts
        On 17 distinct tags
            - returns TagError::TooMany { max: 16, count: 17 }
        On one malformed entry
            - returns TagError::Invalid for that entry
    TagSet::contains_all(filter)
        On an empty filter
            - returns true
        On a filter that is a subset
            - returns true
        On a filter with one missing tag
            - returns false
    TagSet serialisation
        - serialises as a sorted JSON array of strings
    TagError::code / details
        - maps Invalid to invalid_tag with {tag}
        - maps TooMany to too_many_tags with {max, count}

inventory (Inventory)
    create_resource
        On valid tags
            - stores them; get_resource returns them sorted and deduplicated
        On a malformed tag
            - returns InventoryError::InvalidTag and stores nothing
        On 17 tags
            - returns InventoryError::InvalidTag(TooMany) and stores nothing
        On success
            - the resource_created audit entry carries the tags
    update_resource
        On tags (replace)
            - the stored set equals the new one
        On tags: []
            - clears all tags
        On add_tags
            - keeps the existing tags and adds the new ones
        On remove_tags, including a tag not present
            - removes the present ones; the absent one is a no-op
        On add beyond the limit
            - returns too_many_tags and the stored set is unchanged
        On tags together with add_tags or remove_tags
            - returns ConflictingTagEdit (replace_with_add_or_remove), nothing changes
        On a tag in both add_tags and remove_tags
            - returns ConflictingTagEdit (added_and_removed) naming the tag, nothing changes
        On no tag fields
            - leaves the tags unchanged
        On success
            - the resource_updated audit entry carries the resulting tags
    list_resources(view, tags)
        On no tags
            - returns every resource in the view, as before
        On one tag
            - returns only resources carrying it
        On several tags
            - returns only resources carrying all of them
        On a tag nobody has
            - returns an empty list
        On match in another project
            - the other project's resource is not returned
        On View::All (admin)
            - filters across every scope
        On malformed filter tag
            - returns InvalidTag; no listing
    add_unit / update_unit duplicate payloads
        - DuplicateInScope, DuplicateOfUserUnit, DuplicateOfProjectUnit, SimilarInOtherProject carry no tags

store
    migration 0011
        On a database at version 10 with resources
            - those resources have no tags after migrating
    On resource delete
        - its resource_tags rows are gone

admin
    rename / move / approve promotion
        - the resource keeps its tags
    export
        - writes tags per resource
    import
        On a current export
            - round-trips tags identically
        On an export without tags fields
            - imports resources with no tags
        On an imported malformed tag
            - fails with invalid_tag and imports nothing

client::Launcher
    spawn (via Client::ensure_running)
        On a relative CLERK_HOME
            - the daemon's socket, lock and database are under <client cwd>/<CLERK_HOME>
            - a second client from the same cwd connects to it; no second daemon
        On relative XDG_STATE_HOME / XDG_CONFIG_HOME / XDG_RUNTIME_DIR (no CLERK_HOME)
            - the same, per variable
        On absolute variables
            - passed through unchanged
        On the daemon's own working directory
            - is still /

scheduler
    acquire
        On immediate grant
            - Grant carries resource_tags
    claim / get_lease / my_leases
        On any lease
            - Lease carries resource_tags
        After a tag change
            - get_lease shows the new tags
    status
        - each ResourceStatus carries description and tags
        - each UnitStatus carries description
        - other projects' holders stay "held by an agent in another project" (unchanged)

engine wire
    create_resource / update_resource
        On args with tags / add_tags / remove_tags
            - round-trip through RPC into get_resource
        On a malformed tag
            - the RPC error code is invalid_tag (not a generic args error)
    list_resources
        On args { tags: [...] }
            - returns only matching resources
        On args { view } only
            - behaves as before
    ResourceView / LeaseView / Grant JSON
        - expose tags / resource_tags as sorted string arrays
```

## File structure

```
src/client.rs         (modify) Launcher::spawn passes absolute paths (Task 1)
src/model/tag.rs      Tag / TagSet format rules (Task 2)
src/store/migrations/0011_resource_tags.sql  (Task 3)
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

### Task 1: The spawned daemon gets absolute paths

**Files:**
- Modify: `src/client.rs` (`Launcher::spawn`)
- Test: `tests/daemon.rs`

**Interfaces:**
- Consumes: `config::Paths::from_lookup` (already makes `CLERK_HOME`
  absolute for the client) and `Launcher`.
- **The bug:** the client resolves a relative `CLERK_HOME` against its own
  working directory, but the daemon is spawned with `current_dir("/")` and
  the raw environment, so it resolves the same value against `/` and listens
  somewhere else. The client then times out (exit 5) or a later client spawns
  a second daemon.
- **The fix:** `spawn` sets each of `CLERK_HOME`, `HOME`, `XDG_STATE_HOME`,
  `XDG_CONFIG_HOME` and `XDG_RUNTIME_DIR` that is set and relative in the
  child's environment to its absolute form (`std::path::absolute`, against
  the client's working directory). Empty and absolute values pass through.
  The daemon keeps `current_dir("/")`.
- Produces: no API change.

- [ ] **Step 1:** Write the failing tests for the BDD subtree
  `client::Launcher`. Each runs the real binary from a temporary working
  directory with the variable relative, then asserts the socket path and
  that a second `clerk daemon status` from the same directory reports the same
  pid.
- [ ] **Step 2:** Run `CARGO_TARGET_DIR=/tmp/claude-1000/leclerk-target cargo test --test daemon 2>&1 | tee <scratch>/p2-t1-red.log`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the full suite, fmt and clippy until they pass.
- [ ] **Step 5:** Commit: `fix(client): spawned daemon resolves relative path variables like its client`

### Task 2: Tags core: `model::tag`, a pure module for tags

**Files:**
- Create: `src/model/tag.rs`
- Modify: `src/model/mod.rs` (declare and re-export `Tag`, `TagSet`, `TagError`, `TAG_MAX_CHARS`, `TAGS_MAX`)

**Interfaces:**
- Produces:
  - `pub struct Tag(String)`: `parse(text: &str) -> Result<Self, TagError>`, `as_str()`, `Ord`, `Serialize` as a string.
  - `pub struct TagSet(BTreeSet<Tag>)`: `parse<I: IntoIterator<Item = S>, S: AsRef<str>>(texts: I) -> Result<Self, TagError>`, `iter()`, `len()`, `is_empty()`, `contains_all(&self, filter: &TagSet) -> bool`, `Default`, `Serialize` as a sorted array.
  - `pub(crate) TagSet::from_stored(Vec<String>) -> Self` for rows read from the store, documented as "only for text that passed `parse` when stored".
  - `pub const TAG_MAX_CHARS: usize = 32; pub const TAGS_MAX: usize = 16;`
  - `pub enum TagError { Invalid { tag: String }, TooMany { max: usize, count: usize } }`, implementing `ErrorCode` with the codes and details from the Global Constraints.
- Tag validation is a plain character check, no regex dependency.

- [ ] **Step 1:** Write the failing unit tests in `mod tests` of `src/model/tag.rs`, one per effect under `model::tag` in the BDD skeleton.
- [ ] **Step 2:** Run them and confirm they fail: `CARGO_TARGET_DIR=/tmp/claude-1000/leclerk-target cargo test --lib model::tag 2>&1 | tee <scratch>/p2-t2-red.log`.
- [ ] **Step 3:** Implement the module to the Interfaces above.
- [ ] **Step 4:** Run the tests until they pass, then `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings`.
- [ ] **Step 5:** Commit: `feat(model): resource tags`.

### Task 3: Tags core: inventory, store, wire and export carry tags; listing filters by them

**Files:**
- Create: `src/store/migrations/0011_resource_tags.sql`
- Modify:
  - `src/store/mod.rs` (register 0011);
  - `src/store/inventory.rs` (`set_tags(tx, row, &TagSet)` replacing the row's tags; resource loading fills `tags`);
  - `src/model/resource.rs` (`Resource.tags: TagSet`);
  - `src/inventory.rs`:
    - `NewResource.tags: Vec<String>`;
    - `ResourcePatch { …, tags: Option<Vec<String>>, add_tags: Vec<String>, remove_tags: Vec<String> }`;
    - `list_resources(&self, tx, who, view: View, tags: &[String]) -> Result<Vec<Resource>, InventoryError>`;
    - `InventoryError::InvalidTag(#[from] TagError)` and `InventoryError::ConflictingTagEdit { reason: &'static str, tag: Option<String> }`, both forwarding code and details;
    - audit details include `tags`;
  - `src/engine/command.rs` (`CreateResourceArgs.tags`, `UpdateResourceArgs.{tags, add_tags, remove_tags}` as strings with `#[serde(default)]`; new `ListResourcesArgs { view, tags }` for `list_resources` only);
  - `src/engine/wire.rs` (`ResourceView.tags`);
  - `src/admin.rs` (`ExportResource.tags` with `#[serde(default)]`; export writes them; import passes them to `create_resource`).
- Tests: `tests/inventory.rs`, `tests/admin_ops.rs`, `tests/engine.rs`; mechanical updates where tests build `NewResource`, `ResourcePatch` or call `list_resources`.

**Interfaces:**
- Consumes: Task 2's `TagSet::parse`, `TagSet::contains_all`, `TagError`.
- Produces: `Resource.tags: TagSet`, the new `list_resources` signature, and the wire names in the Global Constraints.
- **Update order:** validate everything (conflict rules, each tag, the resulting set's size) before writing anything. The resulting set is `tags` when given, else `(current ∪ add_tags) \ remove_tags`.
- **Filtering** may be done in SQL or in Rust after loading the view; either way it happens inside the same transaction and the scope filter is applied first.
- Rename, move and promotion approval keep the row, so they need no code change; Task 3 only adds tests for them.

- [ ] **Step 1:** Write the failing tests for the BDD subtrees `inventory`, `store`, `admin` and `engine wire` (minus the grant and lease JSON, which Task 4 owns). The migration test migrates a fresh database to version 10 (`Migrations::to_version(10)`), inserts a resource, then migrates to latest.
- [ ] **Step 2:** Run them and confirm they fail.
- [ ] **Step 3:** Add the migration, then thread tags through store, model, inventory, command, wire and admin.
- [ ] **Step 4:** Make existing tests compile (mechanical changes only), then run the full suite, fmt and clippy: `CARGO_TARGET_DIR=/tmp/claude-1000/leclerk-target cargo test 2>&1 | tee <scratch>/p2-t2.log`.
- [ ] **Step 5:** Commit: `feat(inventory): resource tags with search filter`.

### Task 4: Tags core: grants, leases and status carry tags and descriptions

**Files:**
- Modify:
  - `src/scheduler/mod.rs` (`Grant` and `Lease` gain `resource_tags: TagSet`; `brief` returns a `Brief` struct instead of a tuple, with `resource_description`, `unit_description`, `usage` and `resource_tags`);
  - `src/scheduler/mod.rs` also: `ResourceStatus` gains `description: String` and `tags: TagSet`; `UnitStatus` gains `description: String`;
  - `src/engine/wire.rs` (`LeaseView.resource_tags`; status JSON carries the new fields).
- Tests: `tests/scheduler.rs`, `tests/engine.rs`.

**Interfaces:**
- Consumes: Task 3's `Resource.tags`.
- Produces: `Grant { …, resource_tags }`, `Lease { …, resource_tags }`, `LeaseView` mirrors `Lease`; private `struct Brief` and `fn brief(resource: &Resource, unit: &Unit) -> Brief`, still the single source for both the immediate grant and `lease_view`.

- [ ] **Step 1:** Write the failing tests for the BDD subtree `scheduler` (including `status`) and the grant/lease JSON effect under `engine wire`.
- [ ] **Step 2:** Run them and confirm they fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the full verification: `bash -c 'cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test' 2>&1 | tee <scratch>/p2-t3.log` with the same `CARGO_TARGET_DIR`. Expected: all clean, all PASS.
- [ ] **Step 5:** Commit: `feat(scheduler): grants, leases and status carry tags and descriptions`.

### Task 5: CLI framework, inventory and view commands

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
  - `leases [--mine]`: `--mine` calls `list_my_leases` for the caller's holder
  - Error rendering: `Output::fail` copies `details` from `ApiError` into JSON
    and prints them as `key: value` lines in human mode; it maps
    `daemon_unresponsive` and `version_mismatch` to exit 5 and `no_units` to
    exit 4
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
  - `cli::error_output::on_unknown_placeholder::json_details_carry_valid_keys`
  - `cli::daemon_unavailable::on_unresponsive_daemon::exits_5_naming_pid`
  - `cli::status::shows_descriptions_and_tags`
  - `cli::yaml_input::on_heredoc_usage_block::matches_flag_equivalent`
  - `cli::yaml_input::on_flag_and_key_both_set::exits_2_naming_field`
  - `cli::yaml_input::on_numeric_tag::exits_2_asking_to_quote`
- [ ] **Step 2:** Run `cargo test --test cli 2>&1 | tee /tmp/claude-1000/leclerk-cli.log`.
  Expected: FAIL.
- [ ] **Step 3:** Implement. Human rendering is plain aligned text, with no
  colour library.
- [ ] **Step 4:** Run `cargo test`. Expected: PASS.
- [ ] **Step 5:** Commit: `feat(cli): framework, inventory and views`

### Task 6: Leasing commands: acquire, attach, release, extend, run, wait

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
  - Ctrl+C while queued cancels; while an offer is pending (notice seen,
    claim not yet answered) it also calls `cancel`, which withdraws the offer;
    once held it releases (confirm Clean);
  - on grant, and on `attach`, prints the brief: both descriptions, the
    resource's tags and the rendered usage;
  - Ctrl+] calls `rehome_holder` with `User { tag }`, prints the lease ID, and
    exits 0.
- **`attach <lease>`** calls `get_lease` first; `not_found` (unknown, ended
  or someone else's lease) exits 3.
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
  - on child exit 0, releases with confirm Clean (exit 0); on any other exit,
    calls `release` with `failed: true` (lease ends now, unit dirty
    `command_failed`) and exits with the child's code.

- [ ] **Step 1:** Write the failing tests for the BDD `CLI` subtrees `acquire`,
  `attach`, `release`, `extend`, `run`, `wait` and the `--override` agent gate.
  They include:
  - `cli::acquire::on_stdin_not_tty::prints_queued_and_exits_6_without_reading_stdin`
    (Review Focus 1)
  - `cli::acquire::on_invalid_for::exits_2_for_each_of_0_neg_abc_100000d`
    (Review Focus 3)
  - `cli::run::on_cmd_exit_3::unit_dirty_command_failed_and_exit_3`
  - `cli::acquire::on_ctrl_c_with_offer_pending::offer_withdrawn_nothing_held` (in `tests/cli_tty.rs`)
  - `cli::acquire::on_no_units::exits_4_code_no_units`
  - `cli::attach::on_ended_lease::exits_3`
  - `cli::acquire::on_grant::prints_brief`
  - `cli::run::on_cmd::receives_clerk_unit_env`
  - `cli::acquire::on_session_env::lease_owned_by_agent_session`
  - `tests/cli_tty.rs` runs `clerk` under `script -qfc` and writes `\x03` and
    `\x1d`. It covers Ctrl+C releasing, Ctrl+] detaching with the lease kept
    and its holder `User`, and SIGTERM restoring `icanon` (Review Focus 2).
- [ ] **Step 2:** Run `cargo test --test cli --test cli_tty`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test`. Expected: PASS.
- [ ] **Step 5:** Commit: `feat(cli): leasing commands with interactive view and run`

### Task 7: Admin commands behind the gate

**Files:**
- Create: `src/cli/admin.rs`
- Test: `tests/cli_admin.rs`

**Interfaces:**
- Consumes `cli::gate::require` (Task 5) and the phase 1 `admin_*` RPC methods
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
    attribution): Task 6
  - §8.4 CLI (inventory, views, approve, tag flags, YAML input): Task 5
  - Admin tools and gates: Task 7
  - Exit codes and output modes: Task 5
  - §4.8 tags in the core (model, inventory, store, wire, export, grants):
    Tasks 2–4
  - §4.7 descriptions and usage in `clerk status`, and the brief on
    `acquire`/`attach`: Tasks 4, 6
  - Phase 1 follow-ups: relative path variables for a spawned daemon (Task 1);
    `release { failed }`, `get_lease`, `list_my_leases`, error `details`,
    `no_units`, `daemon_unresponsive`, withdrawing an offer on Ctrl+C
    (Tasks 5, 6)
- **Deferred:** MCP (phase 3) and the plugin and skills (phase 4).
