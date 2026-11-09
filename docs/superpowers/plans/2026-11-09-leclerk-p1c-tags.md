# Le Clerk: Phase 1c (resource tags) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let agents find resources by what they are. Every resource gets a set
of tags (`esp32c6`, `thread`, `devboard`), `list_resources` filters by them,
and grants and leases carry them.

**Architecture:**
- A new pure module, `src/model/tag.rs`, owns the format: one tag, and a
  sorted, deduplicated, bounded set of tags.
- Tags live in their own table, keyed by the resource's internal row, so
  rename, move and promotion keep them for free and delete cascades.
- The inventory validates tags on every write and on the list filter. Wire
  args carry plain strings so a bad tag reaches the caller as `invalid_tag`,
  not as a generic argument error.
- The scheduler's existing `brief` helper also copies the resource's tags into
  `Grant` and `Lease`.

**Tech Stack:** as phase 1. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-11-07-leclerk-design.md`. §4.8 is new;
§4.1, §8.1, §8.3, §8.4 and §12 are touched. **Prerequisite:** phase 1b is
complete (main at `63e927c` or later).

**Styleguide:** `STYLEGUIDE.md`. It covers formatting, lints, `thiserror`
errors with snake_case codes, test layout (one `mod` per BDD
module/method/scenario, one `#[test]` per effect) and Conventional Commits.
Builds use `CARGO_TARGET_DIR=/tmp/claude-1000/leclerk-target`, because /home
is nearly full.

## Global Constraints

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

1. **An agent guessing the format:** `Thread`, `esp32 c6`, `zigbee,thread` as
   one string, `-x`, or a 33-character tag must be refused with `invalid_tag`
   naming the tag, both on write and as a search filter, never stored or
   silently matching nothing. Covered by `model::tag` "Tag::parse" and
   `inventory::list_resources` "on malformed filter tag".
2. **Lost update on add:** `add_tags: ["thread"]` on a resource tagged
   `esp32, zigbee` must leave all three. Covered by `inventory::update_resource`
   "on add_tags".
3. **Cross-project leak via search:** a project-scope resource tagged `thread`
   in project B never appears in project A's search. Covered by
   `inventory::list_resources` "on match in another project".
4. **Tags surviving identity changes:** `admin rename`, `admin move` and
   promotion approval keep the tags; delete leaves no orphan rows. Covered by
   `admin` "rename / move / approve" and `store` "on resource delete".
5. **The 17th tag:** `add_tags` that would make 17 is refused with
   `too_many_tags` and the existing 16 stay. Covered by
   `inventory::update_resource` "on add beyond the limit".

---

## BDD skeleton

```
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

scheduler
    acquire
        On immediate grant
            - Grant carries resource_tags
    claim / get_lease / my_leases
        On any lease
            - Lease carries resource_tags
        After a tag change
            - get_lease shows the new tags

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

---

### Task 1: `model::tag`, a pure module for tags

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
- [ ] **Step 2:** Run them and confirm they fail: `CARGO_TARGET_DIR=/tmp/claude-1000/leclerk-target cargo test --lib model::tag 2>&1 | tee <scratch>/t1-red.log`.
- [ ] **Step 3:** Implement the module to the Interfaces above.
- [ ] **Step 4:** Run the tests until they pass, then `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings`.
- [ ] **Step 5:** Commit: `feat(model): resource tags`.

### Task 2: Inventory, store, wire and export carry tags; listing filters by them

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
- Consumes: Task 1's `TagSet::parse`, `TagSet::contains_all`, `TagError`.
- Produces: `Resource.tags: TagSet`, the new `list_resources` signature, and the wire names in the Global Constraints.
- **Update order:** validate everything (conflict rules, each tag, the resulting set's size) before writing anything. The resulting set is `tags` when given, else `(current ∪ add_tags) \ remove_tags`.
- **Filtering** may be done in SQL or in Rust after loading the view; either way it happens inside the same transaction and the scope filter is applied first.
- Rename, move and promotion approval keep the row, so they need no code change; Task 2 only adds tests for them.

- [ ] **Step 1:** Write the failing tests for the BDD subtrees `inventory`, `store`, `admin` and `engine wire` (minus the grant and lease JSON, which Task 3 owns). The migration test migrates a fresh database to version 10 (`Migrations::to_version(10)`), inserts a resource, then migrates to latest.
- [ ] **Step 2:** Run them and confirm they fail.
- [ ] **Step 3:** Add the migration, then thread tags through store, model, inventory, command, wire and admin.
- [ ] **Step 4:** Make existing tests compile (mechanical changes only), then run the full suite, fmt and clippy: `CARGO_TARGET_DIR=/tmp/claude-1000/leclerk-target cargo test 2>&1 | tee <scratch>/t2.log`.
- [ ] **Step 5:** Commit: `feat(inventory): resource tags with search filter`.

### Task 3: Grants and leases carry the resource's tags

**Files:**
- Modify:
  - `src/scheduler/mod.rs` (`Grant` and `Lease` gain `resource_tags: TagSet`; `brief` returns a `Brief` struct instead of a tuple, with `resource_description`, `unit_description`, `usage` and `resource_tags`);
  - `src/engine/wire.rs` (`LeaseView.resource_tags`).
- Tests: `tests/scheduler.rs`, `tests/engine.rs`.

**Interfaces:**
- Consumes: Task 2's `Resource.tags`.
- Produces: `Grant { …, resource_tags }`, `Lease { …, resource_tags }`, `LeaseView` mirrors `Lease`; private `struct Brief` and `fn brief(resource: &Resource, unit: &Unit) -> Brief`, still the single source for both the immediate grant and `lease_view`.

- [ ] **Step 1:** Write the failing tests for the BDD subtree `scheduler` and the grant/lease JSON effect under `engine wire`.
- [ ] **Step 2:** Run them and confirm they fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the full verification: `bash -c 'cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test' 2>&1 | tee <scratch>/t3.log` with the same `CARGO_TARGET_DIR`. Expected: all clean, all PASS.
- [ ] **Step 5:** Commit: `feat(scheduler): grants and leases carry resource tags`.

---

## Out of scope

- CLI flags (`--tag`, `--add-tag`, `--remove-tag`, `clerk resources --tag`)
  and tags in `clerk status`: the phase 2 plan, already updated.
- MCP tool wording and skill guidance on tagging: phases 3 and 4.
- Unit tags, "any" matching, prefix or fuzzy search, searching descriptions:
  not requested (spec §4.8).
