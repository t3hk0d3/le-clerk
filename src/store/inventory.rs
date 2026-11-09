//! SQL for resources and units.

use std::path::PathBuf;

use rusqlite::{OptionalExtension, Row};
use serde_json::json;

use crate::dedup::Identity;
use crate::model::{
    Caller, DirtyInfo, HealthSpec, ProjectKey, PromotionId, Resource, ResourceId, ResourceType,
    Scope, TagSet, Unit, UnitId, UnitSettings,
};
use crate::store::{StoreError, Tx, audit, enum_text, parse_enum};

/// Which resources a query covers.
#[derive(Debug, Clone, Copy)]
pub enum ScopeFilter<'a> {
    /// Every scope.
    All,
    /// User scope plus, when given, one project.
    Visible(Option<&'a ProjectKey>),
}

/// Values for a new resource row.
#[derive(Debug)]
pub struct NewResourceRow<'a> {
    pub id: &'a ResourceId,
    pub ty: ResourceType,
    pub scope: &'a Scope,
    pub description: &'a str,
    pub usage: &'a str,
    pub health: Option<&'a HealthSpec>,
    pub created_at: i64,
}

/// Values for a new unit row.
#[derive(Debug)]
pub struct NewUnitRow<'a> {
    pub resource_row: i64,
    pub id: &'a UnitId,
    pub settings: &'a UnitSettings,
    pub description: &'a str,
    pub created_at: i64,
}

fn scope_columns(scope: &Scope) -> (&'static str, String) {
    match scope {
        Scope::User => ("user", String::new()),
        Scope::Project(key) => ("project", key.0.to_string_lossy().into_owned()),
    }
}

fn scope_from_columns(kind: &str, key: String) -> Result<Scope, StoreError> {
    match kind {
        "user" => Ok(Scope::User),
        "project" => Ok(Scope::Project(ProjectKey(PathBuf::from(key)))),
        _ => Err(StoreError::Corrupt("scope_kind")),
    }
}

const RESOURCE_COLUMNS: &str = "r.row_id, r.id, r.scope_kind, r.project_key, r.type, \
     r.description, r.usage, r.health, r.created_at, \
     (r.scope_kind = 'project' AND EXISTS (SELECT 1 FROM resources u \
        WHERE u.scope_kind = 'user' AND u.id = r.id))";

struct RawResource {
    row: i64,
    id: String,
    scope_kind: String,
    project_key: String,
    ty: String,
    description: String,
    usage: String,
    health: Option<String>,
    created_at: i64,
    shadows_user: bool,
}

fn raw_resource(r: &Row<'_>) -> rusqlite::Result<RawResource> {
    Ok(RawResource {
        row: r.get(0)?,
        id: r.get(1)?,
        scope_kind: r.get(2)?,
        project_key: r.get(3)?,
        ty: r.get(4)?,
        description: r.get(5)?,
        usage: r.get(6)?,
        health: r.get(7)?,
        created_at: r.get(8)?,
        shadows_user: r.get(9)?,
    })
}

fn build_resource(tx: &Tx, raw: RawResource) -> Result<Resource, StoreError> {
    let scope = scope_from_columns(&raw.scope_kind, raw.project_key)?;
    Ok(Resource {
        row: raw.row,
        id: ResourceId::parse(&raw.id).map_err(|_| StoreError::Corrupt("resources.id"))?,
        ty: parse_enum(&raw.ty, "resources.type")?,
        scope,
        description: raw.description,
        usage: raw.usage,
        health: raw.health.map(|h| serde_json::from_str(&h)).transpose()?,
        tags: load_tags(tx, raw.row)?,
        created_at: raw.created_at,
        shadows_user: raw.shadows_user,
        units: load_units(tx, raw.row)?,
    })
}

/// The tags of a resource, sorted.
fn load_tags(tx: &Tx, row: i64) -> Result<TagSet, StoreError> {
    let tags = tx
        .conn()
        .prepare("SELECT tag FROM resource_tags WHERE resource_row = ?1 ORDER BY tag")?
        .query_map([row], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(TagSet::from_stored(tags))
}

/// Replace the tags of a resource with `tags`.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn set_tags(tx: &Tx, row: i64, tags: &TagSet) -> Result<(), StoreError> {
    tx.conn()
        .execute("DELETE FROM resource_tags WHERE resource_row = ?1", [row])?;
    let mut insert = tx
        .conn()
        .prepare("INSERT INTO resource_tags (resource_row, tag) VALUES (?1, ?2)")?;
    for tag in tags.iter() {
        insert.execute(rusqlite::params![row, tag.as_str()])?;
    }
    Ok(())
}

/// Insert a resource; returns its row id.
///
/// # Errors
/// [`StoreError`] on SQL failure, including a unique-index violation.
pub fn insert_resource(tx: &Tx, new: &NewResourceRow<'_>) -> Result<i64, StoreError> {
    let (kind, key) = scope_columns(new.scope);
    tx.conn().execute(
        "INSERT INTO resources \
         (id, scope_kind, project_key, type, description, usage, health, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            new.id.as_str(),
            kind,
            key,
            enum_text(&new.ty)?,
            new.description,
            new.usage,
            new.health.map(serde_json::to_string).transpose()?,
            new.created_at,
        ],
    )?;
    Ok(tx.conn().last_insert_rowid())
}

/// The row id of the resource `id` in exactly `scope`.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn find_in_scope(tx: &Tx, scope: &Scope, id: &ResourceId) -> Result<Option<i64>, StoreError> {
    let (kind, key) = scope_columns(scope);
    Ok(tx
        .conn()
        .query_row(
            "SELECT row_id FROM resources WHERE scope_kind = ?1 AND project_key = ?2 AND id = ?3",
            rusqlite::params![kind, key, id.as_str()],
            |r| r.get(0),
        )
        .optional()?)
}

/// The row id `id` resolves to for a caller in `project`: the project's
/// resource first, then the user's.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn find_visible(
    tx: &Tx,
    project: Option<&ProjectKey>,
    id: &ResourceId,
) -> Result<Option<i64>, StoreError> {
    if let Some(key) = project
        && let Some(row) = find_in_scope(tx, &Scope::Project(key.clone()), id)?
    {
        return Ok(Some(row));
    }
    find_in_scope(tx, &Scope::User, id)
}

/// Load one resource with its units.
///
/// # Errors
/// [`StoreError`] on SQL failure or a damaged row.
pub fn load_resource(tx: &Tx, row: i64) -> Result<Resource, StoreError> {
    let raw = tx.conn().query_row(
        &format!("SELECT {RESOURCE_COLUMNS} FROM resources r WHERE r.row_id = ?1"),
        [row],
        raw_resource,
    )?;
    build_resource(tx, raw)
}

/// Load all resources matching `filter`, ordered by id then scope.
///
/// # Errors
/// [`StoreError`] on SQL failure or a damaged row.
pub fn list_resources(tx: &Tx, filter: ScopeFilter<'_>) -> Result<Vec<Resource>, StoreError> {
    let sql = format!(
        "SELECT {RESOURCE_COLUMNS} FROM resources r \
         WHERE ?1 OR r.scope_kind = 'user' OR (r.scope_kind = 'project' AND r.project_key = ?2) \
         ORDER BY r.id, r.scope_kind, r.project_key"
    );
    let (all, key) = match filter {
        ScopeFilter::All => (true, String::new()),
        ScopeFilter::Visible(project) => (
            false,
            // An impossible key keeps the project branch from matching.
            project.map_or_else(|| "\0".to_owned(), |k| k.0.to_string_lossy().into_owned()),
        ),
    };
    let raws = tx
        .conn()
        .prepare(&sql)?
        .query_map(rusqlite::params![all, key], raw_resource)?
        .collect::<Result<Vec<_>, _>>()?;
    raws.into_iter()
        .map(|raw| build_resource(tx, raw))
        .collect()
}

/// Overwrite the mutable fields of a resource.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn update_resource(
    tx: &Tx,
    row: i64,
    description: &str,
    usage: &str,
    health: Option<&HealthSpec>,
) -> Result<(), StoreError> {
    tx.conn().execute(
        "UPDATE resources SET description = ?2, usage = ?3, health = ?4 WHERE row_id = ?1",
        rusqlite::params![
            row,
            description,
            usage,
            health.map(serde_json::to_string).transpose()?
        ],
    )?;
    Ok(())
}

/// Delete a resource; its leases and requests cascade. Its units are
/// deleted one by one through [`delete_unit`] first, so identity groups they
/// bridge stay whole. `at` and `who` stamp the `group_preserved` audit.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn delete_resource(tx: &Tx, row: i64, at: i64, who: &Caller) -> Result<(), StoreError> {
    let mut stmt = tx
        .conn()
        .prepare("SELECT row_id FROM units WHERE resource_row = ?1 ORDER BY row_id")?;
    let units = stmt
        .query_map([row], |r| r.get::<_, i64>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for unit in units {
        delete_unit(tx, unit, at, who)?;
    }
    tx.conn()
        .execute("DELETE FROM resources WHERE row_id = ?1", [row])?;
    Ok(())
}

/// True when any unit of the resource has a live lease or the resource has a
/// queued request.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn resource_busy(tx: &Tx, row: i64) -> Result<bool, StoreError> {
    Ok(tx.conn().query_row(
        "SELECT EXISTS (SELECT 1 FROM leases WHERE resource_row = ?1 AND state != 'ended') \
             OR EXISTS (SELECT 1 FROM requests WHERE resource_row = ?1 AND state = 'queued')",
        [row],
        |r| r.get(0),
    )?)
}

/// True when the unit has a live lease or a queued request pinned to it.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn unit_busy(tx: &Tx, unit_row: i64) -> Result<bool, StoreError> {
    Ok(tx.conn().query_row(
        "SELECT EXISTS (SELECT 1 FROM leases WHERE unit_row = ?1 AND state != 'ended') \
             OR EXISTS (SELECT 1 FROM requests WHERE unit_row = ?1 AND state = 'queued')",
        [unit_row],
        |r| r.get(0),
    )?)
}

const UNIT_COLUMNS: &str = "row_id, resource_row, id, settings, description, dirty_reason, dirty_at, \
     dirty_prev_holder, retiring, last_health, last_health_at, created_at";

fn unit_from_row(r: &Row<'_>) -> rusqlite::Result<Result<Unit, StoreError>> {
    let row: i64 = r.get(0)?;
    let resource_row: i64 = r.get(1)?;
    let id: String = r.get(2)?;
    let settings: String = r.get(3)?;
    let description: String = r.get(4)?;
    let dirty_reason: Option<String> = r.get(5)?;
    let dirty_at: Option<i64> = r.get(6)?;
    let dirty_prev: Option<String> = r.get(7)?;
    let retiring: bool = r.get(8)?;
    let last_health: Option<String> = r.get(9)?;
    let last_health_at: Option<i64> = r.get(10)?;
    let created_at: i64 = r.get(11)?;
    Ok((|| {
        let dirty = match (dirty_reason, dirty_at, dirty_prev) {
            (Some(reason), Some(at), Some(prev)) => Some(DirtyInfo {
                reason: parse_enum(&reason, "units.dirty_reason")?,
                at,
                previous_holder: serde_json::from_str(&prev)?,
            }),
            _ => None,
        };
        Ok(Unit {
            row,
            resource_row,
            id: UnitId::parse(&id).map_err(|_| StoreError::Corrupt("units.id"))?,
            settings: serde_json::from_str(&settings)?,
            description,
            dirty,
            retiring,
            last_health,
            last_health_at,
            created_at,
        })
    })())
}

fn load_units(tx: &Tx, resource_row: i64) -> Result<Vec<Unit>, StoreError> {
    let mut stmt = tx.conn().prepare(&format!(
        "SELECT {UNIT_COLUMNS} FROM units WHERE resource_row = ?1 ORDER BY id"
    ))?;
    let units = stmt
        .query_map([resource_row], unit_from_row)?
        .collect::<Result<Vec<_>, _>>()?;
    units.into_iter().collect()
}

/// The unit `id` of a resource, if present.
///
/// # Errors
/// [`StoreError`] on SQL failure or a damaged row.
pub fn find_unit(tx: &Tx, resource_row: i64, id: &UnitId) -> Result<Option<Unit>, StoreError> {
    tx.conn()
        .query_row(
            &format!("SELECT {UNIT_COLUMNS} FROM units WHERE resource_row = ?1 AND id = ?2"),
            rusqlite::params![resource_row, id.as_str()],
            unit_from_row,
        )
        .optional()?
        .transpose()
}

/// Insert a unit; returns its row id.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn insert_unit(tx: &Tx, new: &NewUnitRow<'_>) -> Result<i64, StoreError> {
    tx.conn().execute(
        "INSERT INTO units (resource_row, id, settings, description, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            new.resource_row,
            new.id.as_str(),
            serde_json::to_string(new.settings)?,
            new.description,
            new.created_at,
        ],
    )?;
    Ok(tx.conn().last_insert_rowid())
}

/// Overwrite the mutable fields of a unit.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn update_unit(
    tx: &Tx,
    row: i64,
    settings: &UnitSettings,
    description: &str,
) -> Result<(), StoreError> {
    tx.conn().execute(
        "UPDATE units SET settings = ?2, description = ?3 WHERE row_id = ?1",
        rusqlite::params![row, serde_json::to_string(settings)?, description],
    )?;
    Ok(())
}

/// Delete a unit; its links, leases and pinned requests cascade.
///
/// This is the only way a unit row is deleted. Its identity group must not
/// fall apart with it (a unit can be the one link between two others, and a
/// lease on one of them must keep covering the other), so first every pair
/// of its direct neighbours not already linked directly is linked as a
/// manual link. Neighbours are always in one group, so a suppression between
/// them is bypassed (and kept). The new links are audited as
/// `group_preserved`, stamped `at` and `who`.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn delete_unit(tx: &Tx, row: i64, at: i64, who: &Caller) -> Result<(), StoreError> {
    let neighbours = neighbours(tx, row)?;
    let mut added: Vec<(i64, i64)> = Vec::new();
    for (i, a) in neighbours.iter().enumerate() {
        for b in &neighbours[i + 1..] {
            if !link_exists(tx, *a, *b)? {
                insert_link(tx, *a, *b, LinkOrigin::Manual)?;
                added.push((*a, *b));
            }
        }
    }
    if !added.is_empty() {
        let mut links = Vec::with_capacity(added.len());
        for (a, b) in &added {
            links.push([unit_label(tx, *a)?, unit_label(tx, *b)?]);
        }
        audit::record(
            tx,
            at,
            who,
            "group_preserved",
            &json!({ "unit": unit_label(tx, row)?, "links": links }),
        )?;
    }
    tx.conn()
        .execute("DELETE FROM units WHERE row_id = ?1", [row])?;
    Ok(())
}

/// The units linked directly to `unit`, ascending.
fn neighbours(tx: &Tx, unit: i64) -> Result<Vec<i64>, StoreError> {
    let mut stmt = tx.conn().prepare(
        "SELECT unit_b FROM unit_links WHERE unit_a = ?1 \
         UNION SELECT unit_a FROM unit_links WHERE unit_b = ?1 ORDER BY 1",
    )?;
    Ok(stmt
        .query_map([unit], |r| r.get(0))?
        .collect::<Result<Vec<_>, _>>()?)
}

/// `resource/unit` for a unit row (`#<row>` when it is gone).
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn unit_label(tx: &Tx, row: i64) -> Result<String, StoreError> {
    Ok(match unit_by_row(tx, row)? {
        Some(unit) => format!("{}/{}", load_resource(tx, unit.resource_row)?.id, unit.id),
        None => format!("#{row}"),
    })
}

/// Flag a unit as retiring.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn mark_retiring(tx: &Tx, row: i64) -> Result<(), StoreError> {
    tx.conn()
        .execute("UPDATE units SET retiring = 1 WHERE row_id = ?1", [row])?;
    Ok(())
}

/// Store the identity used for duplicate detection.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn set_identity(tx: &Tx, row: i64, identity: &Identity) -> Result<(), StoreError> {
    tx.conn().execute(
        "UPDATE units SET identity = ?2 WHERE row_id = ?1",
        rusqlite::params![row, serde_json::to_string(identity)?],
    )?;
    Ok(())
}

/// A stored unit to compare a new identity against.
#[derive(Debug)]
pub struct Candidate {
    pub unit_row: i64,
    pub unit_id: UnitId,
    pub resource_row: i64,
    pub resource_id: ResourceId,
    pub scope: Scope,
    pub identity: Identity,
    pub resource_description: String,
    pub unit_description: String,
}

/// Every unit of resources of type `ty`, in any scope, that has an identity,
/// except `exclude` (a unit row, or `None`).
///
/// # Errors
/// [`StoreError`] on SQL failure or a damaged row.
pub fn candidates(
    tx: &Tx,
    ty: ResourceType,
    exclude: Option<i64>,
) -> Result<Vec<Candidate>, StoreError> {
    let mut stmt = tx.conn().prepare(
        "SELECT u.row_id, u.id, r.row_id, r.id, r.scope_kind, r.project_key, u.identity, \
         r.description, u.description \
         FROM units u JOIN resources r ON r.row_id = u.resource_row \
         WHERE r.type = ?1 AND u.identity IS NOT NULL AND u.row_id != ?2 \
         ORDER BY u.row_id",
    )?;
    let raws = stmt
        .query_map(
            rusqlite::params![enum_text(&ty)?, exclude.unwrap_or(-1)],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, String>(8)?,
                ))
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    raws.into_iter()
        .map(
            |(
                unit_row,
                uid,
                resource_row,
                rid,
                kind,
                key,
                identity,
                resource_description,
                unit_description,
            )| {
                Ok(Candidate {
                    unit_row,
                    unit_id: UnitId::parse(&uid).map_err(|_| StoreError::Corrupt("units.id"))?,
                    resource_row,
                    resource_id: ResourceId::parse(&rid)
                        .map_err(|_| StoreError::Corrupt("resources.id"))?,
                    scope: scope_from_columns(&kind, key)?,
                    identity: serde_json::from_str(&identity)?,
                    resource_description,
                    unit_description,
                })
            },
        )
        .collect()
}

/// How a unit link came about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkOrigin {
    /// Found by identity matching.
    Auto,
    /// Made by an administrator.
    Manual,
}

impl LinkOrigin {
    fn text(self) -> &'static str {
        match self {
            LinkOrigin::Auto => "auto",
            LinkOrigin::Manual => "manual",
        }
    }
}

/// Link two units, once per pair (lower row id first). A manual link
/// overrides an existing automatic one; an automatic link never downgrades a
/// manual one.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn insert_link(tx: &Tx, a: i64, b: i64, origin: LinkOrigin) -> Result<(), StoreError> {
    if a == b {
        return Ok(());
    }
    let (lo, hi) = (a.min(b), a.max(b));
    tx.conn().execute(
        "INSERT INTO unit_links (unit_a, unit_b, origin) VALUES (?1, ?2, ?3) \
         ON CONFLICT (unit_a, unit_b) DO UPDATE SET origin = 'manual' \
         WHERE excluded.origin = 'manual'",
        rusqlite::params![lo, hi, origin.text()],
    )?;
    Ok(())
}

/// Remove the link between two units, whatever its origin.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn delete_link(tx: &Tx, a: i64, b: i64) -> Result<(), StoreError> {
    tx.conn().execute(
        "DELETE FROM unit_links WHERE unit_a = ?1 AND unit_b = ?2",
        rusqlite::params![a.min(b), a.max(b)],
    )?;
    Ok(())
}

/// Remove every automatic link of a unit.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn delete_auto_links(tx: &Tx, unit: i64) -> Result<(), StoreError> {
    tx.conn().execute(
        "DELETE FROM unit_links WHERE origin = 'auto' AND (unit_a = ?1 OR unit_b = ?1)",
        [unit],
    )?;
    Ok(())
}

/// The transitive closure of links around `unit`, including `unit`, sorted.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn identity_group(tx: &Tx, unit: i64) -> Result<Vec<i64>, StoreError> {
    let mut stmt = tx.conn().prepare(
        "WITH RECURSIVE g(r) AS ( \
             SELECT ?1 \
             UNION \
             SELECT CASE WHEN l.unit_a = g.r THEN l.unit_b ELSE l.unit_a END \
             FROM unit_links l JOIN g ON l.unit_a = g.r OR l.unit_b = g.r \
         ) SELECT r FROM g ORDER BY r",
    )?;
    Ok(stmt
        .query_map([unit], |r| r.get(0))?
        .collect::<Result<Vec<_>, _>>()?)
}

/// A pending promotion as stored.
#[derive(Debug, Clone)]
pub struct PromotionRow {
    pub row: i64,
    pub id: PromotionId,
    pub resource_row: i64,
    pub resource_id: ResourceId,
    /// The resource's description now.
    pub resource_description: String,
    pub requester: Option<ProjectKey>,
    pub reason: String,
    pub created_at: i64,
}

fn requester_text(requester: Option<&ProjectKey>) -> String {
    requester.map_or_else(String::new, |k| k.0.to_string_lossy().into_owned())
}

const PROMOTION_SELECT: &str = "SELECT p.row_id, p.promotion_id, p.resource_row, r.id, \
     p.requester, p.reason, p.created_at, r.description \
     FROM promotions p JOIN resources r ON r.row_id = p.resource_row \
     WHERE p.state = 'pending'";

fn promotion_from_row(r: &Row<'_>) -> rusqlite::Result<Result<PromotionRow, StoreError>> {
    let row: i64 = r.get(0)?;
    let id: String = r.get(1)?;
    let resource_row: i64 = r.get(2)?;
    let resource_id: String = r.get(3)?;
    let requester: String = r.get(4)?;
    let reason: String = r.get(5)?;
    let created_at: i64 = r.get(6)?;
    let resource_description: String = r.get(7)?;
    Ok((|| {
        Ok(PromotionRow {
            row,
            id: PromotionId::parse(&id).map_err(|_| StoreError::Corrupt("promotions.id"))?,
            resource_row,
            resource_id: ResourceId::parse(&resource_id)
                .map_err(|_| StoreError::Corrupt("resources.id"))?,
            resource_description,
            requester: (!requester.is_empty()).then(|| ProjectKey(PathBuf::from(requester))),
            reason,
            created_at,
        })
    })())
}

/// The pending promotion of `resource_row` requested by `requester`, if any.
///
/// # Errors
/// [`StoreError`] on SQL failure or a damaged row.
pub fn find_pending_promotion(
    tx: &Tx,
    resource_row: i64,
    requester: Option<&ProjectKey>,
) -> Result<Option<PromotionId>, StoreError> {
    let found: Option<String> = tx
        .conn()
        .query_row(
            "SELECT promotion_id FROM promotions \
             WHERE state = 'pending' AND resource_row = ?1 AND requester = ?2",
            rusqlite::params![resource_row, requester_text(requester)],
            |r| r.get(0),
        )
        .optional()?;
    found
        .map(|id| PromotionId::parse(&id).map_err(|_| StoreError::Corrupt("promotions.id")))
        .transpose()
}

/// Insert a pending promotion.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn insert_promotion(
    tx: &Tx,
    id: &PromotionId,
    resource_row: i64,
    requester: Option<&ProjectKey>,
    reason: &str,
    created_at: i64,
) -> Result<(), StoreError> {
    tx.conn().execute(
        "INSERT INTO promotions (promotion_id, resource_row, requester, reason, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            id.as_str(),
            resource_row,
            requester_text(requester),
            reason,
            created_at
        ],
    )?;
    Ok(())
}

/// All pending promotions, oldest first.
///
/// # Errors
/// [`StoreError`] on SQL failure or a damaged row.
pub fn list_promotions(tx: &Tx) -> Result<Vec<PromotionRow>, StoreError> {
    let mut stmt = tx
        .conn()
        .prepare(&format!("{PROMOTION_SELECT} ORDER BY p.row_id"))?;
    let rows = stmt
        .query_map([], promotion_from_row)?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter().collect()
}

/// One pending promotion by id.
///
/// # Errors
/// [`StoreError`] on SQL failure or a damaged row.
pub fn find_promotion(tx: &Tx, id: &PromotionId) -> Result<Option<PromotionRow>, StoreError> {
    tx.conn()
        .query_row(
            &format!("{PROMOTION_SELECT} AND p.promotion_id = ?1"),
            [id.as_str()],
            promotion_from_row,
        )
        .optional()?
        .transpose()
}

/// Delete every promotion of a resource.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn delete_promotions_of(tx: &Tx, resource_row: i64) -> Result<(), StoreError> {
    tx.conn().execute(
        "DELETE FROM promotions WHERE resource_row = ?1",
        [resource_row],
    )?;
    Ok(())
}

/// Move a resource to user scope under `new_id`.
///
/// # Errors
/// [`StoreError`] on SQL failure, including a unique-index violation.
pub fn move_to_user_scope(tx: &Tx, row: i64, new_id: &ResourceId) -> Result<(), StoreError> {
    tx.conn().execute(
        "UPDATE resources SET scope_kind = 'user', project_key = '', id = ?2 WHERE row_id = ?1",
        rusqlite::params![row, new_id.as_str()],
    )?;
    Ok(())
}

/// One unit by row id, if present.
///
/// # Errors
/// [`StoreError`] on SQL failure or a damaged row.
pub fn unit_by_row(tx: &Tx, row: i64) -> Result<Option<Unit>, StoreError> {
    tx.conn()
        .query_row(
            &format!("SELECT {UNIT_COLUMNS} FROM units WHERE row_id = ?1"),
            [row],
            unit_from_row,
        )
        .optional()?
        .transpose()
}

/// Every unit row id, ascending.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn all_unit_rows(tx: &Tx) -> Result<Vec<i64>, StoreError> {
    let mut stmt = tx
        .conn()
        .prepare("SELECT row_id FROM units ORDER BY row_id")?;
    Ok(stmt
        .query_map([], |r| r.get(0))?
        .collect::<Result<Vec<_>, _>>()?)
}

/// Row ids of retiring units, ascending.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn retiring_unit_rows(tx: &Tx) -> Result<Vec<i64>, StoreError> {
    let mut stmt = tx
        .conn()
        .prepare("SELECT row_id FROM units WHERE retiring = 1 ORDER BY row_id")?;
    Ok(stmt
        .query_map([], |r| r.get(0))?
        .collect::<Result<Vec<_>, _>>()?)
}

/// Record the outcome of a health check.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn record_health(tx: &Tx, row: i64, text: &str, at: i64) -> Result<(), StoreError> {
    tx.conn().execute(
        "UPDATE units SET last_health = ?2, last_health_at = ?3 WHERE row_id = ?1",
        rusqlite::params![row, text, at],
    )?;
    Ok(())
}

/// Set or clear a unit's dirty record.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn set_dirty(tx: &Tx, row: i64, dirty: Option<&DirtyInfo>) -> Result<(), StoreError> {
    let (reason, at, prev) = match dirty {
        Some(d) => (
            Some(enum_text(&d.reason)?),
            Some(d.at),
            Some(serde_json::to_string(&d.previous_holder)?),
        ),
        None => (None, None, None),
    };
    tx.conn().execute(
        "UPDATE units SET dirty_reason = ?2, dirty_at = ?3, dirty_prev_holder = ?4 \
         WHERE row_id = ?1",
        rusqlite::params![row, reason, at, prev],
    )?;
    Ok(())
}

/// Record that an administrator declared units `a` and `b` different
/// things, so identity matching never links them again.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn insert_suppression(tx: &Tx, a: i64, b: i64) -> Result<(), StoreError> {
    if a == b {
        return Ok(());
    }
    tx.conn().execute(
        "INSERT OR IGNORE INTO link_suppressions (unit_a, unit_b) VALUES (?1, ?2)",
        rusqlite::params![a.min(b), a.max(b)],
    )?;
    Ok(())
}

/// Forget a suppression of the pair `a`, `b`, if any.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn delete_suppression(tx: &Tx, a: i64, b: i64) -> Result<(), StoreError> {
    tx.conn().execute(
        "DELETE FROM link_suppressions WHERE unit_a = ?1 AND unit_b = ?2",
        rusqlite::params![a.min(b), a.max(b)],
    )?;
    Ok(())
}

/// True when the pair `a`, `b` must not be auto-linked.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn suppressed(tx: &Tx, a: i64, b: i64) -> Result<bool, StoreError> {
    Ok(tx.conn().query_row(
        "SELECT EXISTS (SELECT 1 FROM link_suppressions WHERE unit_a = ?1 AND unit_b = ?2)",
        rusqlite::params![a.min(b), a.max(b)],
        |r| r.get(0),
    )?)
}

/// True when `a` and `b` are linked directly (not just through others).
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn link_exists(tx: &Tx, a: i64, b: i64) -> Result<bool, StoreError> {
    Ok(tx.conn().query_row(
        "SELECT EXISTS (SELECT 1 FROM unit_links WHERE unit_a = ?1 AND unit_b = ?2)",
        rusqlite::params![a.min(b), a.max(b)],
        |r| r.get(0),
    )?)
}

/// Every link as `(unit_a, unit_b)` row pairs, lower row first, in row order.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn all_links(tx: &Tx) -> Result<Vec<(i64, i64)>, StoreError> {
    let mut stmt = tx
        .conn()
        .prepare("SELECT unit_a, unit_b FROM unit_links ORDER BY unit_a, unit_b")?;
    Ok(stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<Vec<_>, _>>()?)
}

/// Links naming a unit row that does not exist (possible only in a damaged
/// database: foreign keys normally cascade).
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn dangling_links(tx: &Tx) -> Result<Vec<(i64, i64)>, StoreError> {
    let mut stmt = tx.conn().prepare(
        "SELECT unit_a, unit_b FROM unit_links \
         WHERE unit_a NOT IN (SELECT row_id FROM units) \
            OR unit_b NOT IN (SELECT row_id FROM units) \
         ORDER BY unit_a, unit_b",
    )?;
    Ok(stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<Vec<_>, _>>()?)
}

/// Move a resource to `scope`, keeping its id.
///
/// # Errors
/// [`StoreError`] on SQL failure, including a unique-index violation.
pub fn set_scope(tx: &Tx, row: i64, scope: &Scope) -> Result<(), StoreError> {
    let (kind, key) = scope_columns(scope);
    tx.conn().execute(
        "UPDATE resources SET scope_kind = ?2, project_key = ?3 WHERE row_id = ?1",
        rusqlite::params![row, kind, key],
    )?;
    Ok(())
}

/// Change a resource's id.
///
/// # Errors
/// [`StoreError`] on SQL failure, including a unique-index violation.
pub fn rename_resource(tx: &Tx, row: i64, id: &ResourceId) -> Result<(), StoreError> {
    tx.conn().execute(
        "UPDATE resources SET id = ?2 WHERE row_id = ?1",
        rusqlite::params![row, id.as_str()],
    )?;
    Ok(())
}

/// Change a unit's id.
///
/// # Errors
/// [`StoreError`] on SQL failure, including a unique-index violation.
pub fn rename_unit(tx: &Tx, row: i64, id: &UnitId) -> Result<(), StoreError> {
    tx.conn().execute(
        "UPDATE units SET id = ?2 WHERE row_id = ?1",
        rusqlite::params![row, id.as_str()],
    )?;
    Ok(())
}

/// Every suppressed pair, lower row first.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn all_suppressions(tx: &Tx) -> Result<Vec<(i64, i64)>, StoreError> {
    let mut stmt = tx
        .conn()
        .prepare("SELECT unit_a, unit_b FROM link_suppressions ORDER BY unit_a, unit_b")?;
    Ok(stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<Vec<_>, _>>()?)
}

/// Every link as `(unit_a, unit_b, origin)`, lower row first, in row order.
///
/// # Errors
/// [`StoreError`] on SQL failure or a damaged row.
pub fn all_links_with_origin(tx: &Tx) -> Result<Vec<(i64, i64, LinkOrigin)>, StoreError> {
    let mut stmt = tx
        .conn()
        .prepare("SELECT unit_a, unit_b, origin FROM unit_links ORDER BY unit_a, unit_b")?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(|(a, b, origin)| match origin.as_str() {
            "auto" => Ok((a, b, LinkOrigin::Auto)),
            "manual" => Ok((a, b, LinkOrigin::Manual)),
            _ => Err(StoreError::Corrupt("unit_links.origin")),
        })
        .collect()
}
