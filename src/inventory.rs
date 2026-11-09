//! Inventory rules: scoped CRUD over resources and units.
//!
//! Lookup by id tries the caller's project scope first, then user scope. A
//! caller without a project sees user-scope resources only; the CLI resolves
//! its working directory into `Caller::project` before calling in.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use serde_json::json;
use thiserror::Error;

use crate::clock::Clock;
use crate::dedup::{self, DEFAULT_SYSFS, Identity, Match};
use crate::error::ErrorCode;
use crate::model::{
    Caller, Description, HealthSpec, IdError, InvalidSettings, LeaseId, ProjectKey, PromotionId,
    Resource, ResourceId, ResourceType, Scope, TextError, Unit, UnitId, UnitSettings,
    UsageTemplate,
};
use crate::store::inventory::{
    self as sql, Candidate, LinkOrigin, NewResourceRow, NewUnitRow, ScopeFilter,
};
use crate::store::{StoreError, Tx, audit, leases};

/// Which resources a listing covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// What the caller can see: user scope plus the caller's project.
    Caller,
    /// Every scope (admin).
    All,
}

/// A resource to create.
#[derive(Debug, Clone)]
pub struct NewResource {
    pub id: ResourceId,
    pub ty: ResourceType,
    pub scope: Scope,
    pub description: String,
    /// Usage template; checked against `ty`.
    pub usage: String,
    /// `None` means the type default.
    pub health: Option<HealthSpec>,
}

/// Changes to a resource. `None` leaves a field alone.
#[derive(Debug, Clone, Default)]
pub struct ResourcePatch {
    pub description: Option<String>,
    /// Checked against the resource's type.
    pub usage: Option<String>,
    /// `Some(None)` clears the spec back to the type default.
    pub health: Option<Option<HealthSpec>>,
}

/// A unit to add.
#[derive(Debug, Clone)]
pub struct NewUnit {
    pub id: UnitId,
    pub settings: UnitSettings,
    pub description: String,
}

/// Changes to a unit. `None` leaves a field alone.
#[derive(Debug, Clone, Default)]
pub struct UnitPatch {
    pub settings: Option<UnitSettings>,
    pub description: Option<String>,
}

/// Something worth telling the caller about a successful change.
///
/// `SimilarInOtherProject` deliberately carries only the other project's path
/// and the promotion that lets the user share the thing: never its resource,
/// unit, settings, description or usage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Warning {
    /// A new project resource hides a user resource of the same id.
    ShadowsUserResource { id: ResourceId },
    /// A new user resource is hidden, inside `project`, by a project resource.
    ShadowedByProject { id: ResourceId, project: ProjectKey },
    /// A project unit looks like a user-scope unit.
    DuplicateOfUserUnit {
        resource: ResourceId,
        unit: UnitId,
        resource_description: String,
        unit_description: String,
    },
    /// A new user-scope unit looks like a unit of the caller's own project.
    DuplicateOfProjectUnit {
        resource: ResourceId,
        unit: UnitId,
        resource_description: String,
        unit_description: String,
    },
    /// A unit in another project looks like this one.
    SimilarInOtherProject {
        project: ProjectKey,
        promotion_id: PromotionId,
    },
}

/// A result plus the warnings raised while producing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created<T> {
    pub value: T,
    pub warnings: Vec<Warning>,
}

/// Why an inventory operation failed.
#[derive(Debug, Error)]
pub enum InventoryError {
    /// An id breaks the format rule.
    #[error("invalid id: {0}")]
    InvalidId(#[from] IdError),
    /// No such resource or unit is visible to the caller.
    #[error("no such resource or unit")]
    NotFound,
    /// The id is already used in that scope.
    #[error("that id is already in use")]
    AlreadyExists,
    /// A field is invalid; `field` names it.
    #[error("invalid value for {field}")]
    InvalidSettings { field: &'static str },
    /// A description or usage template breaks its rules.
    #[error(transparent)]
    InvalidText(#[from] TextError),
    /// The caller may not create in that scope.
    #[error("you may only create resources in user scope or your own project")]
    ScopeNotAllowed,
    /// Units are held or queued for, or the change would split an
    /// identity group that has a live lease.
    #[error(
        "the resource or unit (or a unit linked to it) is held or has waiters; release or \
         retire it first"
    )]
    Busy,
    /// The unit duplicates one in the same scope.
    #[error("unit duplicates {unit} of {resource}")]
    DuplicateInScope {
        resource: ResourceId,
        unit: UnitId,
        resource_description: String,
        unit_description: String,
    },
    /// The unit matches units that must stay apart: they are held by
    /// different holders, or an administrator declared them different
    /// things. Joining them would make one physical thing of them.
    #[error(
        "the unit matches {}, which must stay apart (held separately, or unlinked by an \
         administrator); change its settings, or link it to one of them explicitly",
        .units.join(", ")
    )]
    AmbiguousDuplicate {
        /// The matched units, as `resource/unit`.
        units: Vec<String>,
    },
    /// A link would join identity groups that each have a live lease, so
    /// one physical thing would have two holders.
    #[error(
        "linking {} would give one physical thing to two live leases ({}); end one of them \
         first",
        .units.join(" and "),
        .leases.iter().map(LeaseId::as_str).collect::<Vec<_>>().join(", ")
    )]
    LinkConflict {
        /// The two units, as `resource/unit`.
        units: Vec<String>,
        /// The live leases on the groups being joined.
        leases: Vec<LeaseId>,
    },
    /// Storage failed.
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl ErrorCode for InventoryError {
    fn code(&self) -> &'static str {
        match self {
            InventoryError::InvalidId(_) => "invalid_id",
            InventoryError::NotFound => "not_found",
            InventoryError::AlreadyExists => "already_exists",
            InventoryError::InvalidSettings { .. } => "invalid_settings",
            InventoryError::InvalidText(e) => e.code(),
            InventoryError::ScopeNotAllowed => "scope_not_allowed",
            InventoryError::Busy => "busy",
            InventoryError::DuplicateInScope { .. } => "duplicate_in_scope",
            InventoryError::AmbiguousDuplicate { .. } => "ambiguous_duplicate",
            InventoryError::LinkConflict { .. } => "link_conflict",
            InventoryError::Store(e) => e.code(),
        }
    }

    fn details(&self) -> Option<serde_json::Value> {
        match self {
            InventoryError::DuplicateInScope {
                resource,
                unit,
                resource_description,
                unit_description,
            } => Some(json!({
                "resource": resource,
                "unit": unit,
                "resource_description": resource_description,
                "unit_description": unit_description,
            })),
            InventoryError::InvalidText(e) => e.details(),
            InventoryError::AmbiguousDuplicate { units } => Some(json!({ "units": units })),
            InventoryError::LinkConflict { leases, .. } => Some(json!({ "leases": leases })),
            _ => None,
        }
    }
}

/// Why identity groups may not be joined.
enum JoinRefusal {
    /// Two of the groups each have live leases (these).
    Leases(Vec<LeaseId>),
    /// A pair an administrator unlinked would end up in one group.
    Suppressed,
}

/// The one guard on every new link: refuse to join `groups` (disjoint
/// identity groups) when two of them each have a live lease, or, with
/// `suppressed`, when a pair an administrator unlinked lies across two of
/// them.
fn check_join(
    tx: &Tx,
    groups: &[Vec<i64>],
    suppressed: bool,
) -> Result<Option<JoinRefusal>, InventoryError> {
    if groups.len() < 2 {
        return Ok(None);
    }
    let mut leased = 0;
    let mut ids = Vec::new();
    for group in groups {
        let live = leases::live_leases_on(tx, group)?;
        if !live.is_empty() {
            leased += 1;
            ids.extend(live.into_iter().map(|l| l.id));
        }
    }
    if leased >= 2 {
        return Ok(Some(JoinRefusal::Leases(ids)));
    }
    if suppressed {
        let index: HashMap<i64, usize> = groups
            .iter()
            .enumerate()
            .flat_map(|(i, g)| g.iter().map(move |r| (*r, i)))
            .collect();
        for (a, b) in sql::all_suppressions(tx)? {
            if let (Some(ia), Some(ib)) = (index.get(&a), index.get(&b))
                && ia != ib
            {
                return Ok(Some(JoinRefusal::Suppressed));
            }
        }
    }
    Ok(None)
}

/// The distinct identity groups of `rows`.
fn groups_of(tx: &Tx, rows: &[i64]) -> Result<Vec<Vec<i64>>, InventoryError> {
    let mut groups: Vec<Vec<i64>> = Vec::new();
    for row in rows {
        if !groups.iter().any(|g| g.contains(row)) {
            groups.push(sql::identity_group(tx, *row)?);
        }
    }
    Ok(groups)
}

/// Every link as it will be once `subject`'s automatic links are replaced
/// by links to `hits` (as `update_unit` does), computed without writing.
fn links_after_change(
    tx: &Tx,
    subject: i64,
    hits: &[i64],
) -> Result<Vec<(i64, i64)>, InventoryError> {
    Ok(sql::all_links_with_origin(tx)?
        .into_iter()
        .filter(|(a, b, origin)| !(*origin == LinkOrigin::Auto && (*a == subject || *b == subject)))
        .map(|(a, b, _)| (a, b))
        .chain(hits.iter().map(|h| (subject, *h)))
        .collect())
}

/// The transitive closure of `start` over `edges`, including `start`, sorted.
fn closure(edges: &[(i64, i64)], start: i64) -> Vec<i64> {
    let mut seen: HashSet<i64> = HashSet::from([start]);
    let mut todo = vec![start];
    while let Some(r) = todo.pop() {
        for (a, b) in edges {
            let next = if *a == r {
                *b
            } else if *b == r {
                *a
            } else {
                continue;
            };
            if seen.insert(next) {
                todo.push(next);
            }
        }
    }
    let mut group: Vec<i64> = seen.into_iter().collect();
    group.sort_unstable();
    group
}

/// The distinct identity groups of `rows` as they will be once the
/// automatic links of `subject` are gone (as `update_unit` does before
/// relinking), computed without writing.
fn groups_without_auto_links(
    tx: &Tx,
    subject: i64,
    rows: &[i64],
) -> Result<Vec<Vec<i64>>, InventoryError> {
    let edges = links_after_change(tx, subject, &[])?;
    let mut groups: Vec<Vec<i64>> = Vec::new();
    for start in rows {
        if !groups.iter().any(|g| g.contains(start)) {
            groups.push(closure(&edges, *start));
        }
    }
    Ok(groups)
}

/// The split guard, the counterpart of [`check_join`]: refuse a settings
/// change of `subject` (its automatic links replaced by links to `hits`)
/// when its current identity group has a live lease and the change would
/// separate any member of that group from the others. A lease on one
/// member covers the physical thing every member is; a split would let a
/// former partner be handed out again.
fn check_split(tx: &Tx, subject: i64, hits: &[i64]) -> Result<(), InventoryError> {
    let current = sql::identity_group(tx, subject)?;
    if leases::live_leases_on(tx, &current)?.is_empty() {
        return Ok(());
    }
    let after = closure(&links_after_change(tx, subject, hits)?, subject);
    if current.iter().all(|r| after.binary_search(r).is_ok()) {
        Ok(())
    } else {
        Err(InventoryError::Busy)
    }
}

/// `resource/unit` for a unit row (`#<row>` when it is gone).
fn unit_name(tx: &Tx, row: i64) -> Result<String, InventoryError> {
    Ok(sql::unit_label(tx, row)?)
}

fn settings_field(err: &InvalidSettings) -> &'static str {
    match err {
        InvalidSettings::TypeMismatch(_) => "settings",
        InvalidSettings::Empty(field) => field,
        InvalidSettings::ZeroPort => "port",
    }
}

/// The inventory service.
pub struct Inventory {
    clock: Arc<dyn Clock>,
    sysfs: PathBuf,
}

impl Inventory {
    /// A service stamping its records from `clock`, reading USB data from
    /// the real `/sys`.
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self::with_sysfs(clock, PathBuf::from(DEFAULT_SYSFS))
    }

    /// As [`Inventory::new`], reading USB data below `sysfs` (for tests).
    #[must_use]
    pub fn with_sysfs(clock: Arc<dyn Clock>, sysfs: PathBuf) -> Self {
        Self { clock, sysfs }
    }

    /// The units that are the same physical thing as `unit_row`: the
    /// transitive closure over links, including `unit_row`, sorted.
    ///
    /// # Errors
    /// [`InventoryError::Store`] on storage failure.
    pub fn identity_group(tx: &Tx, unit_row: i64) -> Result<Vec<i64>, InventoryError> {
        Ok(sql::identity_group(tx, unit_row)?)
    }

    /// Link two units as the same thing (a manual link: kept when either
    /// unit's settings change). Linking twice is harmless. An earlier
    /// [`Inventory::unlink`] of the pair is forgotten.
    ///
    /// # Errors
    /// [`InventoryError::Store`] on storage failure, including unknown rows;
    /// [`InventoryError::LinkConflict`] when the two units' identity groups
    /// each have a live lease (nothing is written).
    pub fn link(tx: &Tx, a: i64, b: i64) -> Result<(), InventoryError> {
        if let Some(JoinRefusal::Leases(leases)) = check_join(tx, &groups_of(tx, &[a, b])?, false)?
        {
            return Err(InventoryError::LinkConflict {
                units: vec![unit_name(tx, a)?, unit_name(tx, b)?],
                leases,
            });
        }
        sql::delete_suppression(tx, a, b)?;
        Ok(sql::insert_link(tx, a, b, LinkOrigin::Manual)?)
    }

    /// Remove the link between two units, whatever its origin, and remember
    /// that they are different things: identity matching never links the
    /// pair again (until [`Inventory::link`]).
    ///
    /// # Errors
    /// [`InventoryError::Store`] on storage failure.
    pub fn unlink(tx: &Tx, a: i64, b: i64) -> Result<(), InventoryError> {
        sql::delete_link(tx, a, b)?;
        Ok(sql::insert_suppression(tx, a, b)?)
    }

    /// Hits of `identity` among stored units of type `ty` in every scope,
    /// excluding `exclude` (the unit being changed) and units an
    /// administrator unlinked from it. Fails on a hit in `scope` itself.
    fn find_hits(
        tx: &Tx,
        ty: ResourceType,
        scope: &Scope,
        identity: &Identity,
        exclude: Option<i64>,
    ) -> Result<Vec<Candidate>, InventoryError> {
        let mut hits: Vec<Candidate> = Vec::new();
        for c in sql::candidates(tx, ty, exclude)? {
            if !matches!(dedup::matches(identity, &c.identity), Match::Hit(_)) {
                continue;
            }
            if let Some(subject) = exclude
                && sql::suppressed(tx, subject, c.unit_row)?
            {
                continue;
            }
            hits.push(c);
        }
        if let Some(c) = hits.iter().find(|c| c.scope == *scope) {
            return Err(InventoryError::DuplicateInScope {
                resource: c.resource_id.clone(),
                unit: c.unit_id.clone(),
                resource_description: c.resource_description.clone(),
                unit_description: c.unit_description.clone(),
            });
        }
        Ok(hits)
    }

    /// Refuse a unit (`subject`, the existing row when it is being changed)
    /// whose hits would join identity groups that must stay apart: two of
    /// them leased, or a pair an administrator unlinked across them.
    fn guard_hits(tx: &Tx, subject: Option<i64>, hits: &[Candidate]) -> Result<(), InventoryError> {
        let hit_rows: Vec<i64> = hits.iter().map(|h| h.unit_row).collect();
        let groups = match subject {
            Some(row) => {
                let rows: Vec<i64> = std::iter::once(row).chain(hit_rows).collect();
                groups_without_auto_links(tx, row, &rows)?
            }
            None => groups_of(tx, &hit_rows)?,
        };
        if check_join(tx, &groups, true)?.is_some() {
            return Err(InventoryError::AmbiguousDuplicate {
                units: hits
                    .iter()
                    .map(|h| format!("{}/{}", h.resource_id, h.unit_id))
                    .collect(),
            });
        }
        Ok(())
    }

    /// Link `unit_row` (in `scope`) to every hit and describe each hit as a
    /// warning, creating or reusing promotions for hits in other projects.
    fn link_hits(
        &self,
        tx: &Tx,
        who: &Caller,
        unit_row: i64,
        scope: &Scope,
        hits: &[Candidate],
    ) -> Result<Vec<Warning>, InventoryError> {
        let mut warnings = Vec::new();
        let mut links = Vec::with_capacity(hits.len());
        for hit in hits {
            sql::insert_link(tx, unit_row, hit.unit_row, LinkOrigin::Auto)?;
            links.push([unit_name(tx, unit_row)?, unit_name(tx, hit.unit_row)?]);
            let warning = match (scope, &hit.scope) {
                (Scope::Project(_), Scope::User) => Warning::DuplicateOfUserUnit {
                    resource: hit.resource_id.clone(),
                    unit: hit.unit_id.clone(),
                    resource_description: hit.resource_description.clone(),
                    unit_description: hit.unit_description.clone(),
                },
                (_, Scope::Project(other))
                    if scope == &Scope::User && who.project.as_ref() == Some(other) =>
                {
                    Warning::DuplicateOfProjectUnit {
                        resource: hit.resource_id.clone(),
                        unit: hit.unit_id.clone(),
                        resource_description: hit.resource_description.clone(),
                        unit_description: hit.unit_description.clone(),
                    }
                }
                (_, Scope::Project(other)) => {
                    // The requester is the project the new unit lives in, or,
                    // for a user-scope unit, the caller's own project.
                    let requester = match scope {
                        Scope::Project(p) => Some(p),
                        Scope::User => who.project.as_ref(),
                    };
                    let promotion_id = self.request_promotion(tx, who, hit, requester, scope)?;
                    Warning::SimilarInOtherProject {
                        project: other.clone(),
                        promotion_id,
                    }
                }
                // Same-scope hits were rejected by `find_hits`.
                (Scope::User, Scope::User) => continue,
            };
            if !warnings.contains(&warning) {
                warnings.push(warning);
            }
        }
        if !links.is_empty() {
            self.audit(tx, who, "link_created", &json!({ "links": links }))?;
        }
        Ok(warnings)
    }

    fn request_promotion(
        &self,
        tx: &Tx,
        who: &Caller,
        hit: &Candidate,
        requester: Option<&ProjectKey>,
        scope: &Scope,
    ) -> Result<PromotionId, InventoryError> {
        if let Some(id) = sql::find_pending_promotion(tx, hit.resource_row, requester)? {
            return Ok(id);
        }
        let reason = match scope {
            Scope::Project(p) => format!(
                "a matching unit was registered in project {}",
                p.0.display()
            ),
            Scope::User => "a matching unit was registered in user scope".to_owned(),
        };
        let id = PromotionId::new();
        sql::insert_promotion(
            tx,
            &id,
            hit.resource_row,
            requester,
            &reason,
            self.clock.now_ms(),
        )?;
        self.audit(
            tx,
            who,
            "promotion_requested",
            &json!({
                "promotion": id,
                "resource": hit.resource_id,
                "requester": requester,
                "reason": reason,
            }),
        )?;
        Ok(id)
    }

    fn audit(
        &self,
        tx: &Tx,
        who: &Caller,
        action: &str,
        detail: &serde_json::Value,
    ) -> Result<(), InventoryError> {
        Ok(audit::record(tx, self.clock.now_ms(), who, action, detail)?)
    }

    fn visible_row(tx: &Tx, who: &Caller, id: &ResourceId) -> Result<i64, InventoryError> {
        sql::find_visible(tx, who.project.as_ref(), id)?.ok_or(InventoryError::NotFound)
    }

    /// Create a resource in `new.scope`.
    ///
    /// # Errors
    /// [`InventoryError::ScopeNotAllowed`] for a project scope other than the
    /// caller's, [`InventoryError::InvalidSettings`] (field `health`) when the health
    /// spec does not fit the type, [`InventoryError::InvalidText`] for a bad
    /// description or usage template, [`InventoryError::AlreadyExists`] when the
    /// id is taken in that scope.
    pub fn create_resource(
        &self,
        tx: &Tx,
        who: &Caller,
        new: &NewResource,
    ) -> Result<Created<Resource>, InventoryError> {
        // First, before any lookup, so it cannot be used to probe other scopes.
        if let Scope::Project(key) = &new.scope
            && who.project.as_ref() != Some(key)
        {
            return Err(InventoryError::ScopeNotAllowed);
        }
        if let Some(health) = &new.health {
            health
                .validate(new.ty)
                .map_err(|_| InventoryError::InvalidSettings { field: "health" })?;
        }
        Description::parse(&new.description)?;
        UsageTemplate::parse(&new.usage, new.ty)?;
        if sql::find_in_scope(tx, &new.scope, &new.id)?.is_some() {
            return Err(InventoryError::AlreadyExists);
        }
        let row = sql::insert_resource(
            tx,
            &NewResourceRow {
                id: &new.id,
                ty: new.ty,
                scope: &new.scope,
                description: &new.description,
                usage: &new.usage,
                health: new.health.as_ref(),
                created_at: self.clock.now_ms(),
            },
        )?;
        let mut warnings = Vec::new();
        match &new.scope {
            Scope::Project(_) => {
                if sql::find_in_scope(tx, &Scope::User, &new.id)?.is_some() {
                    warnings.push(Warning::ShadowsUserResource { id: new.id.clone() });
                }
            }
            Scope::User => {
                // Only the caller's own project is checked: other projects'
                // inventories are not the caller's business.
                if let Some(project) = &who.project {
                    let scope = Scope::Project(project.clone());
                    if sql::find_in_scope(tx, &scope, &new.id)?.is_some() {
                        warnings.push(Warning::ShadowedByProject {
                            id: new.id.clone(),
                            project: project.clone(),
                        });
                    }
                }
            }
        }
        self.audit(
            tx,
            who,
            "resource_created",
            &json!({ "id": new.id, "type": new.ty, "scope": new.scope }),
        )?;
        Ok(Created {
            value: sql::load_resource(tx, row)?,
            warnings,
        })
    }

    /// Change a visible resource.
    ///
    /// # Errors
    /// [`InventoryError::NotFound`] when invisible;
    /// [`InventoryError::InvalidSettings`] (field `health`) on a bad spec;
    /// [`InventoryError::InvalidText`] for a bad description, or a usage
    /// template that breaks the rules of the resource's type.
    pub fn update_resource(
        &self,
        tx: &Tx,
        who: &Caller,
        id: &ResourceId,
        patch: ResourcePatch,
    ) -> Result<Resource, InventoryError> {
        let row = Self::visible_row(tx, who, id)?;
        let current = sql::load_resource(tx, row)?;
        let health = match patch.health {
            Some(h) => h,
            None => current.health.clone(),
        };
        if let Some(h) = &health {
            h.validate(current.ty)
                .map_err(|_| InventoryError::InvalidSettings { field: "health" })?;
        }
        if let Some(text) = &patch.description {
            Description::parse(text)?;
        }
        if let Some(text) = &patch.usage {
            UsageTemplate::parse(text, current.ty)?;
        }
        let description = patch.description.unwrap_or(current.description);
        let usage = patch.usage.unwrap_or(current.usage);
        sql::update_resource(tx, row, &description, &usage, health.as_ref())?;
        self.audit(
            tx,
            who,
            "resource_updated",
            &json!({ "id": id, "scope": current.scope }),
        )?;
        Ok(sql::load_resource(tx, row)?)
    }

    /// Delete a visible resource and its units.
    ///
    /// # Errors
    /// [`InventoryError::NotFound`] when invisible;
    /// [`InventoryError::Busy`] when units are held or requests are queued.
    pub fn delete_resource(
        &self,
        tx: &Tx,
        who: &Caller,
        id: &ResourceId,
    ) -> Result<(), InventoryError> {
        let row = Self::visible_row(tx, who, id)?;
        if sql::resource_busy(tx, row)? {
            return Err(InventoryError::Busy);
        }
        let scope = sql::load_resource(tx, row)?.scope;
        sql::delete_resource(tx, row, self.clock.now_ms(), who)?;
        self.audit(
            tx,
            who,
            "resource_deleted",
            &json!({ "id": id, "scope": scope }),
        )
    }

    /// List resources in `view`, ordered by id.
    ///
    /// # Errors
    /// [`InventoryError::Store`] on storage failure.
    pub fn list_resources(
        &self,
        tx: &Tx,
        who: &Caller,
        view: View,
    ) -> Result<Vec<Resource>, InventoryError> {
        let filter = match view {
            View::All => ScopeFilter::All,
            View::Caller => ScopeFilter::Visible(who.project.as_ref()),
        };
        Ok(sql::list_resources(tx, filter)?)
    }

    /// The resource `id` resolves to for the caller.
    ///
    /// # Errors
    /// [`InventoryError::NotFound`] when invisible.
    pub fn get_resource(
        &self,
        tx: &Tx,
        who: &Caller,
        id: &ResourceId,
    ) -> Result<Resource, InventoryError> {
        let row = Self::visible_row(tx, who, id)?;
        Ok(sql::load_resource(tx, row)?)
    }

    /// Add a unit to a visible resource.
    ///
    /// # Errors
    /// [`InventoryError::NotFound`], [`InventoryError::AlreadyExists`] for a
    /// repeated unit id, [`InventoryError::InvalidSettings`] naming the field,
    /// [`InventoryError::InvalidText`] for a bad description.
    pub fn add_unit(
        &self,
        tx: &Tx,
        who: &Caller,
        resource: &ResourceId,
        new: &NewUnit,
    ) -> Result<Created<Unit>, InventoryError> {
        let row = Self::visible_row(tx, who, resource)?;
        let current = sql::load_resource(tx, row)?;
        new.settings
            .validate(current.ty)
            .map_err(|e| InventoryError::InvalidSettings {
                field: settings_field(&e),
            })?;
        Description::parse(&new.description)?;
        if sql::find_unit(tx, row, &new.id)?.is_some() {
            return Err(InventoryError::AlreadyExists);
        }
        let identity = Identity::from_settings(&new.settings, &self.sysfs);
        let hits = Self::find_hits(tx, current.ty, &current.scope, &identity, None)?;
        Self::guard_hits(tx, None, &hits)?;
        let unit_row = sql::insert_unit(
            tx,
            &NewUnitRow {
                resource_row: row,
                id: &new.id,
                settings: &new.settings,
                description: &new.description,
                created_at: self.clock.now_ms(),
            },
        )?;
        sql::set_identity(tx, unit_row, &identity)?;
        let warnings = self.link_hits(tx, who, unit_row, &current.scope, &hits)?;
        self.audit(
            tx,
            who,
            "unit_added",
            &json!({ "resource": resource, "unit": new.id, "scope": current.scope }),
        )?;
        let value = sql::find_unit(tx, row, &new.id)?.ok_or(InventoryError::NotFound)?;
        Ok(Created { value, warnings })
    }

    /// Change a unit of a visible resource.
    ///
    /// # Errors
    /// [`InventoryError::NotFound`] for an unknown resource or unit;
    /// [`InventoryError::InvalidSettings`] naming the field;
    /// [`InventoryError::InvalidText`] for a bad description;
    /// [`InventoryError::Busy`] when the unit's identity group has a live
    /// lease and the new settings would split it (nothing is written).
    pub fn update_unit(
        &self,
        tx: &Tx,
        who: &Caller,
        resource: &ResourceId,
        unit: &UnitId,
        patch: UnitPatch,
    ) -> Result<Created<Unit>, InventoryError> {
        let row = Self::visible_row(tx, who, resource)?;
        let current = sql::load_resource(tx, row)?;
        let existing = sql::find_unit(tx, row, unit)?.ok_or(InventoryError::NotFound)?;
        let changed = patch
            .settings
            .as_ref()
            .is_some_and(|s| *s != existing.settings);
        let settings = patch.settings.unwrap_or(existing.settings);
        settings
            .validate(current.ty)
            .map_err(|e| InventoryError::InvalidSettings {
                field: settings_field(&e),
            })?;
        if let Some(text) = &patch.description {
            Description::parse(text)?;
        }
        let description = patch.description.unwrap_or(existing.description);
        // Check before writing anything, so a rejected change leaves no trace.
        let recomputed = if changed {
            let identity = Identity::from_settings(&settings, &self.sysfs);
            let hits = Self::find_hits(
                tx,
                current.ty,
                &current.scope,
                &identity,
                Some(existing.row),
            )?;
            Self::guard_hits(tx, Some(existing.row), &hits)?;
            let hit_rows: Vec<i64> = hits.iter().map(|h| h.unit_row).collect();
            check_split(tx, existing.row, &hit_rows)?;
            Some((identity, hits))
        } else {
            None
        };
        sql::update_unit(tx, existing.row, &settings, &description)?;
        let warnings = match recomputed {
            Some((identity, hits)) => {
                sql::set_identity(tx, existing.row, &identity)?;
                sql::delete_auto_links(tx, existing.row)?;
                self.link_hits(tx, who, existing.row, &current.scope, &hits)?
            }
            None => Vec::new(),
        };
        self.audit(
            tx,
            who,
            "unit_updated",
            &json!({ "resource": resource, "unit": unit, "scope": current.scope }),
        )?;
        let value = sql::find_unit(tx, row, unit)?.ok_or(InventoryError::NotFound)?;
        Ok(Created { value, warnings })
    }

    /// Remove a free unit and its links.
    ///
    /// # Errors
    /// [`InventoryError::NotFound`]; [`InventoryError::Busy`] when the unit is
    /// held or has queued requests.
    pub fn remove_unit(
        &self,
        tx: &Tx,
        who: &Caller,
        resource: &ResourceId,
        unit: &UnitId,
    ) -> Result<(), InventoryError> {
        let row = Self::visible_row(tx, who, resource)?;
        let existing = sql::find_unit(tx, row, unit)?.ok_or(InventoryError::NotFound)?;
        if sql::unit_busy(tx, existing.row)? {
            return Err(InventoryError::Busy);
        }
        sql::delete_unit(tx, existing.row, self.clock.now_ms(), who)?;
        self.audit(
            tx,
            who,
            "unit_removed",
            &json!({ "resource": resource, "unit": unit }),
        )
    }

    /// Retire a unit: remove it now when free, else flag it `retiring` so it
    /// is never granted again and can be removed once its lease ends.
    ///
    /// # Errors
    /// [`InventoryError::NotFound`].
    pub fn retire_unit(
        &self,
        tx: &Tx,
        who: &Caller,
        resource: &ResourceId,
        unit: &UnitId,
    ) -> Result<(), InventoryError> {
        let row = Self::visible_row(tx, who, resource)?;
        let existing = sql::find_unit(tx, row, unit)?.ok_or(InventoryError::NotFound)?;
        let removed = if sql::unit_busy(tx, existing.row)? {
            sql::mark_retiring(tx, existing.row)?;
            false
        } else {
            sql::delete_unit(tx, existing.row, self.clock.now_ms(), who)?;
            true
        };
        self.audit(
            tx,
            who,
            "unit_retired",
            &json!({ "resource": resource, "unit": unit, "removed": removed }),
        )
    }
}

/// A pending request to move a resource to user scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Promotion {
    pub id: PromotionId,
    /// The resource that would be promoted.
    pub resource: ResourceId,
    /// The promoted resource's description, as it is now.
    pub description: String,
    /// The project whose registration raised it, if any.
    pub requester: Option<ProjectKey>,
    pub reason: String,
    pub created_at: i64,
}

/// Why approving a promotion failed.
#[derive(Debug, Error)]
pub enum PromotionError {
    /// No such pending promotion.
    #[error("no such promotion")]
    NotFound,
    /// User scope already has a resource with that id.
    #[error("user scope already has a resource named {id}")]
    IdClash { id: ResourceId },
    /// Storage failed.
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl ErrorCode for PromotionError {
    fn code(&self) -> &'static str {
        match self {
            PromotionError::NotFound => "not_found",
            PromotionError::IdClash { .. } => "id_clash",
            PromotionError::Store(e) => e.code(),
        }
    }
}

/// Pending promotions of project resources to user scope.
pub struct Promotions {
    clock: Arc<dyn Clock>,
}

impl Promotions {
    /// A service stamping its audit rows from `clock`.
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self { clock }
    }

    /// Pending promotions, oldest first.
    ///
    /// # Errors
    /// [`PromotionError::Store`] on storage failure.
    pub fn list(&self, tx: &Tx) -> Result<Vec<Promotion>, PromotionError> {
        Ok(sql::list_promotions(tx)?
            .into_iter()
            .map(|p| Promotion {
                id: p.id,
                resource: p.resource_id,
                description: p.resource_description,
                requester: p.requester,
                reason: p.reason,
                created_at: p.created_at,
            })
            .collect())
    }

    /// Move the promoted resource to user scope, under `new_id` when given.
    /// Links are kept. Who may approve is the caller's concern.
    ///
    /// # Errors
    /// [`PromotionError::NotFound`] for an unknown or already-handled
    /// promotion, or one whose resource is already in user scope;
    /// [`PromotionError::IdClash`] when user scope already has a resource
    /// with the target id.
    // The by-value id is the interface agreed for callers handing over a parsed id.
    #[allow(clippy::needless_pass_by_value)]
    pub fn approve(
        &self,
        tx: &Tx,
        who: &Caller,
        id: PromotionId,
        new_id: Option<ResourceId>,
    ) -> Result<Resource, PromotionError> {
        let promo = sql::find_promotion(tx, &id)?.ok_or(PromotionError::NotFound)?;
        let from = sql::load_resource(tx, promo.resource_row)?;
        if from.scope == Scope::User {
            return Err(PromotionError::NotFound);
        }
        let target = new_id.unwrap_or_else(|| promo.resource_id.clone());
        if sql::find_in_scope(tx, &Scope::User, &target)?.is_some() {
            return Err(PromotionError::IdClash { id: target });
        }
        sql::move_to_user_scope(tx, promo.resource_row, &target)?;
        sql::delete_promotions_of(tx, promo.resource_row)?;
        audit::record(
            tx,
            self.clock.now_ms(),
            who,
            "promotion_approved",
            &json!({
                "promotion": id,
                "from_id": from.id,
                "from_scope": from.scope,
                "id": target,
            }),
        )?;
        Ok(sql::load_resource(tx, promo.resource_row)?)
    }
}
