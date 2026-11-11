//! SQL for leases and queued requests.

use std::path::PathBuf;

use rusqlite::{OptionalExtension, Row};

use crate::model::{
    DirtyInfo, EndCause, HolderRef, LeaseId, LeaseState, Priority, ProjectKey, RequestId,
};
use crate::store::{StoreError, Tx, enum_text, parse_enum};

fn project_text(project: Option<&ProjectKey>) -> String {
    project.map_or_else(String::new, |k| k.0.to_string_lossy().into_owned())
}

fn project_from_text(text: String) -> Option<ProjectKey> {
    (!text.is_empty()).then(|| ProjectKey(PathBuf::from(text)))
}

/// A lease as stored.
// A row mirror: each flag is its own column with its own meaning, so folding
// them into enums would only obscure the mapping.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseRow {
    pub row: i64,
    pub id: LeaseId,
    pub unit_row: i64,
    pub resource_row: i64,
    pub state: LeaseState,
    pub holder: HolderRef,
    /// The holder's project when it asked.
    pub project: Option<ProjectKey>,
    pub priority: Priority,
    pub interruptible: bool,
    pub purpose: String,
    pub duration_ms: i64,
    pub created_at: i64,
    pub ends_at: Option<i64>,
    pub claim_deadline: Option<i64>,
    pub cleanup_deadline: Option<i64>,
    pub override_reason: Option<String>,
    /// The unit's dirty record handed over with this lease.
    pub dirty: Option<DirtyInfo>,
    /// The queued request an offer came from; `None` for direct grants.
    pub request_id: Option<RequestId>,
    /// The expiry warning for the current `ends_at` was handled.
    pub warning_sent: bool,
    /// Why the lease is cleaning; set while (and after) it cleans up.
    pub cleanup_cause: Option<EndCause>,
    /// The one cleanup extension was granted.
    pub extension_used: bool,
    /// The queued request this holder is being asked to yield for.
    pub yield_request: Option<RequestId>,
    /// When the pending yield ask went out.
    pub yield_asked_at: Option<i64>,
    /// When the holder's yield window ends; `None` when no ask is pending.
    pub yield_deadline: Option<i64>,
    /// While paused: the time that was left on the lease.
    pub paused_remaining_ms: Option<i64>,
    /// On a paused lease: the borrower's lease row.
    pub borrower_lease: Option<i64>,
    /// On a borrower's lease: the paused lease row it borrows from.
    pub paused_for_lease: Option<i64>,
    /// On a claimed borrower's lease: claim time + declared duration.
    pub borrow_due_at: Option<i64>,
    /// `BorrowerOverran` went out for this borrow.
    pub overran_sent: bool,
    /// Once ended: whether cleanup was confirmed.
    pub cleanup_confirmed: Option<bool>,
    /// Once ended: why.
    pub end_cause: Option<EndCause>,
    /// `CleanupEnding` was handled for the current cleanup deadline.
    pub cleanup_ending_sent: bool,
    /// Past its end with nobody waiting, and still held (agents only).
    pub overdue: bool,
}

/// Values for a new lease row.
#[derive(Debug)]
pub struct NewLease<'a> {
    pub id: &'a LeaseId,
    pub unit_row: i64,
    pub resource_row: i64,
    pub state: LeaseState,
    pub holder: &'a HolderRef,
    pub project: Option<&'a ProjectKey>,
    pub priority: Priority,
    pub interruptible: bool,
    pub purpose: &'a str,
    pub duration_ms: i64,
    pub created_at: i64,
    pub ends_at: Option<i64>,
    pub claim_deadline: Option<i64>,
    pub override_reason: Option<&'a str>,
    pub dirty: Option<&'a DirtyInfo>,
    pub request_id: Option<&'a RequestId>,
}

const LEASE_COLUMNS: &str = "row_id, lease_id, unit_row, resource_row, state, holder, \
     project_key, priority, interruptible, purpose, duration_ms, created_at, ends_at, \
     claim_deadline, cleanup_deadline, override_reason, dirty_info, request_id, warning_sent, \
     cleanup_cause, extension_used, yield_request, yield_asked_at, yield_deadline, \
     paused_remaining_ms, borrower_lease, paused_for_lease, borrow_due_at, overran_sent, \
     cleanup_confirmed, end_cause, cleanup_ending_sent, overdue";

fn lease_from_row(r: &Row<'_>) -> rusqlite::Result<Result<LeaseRow, StoreError>> {
    let row: i64 = r.get(0)?;
    let id: String = r.get(1)?;
    let unit_row: i64 = r.get(2)?;
    let resource_row: i64 = r.get(3)?;
    let state: String = r.get(4)?;
    let holder: String = r.get(5)?;
    let project: String = r.get(6)?;
    let priority: String = r.get(7)?;
    let interruptible: bool = r.get(8)?;
    let purpose: String = r.get(9)?;
    let duration_ms: i64 = r.get(10)?;
    let created_at: i64 = r.get(11)?;
    let ends_at: Option<i64> = r.get(12)?;
    let claim_deadline: Option<i64> = r.get(13)?;
    let cleanup_deadline: Option<i64> = r.get(14)?;
    let override_reason: Option<String> = r.get(15)?;
    let dirty: Option<String> = r.get(16)?;
    let request_id: Option<String> = r.get(17)?;
    let warning_sent: bool = r.get(18)?;
    let cleanup_cause: Option<String> = r.get(19)?;
    let extension_used: bool = r.get(20)?;
    let yield_request: Option<String> = r.get(21)?;
    let yield_asked_at: Option<i64> = r.get(22)?;
    let yield_deadline: Option<i64> = r.get(23)?;
    let paused_remaining_ms: Option<i64> = r.get(24)?;
    let borrower_lease: Option<i64> = r.get(25)?;
    let paused_for_lease: Option<i64> = r.get(26)?;
    let borrow_due_at: Option<i64> = r.get(27)?;
    let overran_sent: bool = r.get(28)?;
    let cleanup_confirmed: Option<bool> = r.get(29)?;
    let end_cause: Option<String> = r.get(30)?;
    let cleanup_ending_sent: bool = r.get(31)?;
    let overdue: bool = r.get(32)?;
    Ok((|| {
        Ok(LeaseRow {
            row,
            id: LeaseId::parse(&id).map_err(|_| StoreError::Corrupt("leases.lease_id"))?,
            unit_row,
            resource_row,
            state: parse_enum(&state, "leases.state")?,
            holder: serde_json::from_str(&holder)?,
            project: project_from_text(project),
            priority: parse_enum(&priority, "leases.priority")?,
            interruptible,
            purpose,
            duration_ms,
            created_at,
            ends_at,
            claim_deadline,
            cleanup_deadline,
            override_reason,
            dirty: dirty.map(|d| serde_json::from_str(&d)).transpose()?,
            request_id: request_id
                .map(|id| {
                    RequestId::parse(&id).map_err(|_| StoreError::Corrupt("leases.request_id"))
                })
                .transpose()?,
            warning_sent,
            cleanup_cause: cleanup_cause
                .map(|c| parse_enum(&c, "leases.cleanup_cause"))
                .transpose()?,
            extension_used,
            yield_request: yield_request
                .map(|id| {
                    RequestId::parse(&id).map_err(|_| StoreError::Corrupt("leases.yield_request"))
                })
                .transpose()?,
            yield_asked_at,
            yield_deadline,
            paused_remaining_ms,
            borrower_lease,
            paused_for_lease,
            borrow_due_at,
            overran_sent,
            cleanup_confirmed,
            end_cause: end_cause
                .map(|c| parse_enum(&c, "leases.end_cause"))
                .transpose()?,
            cleanup_ending_sent,
            overdue,
        })
    })())
}

/// [`lease_from_row`] with the row id, which it may not get to report.
fn lease_with_row(r: &Row<'_>) -> rusqlite::Result<(i64, Result<LeaseRow, StoreError>)> {
    Ok((r.get(0)?, lease_from_row(r)?))
}

/// The rows that decode; each of the others is logged and skipped, so one
/// row a query cannot read does not fail it. SQL failures still do.
fn decodable<T>(
    tx: &Tx,
    table: &'static str,
    rows: impl Iterator<Item = rusqlite::Result<(i64, Result<T, StoreError>)>>,
) -> Result<Vec<T>, StoreError> {
    let mut out = Vec::new();
    for row in rows {
        match row? {
            (_, Ok(value)) => out.push(value),
            (row, Err(e)) => tx.log_skipped(table, row, &e),
        }
    }
    Ok(out)
}

fn collect_leases(
    tx: &Tx,
    rows: impl Iterator<Item = rusqlite::Result<(i64, Result<LeaseRow, StoreError>)>>,
) -> Result<Vec<LeaseRow>, StoreError> {
    decodable(tx, "leases", rows)
}

/// Insert a lease; returns its row id.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn insert_lease(tx: &Tx, new: &NewLease<'_>) -> Result<i64, StoreError> {
    tx.conn().execute(
        "INSERT INTO leases (lease_id, unit_row, resource_row, state, holder, project_key, \
         priority, interruptible, purpose, duration_ms, created_at, ends_at, claim_deadline, \
         override_reason, dirty_info, request_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        rusqlite::params![
            new.id.as_str(),
            new.unit_row,
            new.resource_row,
            enum_text(&new.state)?,
            serde_json::to_string(new.holder)?,
            project_text(new.project),
            enum_text(&new.priority)?,
            new.interruptible,
            new.purpose,
            new.duration_ms,
            new.created_at,
            new.ends_at,
            new.claim_deadline,
            new.override_reason,
            new.dirty.map(serde_json::to_string).transpose()?,
            new.request_id.map(RequestId::as_str),
        ],
    )?;
    Ok(tx.conn().last_insert_rowid())
}

/// One lease by id.
///
/// # Errors
/// [`StoreError`] on SQL failure or a damaged row.
pub fn find_lease(tx: &Tx, id: &LeaseId) -> Result<Option<LeaseRow>, StoreError> {
    tx.conn()
        .query_row(
            &format!("SELECT {LEASE_COLUMNS} FROM leases WHERE lease_id = ?1"),
            [id.as_str()],
            lease_from_row,
        )
        .optional()?
        .transpose()
}

/// The open offer made for queued request `request`, if any.
///
/// # Errors
/// [`StoreError`] on SQL failure or a damaged row.
pub fn offer_for_request(tx: &Tx, request: &RequestId) -> Result<Option<LeaseRow>, StoreError> {
    tx.conn()
        .query_row(
            &format!(
                "SELECT {LEASE_COLUMNS} FROM leases WHERE request_id = ?1 AND state = 'offered'"
            ),
            [request.as_str()],
            lease_from_row,
        )
        .optional()?
        .transpose()
}

/// True when any of `units` has a lease that is not ended, read or not: a
/// row this binary cannot decode still occupies its unit. Whether a unit may
/// be handed out or removed is decided here, not on the decoded leases.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn any_live_on(tx: &Tx, units: &[i64]) -> Result<bool, StoreError> {
    Ok(tx.conn().query_row(
        "SELECT EXISTS (SELECT 1 FROM leases WHERE state != 'ended' \
         AND unit_row IN (SELECT value FROM json_each(?1)))",
        [serde_json::to_string(units)?],
        |r| r.get(0),
    )?)
}

/// Leases that are not ended on any of `units`, oldest first.
///
/// # Errors
/// [`StoreError`] on SQL failure. A row that does not decode is logged and
/// skipped.
pub fn live_leases_on(tx: &Tx, units: &[i64]) -> Result<Vec<LeaseRow>, StoreError> {
    let mut stmt = tx.conn().prepare(&format!(
        "SELECT {LEASE_COLUMNS} FROM leases WHERE unit_row = ?1 AND state != 'ended' \
         ORDER BY created_at, row_id"
    ))?;
    let mut out = Vec::new();
    for unit in units {
        out.extend(collect_leases(tx, stmt.query_map([unit], lease_with_row)?)?);
    }
    out.sort_by_key(|l| (l.created_at, l.row));
    Ok(out)
}

/// Offers whose claim deadline is before `before`, oldest deadline first.
///
/// # Errors
/// [`StoreError`] on SQL failure. A row that does not decode is logged and
/// skipped.
pub fn lapsed_offers(tx: &Tx, before: i64) -> Result<Vec<LeaseRow>, StoreError> {
    let mut stmt = tx.conn().prepare(&format!(
        "SELECT {LEASE_COLUMNS} FROM leases WHERE state = 'offered' AND claim_deadline < ?1 \
         ORDER BY claim_deadline, row_id"
    ))?;
    collect_leases(tx, stmt.query_map([before], lease_with_row)?)
}

/// Leases that have a running timer: offered, held or cleaning.
///
/// # Errors
/// [`StoreError`] on SQL failure. A row that does not decode is logged and
/// skipped.
pub fn timed_leases(tx: &Tx) -> Result<Vec<LeaseRow>, StoreError> {
    let mut stmt = tx.conn().prepare(&format!(
        "SELECT {LEASE_COLUMNS} FROM leases \
         WHERE state IN ('offered', 'held', 'cleaning') ORDER BY row_id"
    ))?;
    collect_leases(tx, stmt.query_map([], lease_with_row)?)
}

/// Move a held lease's end to `ends_at`, record its new total
/// `duration_ms`, and re-arm its expiry warning.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn set_extended(tx: &Tx, row: i64, ends_at: i64, duration_ms: i64) -> Result<(), StoreError> {
    tx.conn().execute(
        "UPDATE leases SET ends_at = ?2, duration_ms = ?3, warning_sent = 0, overdue = 0 \
         WHERE row_id = ?1",
        rusqlite::params![row, ends_at, duration_ms],
    )?;
    Ok(())
}

/// Mark a held lease past its end as overdue: it stays held.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn set_overdue(tx: &Tx, row: i64) -> Result<(), StoreError> {
    tx.conn()
        .execute("UPDATE leases SET overdue = 1 WHERE row_id = ?1", [row])?;
    Ok(())
}

/// Mark the expiry warning of the current `ends_at` handled.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn set_warning_sent(tx: &Tx, row: i64) -> Result<(), StoreError> {
    tx.conn().execute(
        "UPDATE leases SET warning_sent = 1 WHERE row_id = ?1",
        [row],
    )?;
    Ok(())
}

/// Start a lease's cleanup phase for `cause`, ending at `deadline`. A
/// pending yield ask lapses: the unit is being handed back anyway.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn set_cleaning(tx: &Tx, row: i64, cause: EndCause, deadline: i64) -> Result<(), StoreError> {
    tx.conn().execute(
        "UPDATE leases SET state = 'cleaning', cleanup_cause = ?2, cleanup_deadline = ?3, \
         cleanup_ending_sent = 0, yield_request = NULL, yield_deadline = NULL WHERE row_id = ?1",
        rusqlite::params![row, enum_text(&cause)?, deadline],
    )?;
    Ok(())
}

/// Move a cleaning lease's deadline to `deadline` and use up its one
/// extension; the cleanup-ending notice is due again.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn set_cleanup_extended(tx: &Tx, row: i64, deadline: i64) -> Result<(), StoreError> {
    tx.conn().execute(
        "UPDATE leases SET cleanup_deadline = ?2, extension_used = 1, cleanup_ending_sent = 0 \
         WHERE row_id = ?1",
        rusqlite::params![row, deadline],
    )?;
    Ok(())
}

/// Move a cleaning lease's deadline to `deadline`, leaving its extension
/// alone (the restart grace); the cleanup-ending notice is due again.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn set_cleanup_deadline(tx: &Tx, row: i64, deadline: i64) -> Result<(), StoreError> {
    tx.conn().execute(
        "UPDATE leases SET cleanup_deadline = ?2, cleanup_ending_sent = 0 WHERE row_id = ?1",
        rusqlite::params![row, deadline],
    )?;
    Ok(())
}

/// Record that the cleanup-ending notice for the current deadline was
/// handled.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn set_cleanup_ending_sent(tx: &Tx, row: i64) -> Result<(), StoreError> {
    tx.conn().execute(
        "UPDATE leases SET cleanup_ending_sent = 1 WHERE row_id = ?1",
        [row],
    )?;
    Ok(())
}

/// Move an offer's claim deadline to `deadline` (the restart grace).
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn set_claim_deadline(tx: &Tx, row: i64, deadline: i64) -> Result<(), StoreError> {
    tx.conn().execute(
        "UPDATE leases SET claim_deadline = ?2 WHERE row_id = ?1",
        rusqlite::params![row, deadline],
    )?;
    Ok(())
}

/// Turn an offer into a held lease ending at `ends_at`.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn set_held(tx: &Tx, row: i64, ends_at: i64) -> Result<(), StoreError> {
    tx.conn().execute(
        "UPDATE leases SET state = 'held', ends_at = ?2, claim_deadline = NULL WHERE row_id = ?1",
        rusqlite::params![row, ends_at],
    )?;
    Ok(())
}

/// End a lease with `cause`, recording whether cleanup was `confirmed`. A
/// pending yield ask ends with it.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn set_ended(tx: &Tx, row: i64, cause: EndCause, confirmed: bool) -> Result<(), StoreError> {
    tx.conn().execute(
        "UPDATE leases SET state = 'ended', end_cause = ?2, claim_deadline = NULL, \
         cleanup_confirmed = ?3, yield_request = NULL, yield_deadline = NULL \
         WHERE row_id = ?1",
        rusqlite::params![row, enum_text(&cause)?, confirmed],
    )?;
    Ok(())
}

/// Every lease that is not ended, by row.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn live_leases(tx: &Tx) -> Result<Vec<LeaseRow>, StoreError> {
    let mut stmt = tx.conn().prepare(&format!(
        "SELECT {LEASE_COLUMNS} FROM leases WHERE state != 'ended' ORDER BY row_id"
    ))?;
    collect_leases(tx, stmt.query_map([], lease_with_row)?)
}

/// Leases in state paused, by row.
///
/// # Errors
/// [`StoreError`] on SQL failure. A row that does not decode is logged and
/// skipped.
pub fn paused_leases(tx: &Tx) -> Result<Vec<LeaseRow>, StoreError> {
    let mut stmt = tx.conn().prepare(&format!(
        "SELECT {LEASE_COLUMNS} FROM leases WHERE state = 'paused' ORDER BY row_id"
    ))?;
    collect_leases(tx, stmt.query_map([], lease_with_row)?)
}

/// One lease by row id.
///
/// # Errors
/// [`StoreError`] on SQL failure or a damaged row.
pub fn lease_by_row(tx: &Tx, row: i64) -> Result<Option<LeaseRow>, StoreError> {
    tx.conn()
        .query_row(
            &format!("SELECT {LEASE_COLUMNS} FROM leases WHERE row_id = ?1"),
            [row],
            lease_from_row,
        )
        .optional()?
        .transpose()
}

/// The held lease whose holder is being asked to yield for `request`.
///
/// # Errors
/// [`StoreError`] on SQL failure or a damaged row.
pub fn asked_for(tx: &Tx, request: &RequestId) -> Result<Option<LeaseRow>, StoreError> {
    tx.conn()
        .query_row(
            &format!(
                "SELECT {LEASE_COLUMNS} FROM leases \
                 WHERE yield_request = ?1 AND state = 'held' LIMIT 1"
            ),
            [request.as_str()],
            lease_from_row,
        )
        .optional()?
        .transpose()
}

/// Ask the holder of lease `row` to yield for `request`, answering by
/// `deadline`, and remember that it was asked for that request.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn set_yield_asked(
    tx: &Tx,
    row: i64,
    request: &RequestRow,
    at: i64,
    deadline: i64,
) -> Result<(), StoreError> {
    tx.conn().execute(
        "UPDATE leases SET yield_request = ?2, yield_asked_at = ?3, yield_deadline = ?4 \
         WHERE row_id = ?1",
        rusqlite::params![row, request.id.as_str(), at, deadline],
    )?;
    tx.conn().execute(
        "INSERT OR IGNORE INTO yield_asks (request_row, lease_row) VALUES (?1, ?2)",
        [request.row, row],
    )?;
    Ok(())
}

/// Drop the pending yield ask on lease `row`, if any.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn clear_yield_ask(tx: &Tx, row: i64) -> Result<(), StoreError> {
    tx.conn().execute(
        "UPDATE leases SET yield_request = NULL, yield_deadline = NULL WHERE row_id = ?1",
        [row],
    )?;
    Ok(())
}

/// Lease rows already asked to yield for request `request_row`.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn asked_leases(tx: &Tx, request_row: i64) -> Result<Vec<i64>, StoreError> {
    let mut stmt = tx
        .conn()
        .prepare("SELECT lease_row FROM yield_asks WHERE request_row = ?1 ORDER BY lease_row")?;
    let rows = stmt
        .query_map([request_row], |r| r.get(0))?
        .collect::<Result<Vec<i64>, _>>()?;
    Ok(rows)
}

/// Pause a held lease with `remaining_ms` left: its clock stops (no end, no
/// timers) and any yield ask is answered.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn set_paused(tx: &Tx, row: i64, remaining_ms: i64) -> Result<(), StoreError> {
    tx.conn().execute(
        "UPDATE leases SET state = 'paused', paused_remaining_ms = ?2, ends_at = NULL, \
         yield_request = NULL, yield_deadline = NULL WHERE row_id = ?1",
        rusqlite::params![row, remaining_ms],
    )?;
    Ok(())
}

/// Link paused lease `paused` and borrower lease `borrower` both ways.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn set_borrow_link(tx: &Tx, paused: i64, borrower: i64) -> Result<(), StoreError> {
    tx.conn().execute(
        "UPDATE leases SET borrower_lease = ?2 WHERE row_id = ?1",
        [paused, borrower],
    )?;
    tx.conn().execute(
        "UPDATE leases SET paused_for_lease = ?2 WHERE row_id = ?1",
        [borrower, paused],
    )?;
    Ok(())
}

/// Detach a borrower's lease from the paused lease it borrowed from: it
/// becomes an ordinary lease.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn clear_paused_for(tx: &Tx, borrower: i64) -> Result<(), StoreError> {
    tx.conn().execute(
        "UPDATE leases SET paused_for_lease = NULL WHERE row_id = ?1",
        [borrower],
    )?;
    Ok(())
}

/// Resume a paused lease: held until `ends_at`, the expiry warning re-armed,
/// the borrow link cleared, and `dirty` as the record handed back with it.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn set_resumed(
    tx: &Tx,
    row: i64,
    ends_at: i64,
    dirty: Option<&DirtyInfo>,
) -> Result<(), StoreError> {
    tx.conn().execute(
        "UPDATE leases SET state = 'held', ends_at = ?2, warning_sent = 0, \
         paused_remaining_ms = NULL, borrower_lease = NULL, dirty_info = ?3 WHERE row_id = ?1",
        rusqlite::params![row, ends_at, dirty.map(serde_json::to_string).transpose()?],
    )?;
    Ok(())
}

/// Record when a borrower's declared duration ends.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn set_borrow_due(tx: &Tx, row: i64, due_at: i64) -> Result<(), StoreError> {
    tx.conn().execute(
        "UPDATE leases SET borrow_due_at = ?2 WHERE row_id = ?1",
        [row, due_at],
    )?;
    Ok(())
}

/// Mark `BorrowerOverran` sent for the borrow of lease `row`.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn set_overran_sent(tx: &Tx, row: i64) -> Result<(), StoreError> {
    tx.conn().execute(
        "UPDATE leases SET overran_sent = 1 WHERE row_id = ?1",
        [row],
    )?;
    Ok(())
}

/// Set whether lease `row` is open to interruption.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn set_interruptible(tx: &Tx, row: i64, interruptible: bool) -> Result<(), StoreError> {
    tx.conn().execute(
        "UPDATE leases SET interruptible = ?2 WHERE row_id = ?1",
        rusqlite::params![row, interruptible],
    )?;
    Ok(())
}

/// A queued request as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestRow {
    pub row: i64,
    pub id: RequestId,
    pub resource_row: i64,
    /// Set when the request targets one specific unit.
    pub unit_row: Option<i64>,
    pub holder: HolderRef,
    pub project: Option<ProjectKey>,
    pub priority: Priority,
    pub interruptible: bool,
    pub purpose: String,
    pub duration_ms: i64,
    pub created_at: i64,
    pub override_reason: Option<String>,
    /// The lease this override request displaced; it is served first.
    pub override_lease: Option<i64>,
    /// Granted without running the unit's health checks (`--skip-checks`).
    pub skip_checks: bool,
}

/// Values for a new queued request.
#[derive(Debug)]
pub struct NewRequest<'a> {
    pub id: &'a RequestId,
    pub resource_row: i64,
    pub unit_row: Option<i64>,
    pub holder: &'a HolderRef,
    pub project: Option<&'a ProjectKey>,
    pub priority: Priority,
    pub interruptible: bool,
    pub purpose: &'a str,
    pub duration_ms: i64,
    pub created_at: i64,
    pub override_reason: Option<&'a str>,
    pub override_lease: Option<i64>,
    pub skip_checks: bool,
}

const REQUEST_COLUMNS: &str = "row_id, request_id, resource_row, unit_row, holder, project_key, \
     priority, interruptible, purpose, duration_ms, created_at, override_reason, override_lease, \
     skip_checks";

/// [`request_from_row`] with the row id, which it may not get to report.
fn request_with_row(r: &Row<'_>) -> rusqlite::Result<(i64, Result<RequestRow, StoreError>)> {
    Ok((r.get(0)?, request_from_row(r)?))
}

fn request_from_row(r: &Row<'_>) -> rusqlite::Result<Result<RequestRow, StoreError>> {
    let row: i64 = r.get(0)?;
    let id: String = r.get(1)?;
    let resource_row: i64 = r.get(2)?;
    let unit_row: Option<i64> = r.get(3)?;
    let holder: String = r.get(4)?;
    let project: String = r.get(5)?;
    let priority: String = r.get(6)?;
    let interruptible: bool = r.get(7)?;
    let purpose: String = r.get(8)?;
    let duration_ms: i64 = r.get(9)?;
    let created_at: i64 = r.get(10)?;
    let override_reason: Option<String> = r.get(11)?;
    let override_lease: Option<i64> = r.get(12)?;
    let skip_checks: bool = r.get(13)?;
    Ok((|| {
        Ok(RequestRow {
            row,
            id: RequestId::parse(&id).map_err(|_| StoreError::Corrupt("requests.request_id"))?,
            resource_row,
            unit_row,
            holder: serde_json::from_str(&holder)?,
            project: project_from_text(project),
            priority: parse_enum(&priority, "requests.priority")?,
            interruptible,
            purpose,
            duration_ms,
            created_at,
            override_reason,
            override_lease,
            skip_checks,
        })
    })())
}

/// Queue a request; returns its row id.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn insert_request(tx: &Tx, new: &NewRequest<'_>) -> Result<i64, StoreError> {
    tx.conn().execute(
        "INSERT INTO requests (request_id, resource_row, unit_row, state, holder, project_key, \
         priority, interruptible, purpose, duration_ms, created_at, override_reason, \
         override_lease, skip_checks) \
         VALUES (?1, ?2, ?3, 'queued', ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        rusqlite::params![
            new.id.as_str(),
            new.resource_row,
            new.unit_row,
            serde_json::to_string(new.holder)?,
            project_text(new.project),
            enum_text(&new.priority)?,
            new.interruptible,
            new.purpose,
            new.duration_ms,
            new.created_at,
            new.override_reason,
            new.override_lease,
            new.skip_checks,
        ],
    )?;
    Ok(tx.conn().last_insert_rowid())
}

/// One queued request by id.
///
/// # Errors
/// [`StoreError`] on SQL failure or a damaged row.
pub fn find_request(tx: &Tx, id: &RequestId) -> Result<Option<RequestRow>, StoreError> {
    tx.conn()
        .query_row(
            &format!(
                "SELECT {REQUEST_COLUMNS} FROM requests \
                 WHERE request_id = ?1 AND state = 'queued'"
            ),
            [id.as_str()],
            request_from_row,
        )
        .optional()?
        .transpose()
}

/// Every queued request, in insertion order. Callers put them in queue
/// order.
///
/// # Errors
/// [`StoreError`] on SQL failure. A row that does not decode is logged and
/// skipped.
pub fn queued_requests(tx: &Tx) -> Result<Vec<RequestRow>, StoreError> {
    let mut stmt = tx.conn().prepare(&format!(
        "SELECT {REQUEST_COLUMNS} FROM requests WHERE state = 'queued' ORDER BY row_id"
    ))?;
    decodable(tx, "requests", stmt.query_map([], request_with_row)?)
}

/// Remove a request from the queue. A yield ask pending for it is
/// withdrawn, and its record of asked holders goes with it.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn delete_request(tx: &Tx, row: i64) -> Result<(), StoreError> {
    tx.conn().execute(
        "UPDATE leases SET yield_request = NULL, yield_deadline = NULL \
         WHERE yield_request = (SELECT request_id FROM requests WHERE row_id = ?1)",
        [row],
    )?;
    tx.conn()
        .execute("DELETE FROM requests WHERE row_id = ?1", [row])?;
    Ok(())
}

/// Set a lease's priority.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn set_priority(tx: &Tx, row: i64, priority: Priority) -> Result<(), StoreError> {
    tx.conn().execute(
        "UPDATE leases SET priority = ?2 WHERE row_id = ?1",
        rusqlite::params![row, enum_text(&priority)?],
    )?;
    Ok(())
}

/// Give a lease to another holder.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn set_holder(tx: &Tx, row: i64, holder: &HolderRef) -> Result<(), StoreError> {
    tx.conn().execute(
        "UPDATE leases SET holder = ?2 WHERE row_id = ?1",
        rusqlite::params![row, serde_json::to_string(holder)?],
    )?;
    Ok(())
}

/// Give a queued request to another holder.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn set_request_holder(tx: &Tx, row: i64, holder: &HolderRef) -> Result<(), StoreError> {
    tx.conn().execute(
        "UPDATE requests SET holder = ?2 WHERE row_id = ?1",
        rusqlite::params![row, serde_json::to_string(holder)?],
    )?;
    Ok(())
}

/// Live leases whose unit or resource row does not exist (possible only in
/// a damaged database: foreign keys normally cascade), by row.
///
/// # Errors
/// [`StoreError`] on SQL failure. A row that does not decode is logged and
/// skipped.
pub fn orphan_leases(tx: &Tx) -> Result<Vec<LeaseRow>, StoreError> {
    let mut stmt = tx.conn().prepare(&format!(
        "SELECT {LEASE_COLUMNS} FROM leases WHERE state != 'ended' \
         AND (unit_row NOT IN (SELECT row_id FROM units) \
              OR resource_row NOT IN (SELECT row_id FROM resources)) \
         ORDER BY row_id"
    ))?;
    collect_leases(tx, stmt.query_map([], lease_with_row)?)
}

/// Queued requests whose resource, or pinned unit, does not exist, by row.
///
/// # Errors
/// [`StoreError`] on SQL failure. A row that does not decode is logged and
/// skipped.
pub fn orphan_requests(tx: &Tx) -> Result<Vec<RequestRow>, StoreError> {
    let mut stmt = tx.conn().prepare(&format!(
        "SELECT {REQUEST_COLUMNS} FROM requests WHERE state = 'queued' \
         AND (resource_row NOT IN (SELECT row_id FROM resources) \
              OR (unit_row IS NOT NULL AND unit_row NOT IN (SELECT row_id FROM units))) \
         ORDER BY row_id"
    ))?;
    decodable(tx, "requests", stmt.query_map([], request_with_row)?)
}
