//! The audit log.

use serde_json::Value;

use crate::model::{Caller, HolderRef};
use crate::store::{StoreError, Tx};

/// One audit log row.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditEntry {
    /// Row id, increasing with time.
    pub id: i64,
    /// When it happened, unix milliseconds.
    pub at: i64,
    /// Who did it.
    pub actor: HolderRef,
    /// The user request this action served, if any.
    pub override_reason: Option<String>,
    /// What happened, e.g. `resource_created`.
    pub action: String,
    /// Action-specific details.
    pub detail: Value,
}

/// Append an audit entry made by `actor` at `at_ms`.
///
/// # Errors
/// [`StoreError`] when the row cannot be written.
pub fn record(
    tx: &Tx,
    at_ms: i64,
    actor: &Caller,
    action: &str,
    detail: &Value,
) -> Result<(), StoreError> {
    tx.conn().execute(
        "INSERT INTO audit (at, actor, override_reason, action, detail) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            at_ms,
            serde_json::to_string(&actor.holder)?,
            actor.override_reason,
            action,
            serde_json::to_string(detail)?,
        ],
    )?;
    Ok(())
}

/// Longest page [`list`] returns.
pub const MAX_LIMIT: u32 = 1000;
/// Page length [`list`] uses when the filter names none.
pub const DEFAULT_LIMIT: u32 = 100;

/// Which entries [`list`] returns; unset fields do not filter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditFilter {
    /// Only entries at or after this time, unix ms.
    pub since: Option<i64>,
    /// Only entries made by this holder (structural equality).
    pub actor: Option<HolderRef>,
    /// Only entries of this action.
    pub action: Option<String>,
    /// Page length: [`DEFAULT_LIMIT`] when unset, clamped to [`MAX_LIMIT`].
    pub limit: Option<u32>,
}

/// Entries matching `filter`, newest first.
///
/// # Errors
/// [`StoreError`] when rows cannot be read or decoded.
pub fn list(tx: &Tx, filter: &AuditFilter) -> Result<Vec<AuditEntry>, StoreError> {
    let actor = filter
        .actor
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    let limit = filter.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    let mut stmt = tx.conn().prepare(&format!(
        "SELECT {COLUMNS} FROM audit \
         WHERE (?1 IS NULL OR at >= ?1) AND (?2 IS NULL OR actor = ?2) \
           AND (?3 IS NULL OR action = ?3) \
         ORDER BY at DESC, row_id DESC LIMIT ?4"
    ))?;
    let rows = stmt.query_map(
        rusqlite::params![filter.since, actor, filter.action, limit],
        read_row,
    )?;
    decode(rows)
}

/// All entries, oldest first, unfiltered.
///
/// # Errors
/// [`StoreError`] when rows cannot be read or decoded.
pub fn all(tx: &Tx) -> Result<Vec<AuditEntry>, StoreError> {
    let mut stmt = tx
        .conn()
        .prepare(&format!("SELECT {COLUMNS} FROM audit ORDER BY row_id"))?;
    let rows = stmt.query_map([], read_row)?;
    decode(rows)
}

const COLUMNS: &str = "row_id, at, actor, override_reason, action, detail";

type RawRow = (i64, i64, String, Option<String>, String, String);

fn read_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawRow> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
    ))
}

fn decode(
    rows: impl Iterator<Item = rusqlite::Result<RawRow>>,
) -> Result<Vec<AuditEntry>, StoreError> {
    let mut out = Vec::new();
    for row in rows {
        let (id, at, actor, override_reason, action, detail) = row?;
        out.push(AuditEntry {
            id,
            at,
            actor: serde_json::from_str(&actor)?,
            override_reason,
            action,
            detail: serde_json::from_str(&detail)?,
        });
    }
    Ok(out)
}
