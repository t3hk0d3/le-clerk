# Le Clerk: Phase 1b (descriptions and usage) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give agents context about what they lease. Every resource and unit
gets a one-line description, and every resource gets a usage template with
unit placeholders. The template is rendered and handed over on grant.

**Architecture:**
- A new pure module, `src/model/text.rs`, owns the rules:
  - validating descriptions;
  - parsing and checking usage templates against a resource type;
  - rendering a template for one unit.
- The inventory validates text on every write. The store renames the old
  `notes` columns.
- The scheduler renders usage whenever it builds a `Grant` or a `Lease`, so
  every path that shows a holder its lease carries the same brief.

**Tech Stack:** as phase 1. No new dependencies; the placeholder scanner is
hand-written.

**Spec:** `docs/superpowers/specs/2026-11-07-leclerk-design.md`. §4.7 is new;
§4.1, §4.4, §8.1, §8.3 and §8.4 are touched. **Prerequisite:** phase 1 is
complete (main at `5bf634e` or later).

**Styleguide:** `STYLEGUIDE.md`. It covers formatting, lints, `thiserror`
errors with snake_case codes, test layout (one `mod` per BDD
module/method/scenario, one `#[test]` per effect) and Conventional Commits.
Builds use `CARGO_TARGET_DIR=/tmp/claude-1000/leclerk-target`, because /home
is nearly full.

## Global Constraints

- **Description:**
  - at most 200 characters (Unicode scalar values, not bytes);
  - no `\n` or `\r`;
  - empty is allowed and is the default;
  - stored as given, with no trimming.
- **Usage:**
  - at most 4096 bytes (UTF-8);
  - Markdown, never interpreted by the clerk;
  - empty is allowed and is the default.
  - Units have no usage.
- **Placeholder grammar:**
  - exactly `{` + (`resource` | `unit`) + `.` + key + `}`, where the key is one
    or more of `[a-z0-9_]`;
  - all other brace text passes through untouched: `${PORT}`, `{}`,
    `{unit.}`, `{Unit.path}`, `{unit.Path}`, JSON. This means placeholders
    never need escaping.
- **Placeholder keys** (spec §4.7):

  | Type | Keys |
  |------|------|
  | all | `resource.id`, `resource.description`, `unit.id`, `unit.description` |
  | `device` | `unit.path` |
  | `service` | `unit.mode` (`connect` / `listen`), `unit.scheme`, `unit.host`, `unit.port`, `unit.service_id`, `unit.address` |
  | `generic` | `unit.identity_key` |

  - `unit.address` is `host:port`. A host containing `:` (IPv6) is written as
    `[host]:port`.
  - Unset optional settings render as empty text.
- **Write-time check:** every placeholder in a usage template must be a valid
  key for the resource's type; otherwise the write is refused. Resource types
  never change, so a stored template stays valid for life.
- **Wire names:**
  - resource: `description`, `usage` (formerly `notes`);
  - unit: `description` (formerly `notes`);
  - lease and grant: `resource_description`, `unit_description`, and `usage`
    (rendered).
- **Error codes:**
  - `description_too_long`, `description_multiline`;
  - `usage_too_long`;
  - `unknown_placeholder`, with details `{"placeholder": "...", "valid": [...]}`.
- **Visibility:** `SimilarInOtherProject` warnings must stay free of the other
  project's descriptions and usage (spec §4.4).
- **Migration** `0010_description_usage.sql`, append-only:
  - renames `resources.notes` to `usage`;
  - renames `units.notes` to `description`;
  - copies existing data as-is (limits apply only to new writes).
- **Export format:**
  - fields are renamed to match;
  - serde `alias = "notes"` keeps older exports importable: resource `notes`
    becomes `usage`, unit `notes` becomes `description`;
  - imported text is validated like any other write.

## Review Focus

1. **Shell-heavy usage text:** `${PORT}`, `$(…)`, `{}` and JSON must survive
   rendering byte-for-byte. Covered by `text::render` "on non-placeholder
   braces".
2. **A placeholder typo:** `{unit.pth}` on a device resource must be refused
   when written, never left raw at grant time. Covered by `inventory::create_resource`
   "on unknown placeholder".
3. **IPv6 service hosts:** `{unit.address}` for `::1` port 80 is `[::1]:80`.
   Covered by `text::render` "on service unit".
4. **A unit renamed or re-described after grant:** `get_lease` shows the new
   text and nothing breaks. Covered by `scheduler::get_lease` "after
   description change".
5. **Cross-project leak:** a duplicate in another project must not reveal its
   descriptions. Covered by `inventory::add_unit` "on match in other project".

---

## BDD skeleton

```
model::text
    Description::parse(text)
        On empty text
            - returns an empty Description
        On 200 characters, multibyte included
            - returns the Description unchanged
        On 201 characters
            - returns TextError::DescriptionTooLong { max: 200 }
        On text containing \n or \r
            - returns TextError::DescriptionMultiline
    UsageTemplate::parse(text, resource_type)
        On empty text
            - returns an empty template
        On text with valid placeholders for the type
            - returns the template, raw text unchanged
        On 4096 bytes
            - accepts
        On 4097 bytes
            - returns TextError::UsageTooLong { max: 4096 }
        On a key not valid for the type (e.g. {unit.port} on device)
            - returns TextError::UnknownPlaceholder with the placeholder and the sorted valid keys
        On non-placeholder braces (${PORT}, {}, {unit.}, {Unit.path}, {unit.Path})
            - accepts without error
    placeholder_keys(resource_type)
        For each type
            - returns exactly the keys in the Global Constraints table
    UsageTemplate::render(resource, unit)
        On device unit
            - replaces {unit.path} with the unit path
            - replaces {resource.id}, {resource.description}, {unit.id}, {unit.description}
        On service unit
            - renders {unit.address} as host:port
            - renders {unit.address} as [host]:port for an IPv6 host
            - renders {unit.mode} as connect or listen
            - renders unset {unit.scheme} and {unit.service_id} as empty text
        On generic unit
            - replaces {unit.identity_key}
        On non-placeholder braces
            - leaves them byte-for-byte unchanged
        On a placeholder repeated several times
            - replaces every occurrence
        On a substituted value that itself looks like a placeholder
            - does not substitute again (single pass)
    TextError::code / details
        - maps each variant to its snake_case code
        - unknown_placeholder details carry placeholder and valid keys

inventory (Inventory)
    create_resource
        On valid description and usage
            - stores both; get returns the raw template
        On invalid description or usage
            - returns InventoryError::InvalidText(TextError) and stores nothing
        On unknown placeholder
            - is refused with code unknown_placeholder
    update_resource
        On usage patch
            - validates against the resource's type and stores it
        On description patch only
            - leaves usage unchanged
        On any text change
            - records an audit entry
    add_unit / update_unit
        On valid description
            - stores it
        On too-long or multiline description
            - returns InvalidText and stores nothing
        On match in same scope (refused)
            - DuplicateInScope details include the existing resource's and unit's descriptions
        On match with a user-scope unit
            - DuplicateOfUserUnit warning carries resource_description and unit_description
        On match with own-project unit from user scope
            - DuplicateOfProjectUnit warning carries resource_description and unit_description
        On match in other project
            - SimilarInOtherProject warning carries no description or usage fields
    list_promotions
        - each Promotion carries the resource's description

store migration 0010
    On a database at version 9 with notes data
        - resources.usage holds the former resources.notes
        - units.description holds the former units.notes

scheduler
    acquire
        On immediate grant
            - Grant carries resource_description, unit_description and rendered usage
    claim / get_lease / my_leases
        On any lease
            - Lease carries resource_description, unit_description and rendered usage
        After description change
            - get_lease shows the new descriptions and re-rendered usage

engine wire
    create_resource / update_resource / add_unit / update_unit
        On args with description and usage
            - round-trips through RPC into get_resource
        On args with legacy notes
            - is rejected as an unknown field (deny_unknown_fields)
    ResourceView / UnitView / LeaseView / Grant JSON
        - expose the field names in the Global Constraints
    list_resources
        - includes descriptions (usage too: the view is shared; only the CLI trims)

admin export / import
    export
        - writes usage for resources and description for units
    import
        On a current export
            - round-trips identically
        On an older export with notes fields
            - imports resource notes as usage and unit notes as description
        On an imported usage with an unknown placeholder
            - fails with unknown_placeholder and imports nothing
```

---

### Task 1: `model::text`, a pure module for descriptions and usage templates

**Files:**
- Create: `src/model/text.rs`
- Modify: `src/model/mod.rs` (declare and re-export)

**Interfaces:**
- Produces:
  - `pub struct Description(String)` with `parse(text: &str) -> Result<Self, TextError>`, `as_str()`, and `Default`.
  - `pub struct UsageTemplate(String)` with `parse(text: &str, ty: ResourceType) -> Result<Self, TextError>`, `as_str()`, `Default`, and `render(&self, resource: &RenderResource<'_>, unit: &RenderUnit<'_>) -> String`.
  - `pub struct RenderResource<'a> { pub id: &'a ResourceId, pub description: &'a str }`.
  - `pub struct RenderUnit<'a> { pub id: &'a UnitId, pub description: &'a str, pub settings: &'a UnitSettings }`.
  - `pub fn placeholder_keys(ty: ResourceType) -> &'static [&'static str]`. Keys come without braces (e.g. `"unit.path"`), sorted.
  - `pub const DESCRIPTION_MAX_CHARS: usize = 200; pub const USAGE_MAX_BYTES: usize = 4096;`.
  - `pub enum TextError { DescriptionTooLong { max: usize }, DescriptionMultiline, UsageTooLong { max: usize }, UnknownPlaceholder { placeholder: String, valid: Vec<String> } }`. It implements `ErrorCode` with the codes from the Global Constraints, and `details()` for `UnknownPlaceholder`.
- **Placeholder scan (pseudocode).** One pass shared by `parse` and `render`:

  ```
  i = 0
  while i < len:
      if text[i] == '{':
          m = match "(resource|unit)\.[a-z0-9_]+" starting at i+1, followed by '}'
          if m: yield Placeholder(key = m, span = i..end+1); i = end+1; continue
      yield Literal(text[i]); i += 1
  ```

  `parse` checks each placeholder key against `placeholder_keys(ty)`. `render`
  copies literals and appends the value of each placeholder, never rescanning
  inserted values.
- `UnitSettings` already lists its fields per type in `env_vars`. Mirror that
  match for the values, and keep the two in step.

- [ ] **Step 1:** Write the failing unit tests in `mod tests` of `src/model/text.rs`, one per effect under `model::text` in the BDD skeleton.
- [ ] **Step 2:** Run them and confirm they fail: `CARGO_TARGET_DIR=/tmp/claude-1000/leclerk-target cargo test --lib model::text 2>&1 | tee <scratch>/t1-red.log`.
- [ ] **Step 3:** Implement the module to the Interfaces above.
- [ ] **Step 4:** Run the tests until they pass, then run `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings`.
- [ ] **Step 5:** Commit: `feat(model): description and usage template with unit placeholders`.

### Task 2: Inventory, store, wire and export use description and usage

**Files:**
- Create: `src/store/migrations/0010_description_usage.sql`
- Modify:
  - `src/store/mod.rs` (register 0010);
  - `src/store/inventory.rs` (column names and SQL);
  - `src/model/resource.rs` (`Resource.notes` becomes `usage: String`; `Unit.notes` becomes `description: String`);
  - `src/inventory.rs`:
    - `NewResource.notes` becomes `usage`; `ResourcePatch.notes` becomes `usage`;
    - `NewUnit.notes` and `UnitPatch.notes` become `description`;
    - validation on every write;
    - `InventoryError::InvalidText(#[from] TextError)`, which forwards code and details;
    - warning and error payloads carry descriptions;
    - `Promotion.description`;
  - `src/engine/command.rs` (args structs);
  - `src/engine/wire.rs` (`ResourceView`, `UnitView`, `PromotionView`);
  - `src/admin.rs` (`ExportResource.usage` with `#[serde(alias = "notes")]`; `ExportUnit.description` with `#[serde(alias = "notes")]`; import validates through the inventory).
- Tests:
  - `tests/inventory.rs`, `tests/admin_ops.rs`, `tests/engine.rs` for new behaviour;
  - mechanical `notes` renames in `tests/{daemon,interrupt,lifecycle,liveness,notices,scheduler}.rs`.

**Interfaces:**
- Consumes: Task 1's `Description`, `UsageTemplate`, `TextError`.
- Produces:
  - **Stored types:** `Resource { …, description: String, usage: String, … }` and `Unit { …, description: String, … }` store validated text as plain strings. Storage stays stringly typed; validation happens at the inventory boundary.
  - **Warnings:** `Warning::DuplicateOfUserUnit { resource, unit, resource_description: String, unit_description: String }`, and the same new fields on `DuplicateOfProjectUnit`. `SimilarInOtherProject` is unchanged.
  - **Duplicate error:** `InventoryError::DuplicateInScope { resource, unit, resource_description, unit_description }`; `details()` includes both descriptions.
  - **Promotions:** `Promotion { …, description: String }`, the promoted resource's current description, read when listed.
- **Behaviour change:** `update_resource` with a usage patch validates it against the stored resource type.

- [ ] **Step 1:** Write the failing behaviour tests for the BDD subtrees `inventory`, `store migration 0010`, `engine wire` (create/update/add, legacy-notes rejection, field names) and `admin export / import`. The migration test migrates a fresh database to version 9 (`Migrations::to_version(9)`), inserts rows with `notes`, then migrates to latest.
- [ ] **Step 2:** Run them and confirm they fail.
- [ ] **Step 3:** Add the migration, then rename the fields through store, model, inventory, command, wire and admin. Wire `Description::parse` / `UsageTemplate::parse` into create and update for resources and units, and into import.
- [ ] **Step 4:** Make the existing tests compile with the renamed fields; only mechanical renames are allowed there. Then run the full suite, fmt and clippy: `CARGO_TARGET_DIR=/tmp/claude-1000/leclerk-target cargo test 2>&1 | tee <scratch>/t2.log`.
- [ ] **Step 5:** Commit: `feat(inventory): descriptions for resources and units, usage templates for resources`.

### Task 3: Grants and leases carry the brief

**Files:**
- Modify:
  - `src/scheduler/mod.rs` (`Grant` and `Lease` gain `resource_description: String`, `unit_description: String` and `usage: String`, the rendered usage; `lease_view` and the grant path fill them);
  - `src/engine/wire.rs` (`LeaseView` gains the same three fields; the `Grant` serialisation includes them).
- Tests: `tests/scheduler.rs`, `tests/lifecycle.rs`, `tests/engine.rs`.

**Interfaces:**
- Consumes:
  - Task 1's `UsageTemplate::render` with `RenderResource` / `RenderUnit`;
  - Task 2's `Resource.usage` and `Unit.description`.
- **The stored template is trusted.** It was validated at write time, so the
  renderer wraps it without re-checking. Add a crate-internal
  `UsageTemplate::from_stored(String)` in Task 1's module, documented as
  "only for text that passed `parse` when stored".
- Produces:
  - `Grant { …, resource_description, unit_description, usage }`;
  - `Lease { …, resource_description, unit_description, usage }`;
  - `LeaseView` mirrors `Lease`.
- **Rendering happens in one helper,** used by both `lease_view` and the
  immediate-grant path in `acquire`:
  `fn brief(resource: &Resource, unit: &Unit) -> (String, String, String)`.

- [ ] **Step 1:** Write the failing tests for the BDD subtree `scheduler`, plus a daemon-level RPC test in `tests/engine.rs`: acquire on a device resource whose usage is ``"flash: idf.py -p {unit.path} flash; env ${PORT}"`` returns `usage == "flash: idf.py -p /dev/… flash; env ${PORT}"`.
- [ ] **Step 2:** Run them and confirm they fail.
- [ ] **Step 3:** Implement the three fields and the `brief` helper.
- [ ] **Step 4:** Run the full suite, fmt and clippy until they pass.
- [ ] **Step 5:** Commit: `feat(scheduler): grants and leases carry descriptions and rendered usage`.

---

## Out of scope

- CLI flags (`--description`, `--usage`, `--usage-file`) and rendering: the
  phase 2 plan, already updated.
- MCP tool descriptions and skill wording: phases 3 and 4.
- Placeholders for lease fields (e.g. `{lease.id}`): not requested.
