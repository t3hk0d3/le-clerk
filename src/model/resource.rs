//! Stored inventory records.

use crate::model::{DirtyInfo, HealthSpec, ResourceId, ResourceType, Scope, UnitId, UnitSettings};

/// One leasable unit of a resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    /// Internal storage key; stable across renames.
    pub row: i64,
    /// Storage key of the owning resource.
    pub resource_row: i64,
    /// Identifier within the resource.
    pub id: UnitId,
    /// Settings handed to the holder.
    pub settings: UnitSettings,
    /// One-line description.
    pub description: String,
    /// Set when the unit needs cleaning before reuse.
    pub dirty: Option<DirtyInfo>,
    /// Set when the unit is to be removed once its lease ends.
    pub retiring: bool,
    /// Result text of the last health check.
    pub last_health: Option<String>,
    /// When the last health check ran, unix milliseconds.
    pub last_health_at: Option<i64>,
    /// Creation time, unix milliseconds.
    pub created_at: i64,
}

/// A named group of interchangeable units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    /// Internal storage key; stable across renames.
    pub row: i64,
    /// Identifier within its scope.
    pub id: ResourceId,
    /// What kind of units it holds.
    pub ty: ResourceType,
    /// Who may use it.
    pub scope: Scope,
    /// Short description.
    pub description: String,
    /// Usage template (Markdown with unit placeholders), as written.
    pub usage: String,
    /// Health checks; `None` means the type default.
    pub health: Option<HealthSpec>,
    /// Creation time, unix milliseconds.
    pub created_at: i64,
    /// True when this project resource hides a user resource of the same id.
    pub shadows_user: bool,
    /// Its units, ordered by id.
    pub units: Vec<Unit>,
}
