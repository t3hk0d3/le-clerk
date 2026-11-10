//! Notices: events stored for a holder until it reads them.
//!
//! The whole notice is kept as JSON in `payload`; the foreign-key columns of
//! the table stay empty so that a notice outlives the unit or lease it talks
//! about (a `QueueDropped` for a removed unit must still reach its holder).

use serde::{Deserialize, Serialize};

use crate::model::{DirtyInfo, HolderRef, LeaseId, NoticeId, RequestId, ResourceId, UnitId};
use crate::store::{StoreError, Tx, enum_text};

/// What a notice is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeKind {
    /// A unit is offered; claim it before the deadline.
    Offered,
    /// A queued request (or an unclaimed offer) was dropped.
    QueueDropped,
    /// A queued request moved in its queue.
    PositionChanged,
    /// A lease is about to expire.
    ExpiryWarning,
    /// The cleanup window started.
    CleanupWindowStarted,
    /// The cleanup window is about to end.
    CleanupEnding,
    /// Someone asks the holder to yield.
    YieldRequested,
    /// A yield request was withdrawn: the holder need not yield.
    YieldWithdrawn,
    /// A lent-out unit is back.
    UnitReturned,
    /// A borrower ran past its declared duration.
    BorrowerOverran,
    /// The lease was overridden.
    Overridden,
    /// The lease ended.
    LeaseEnded,
    /// The resource or unit was removed.
    ResourceRemoved,
}

/// One notice to a holder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notice {
    pub id: NoticeId,
    /// The recipient.
    pub holder: HolderRef,
    pub kind: NoticeKind,
    pub lease_id: Option<LeaseId>,
    pub request_id: Option<RequestId>,
    pub resource: Option<ResourceId>,
    pub unit: Option<UnitId>,
    /// The other party's purpose; `None` when the recipient may not see it.
    pub other_purpose: Option<String>,
    /// The deadline this notice is about (claim, cleanup, yield), unix ms.
    pub deadline: Option<i64>,
    /// The new queue position, for `PositionChanged`.
    pub position: Option<u32>,
    /// The unit's dirty record, when it is handed over dirty.
    pub dirty: Option<DirtyInfo>,
    /// For `UnitReturned`: whether the borrower confirmed its cleanup.
    #[serde(default)]
    pub cleanup_confirmed: Option<bool>,
    /// What the recipient should do, in plain words.
    pub action: String,
    pub created_at: i64,
}

impl Notice {
    /// A notice with a fresh id and every optional field empty.
    #[must_use]
    pub fn new(holder: HolderRef, kind: NoticeKind, action: String, created_at: i64) -> Self {
        Self {
            id: NoticeId::new(),
            holder,
            kind,
            lease_id: None,
            request_id: None,
            resource: None,
            unit: None,
            other_purpose: None,
            deadline: None,
            position: None,
            dirty: None,
            cleanup_confirmed: None,
            action,
            created_at,
        }
    }
}

/// Store a notice for its holder. Visibility of `other_purpose` is the
/// caller's concern: publish through `scheduler::notify`.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn publish(tx: &Tx, notice: &Notice) -> Result<(), StoreError> {
    tx.conn().execute(
        "INSERT INTO notices (notice_id, holder, kind, payload, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            notice.id.as_str(),
            serde_json::to_string(&notice.holder)?,
            enum_text(&notice.kind)?,
            serde_json::to_string(notice)?,
            notice.created_at,
        ],
    )?;
    Ok(())
}

/// Undelivered notices of `holder`, oldest first, without marking them.
///
/// # Errors
/// [`StoreError`] on SQL failure or a damaged row.
pub fn pending(tx: &Tx, holder: &HolderRef) -> Result<Vec<Notice>, StoreError> {
    let mut stmt = tx.conn().prepare(
        "SELECT payload FROM notices WHERE holder = ?1 AND delivered_at IS NULL \
         ORDER BY created_at, row_id",
    )?;
    let payloads = stmt
        .query_map([serde_json::to_string(holder)?], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    payloads
        .iter()
        .map(|p| serde_json::from_str(p).map_err(StoreError::from))
        .collect()
}

/// Take up to `limit` undelivered notices of `holder`, oldest first (by
/// `created_at`, then insertion order, never by id), and mark them delivered
/// in the same transaction. The holder matches structurally: its JSON
/// encoding is the serde derive's, whose field order is fixed.
/// `delivered_at` is set to `now_ms`.
///
/// # Errors
/// [`StoreError`] on SQL failure or a damaged row.
pub fn take(
    tx: &Tx,
    holder: &HolderRef,
    limit: u32,
    now_ms: i64,
) -> Result<Vec<Notice>, StoreError> {
    take_matching(tx, holder, None, limit, now_ms)
}

/// Which of a holder's notices a wait is about: those naming one of these
/// leases or requests. Used by a CLI command that acts for an agent
/// session, so the session's other notices stay pending for it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoticeFilter {
    pub leases: Vec<LeaseId>,
    pub requests: Vec<RequestId>,
}

impl NoticeFilter {
    /// True when `n` names one of the filter's leases or requests.
    #[must_use]
    pub fn matches(&self, n: &Notice) -> bool {
        n.lease_id.as_ref().is_some_and(|l| self.leases.contains(l))
            || n.request_id
                .as_ref()
                .is_some_and(|r| self.requests.contains(r))
    }
}

/// [`take`], but with `filter` only the notices it matches are taken (up to
/// `limit`, oldest first); the others stay pending. The filter runs in SQL
/// on the stored payload, so only the rows taken are read and parsed: a
/// damaged row that is not taken cannot fail the take.
///
/// # Errors
/// [`StoreError`] on SQL failure or a damaged row among those taken.
pub fn take_matching(
    tx: &Tx,
    holder: &HolderRef,
    filter: Option<&NoticeFilter>,
    limit: u32,
    now_ms: i64,
) -> Result<Vec<Notice>, StoreError> {
    let rows = select_pending(tx, holder, filter, limit)?;
    let mut mark = tx
        .conn()
        .prepare("UPDATE notices SET delivered_at = ?2 WHERE row_id = ?1")?;
    rows.iter()
        .map(|(row, payload)| {
            mark.execute(rusqlite::params![row, now_ms])?;
            serde_json::from_str(payload).map_err(StoreError::from)
        })
        .collect()
}

/// Acknowledged delivery (MCP `lease_wait`): up to `limit` undelivered
/// notices of `holder`, oldest first, returned **without** marking them
/// delivered. Each is marked returned (`returned_at = now_ms`, kept from
/// its first return), and stays pending until [`ack`] names it, so a result
/// the client threw away loses nothing. With `filter`, only the notices it
/// matches.
///
/// # Errors
/// [`StoreError`] on SQL failure or a damaged row among those returned.
pub fn returned(
    tx: &Tx,
    holder: &HolderRef,
    filter: Option<&NoticeFilter>,
    limit: u32,
    now_ms: i64,
) -> Result<Vec<Notice>, StoreError> {
    let rows = select_pending(tx, holder, filter, limit)?;
    let mut mark = tx
        .conn()
        .prepare("UPDATE notices SET returned_at = COALESCE(returned_at, ?2) WHERE row_id = ?1")?;
    rows.iter()
        .map(|(row, payload)| {
            mark.execute(rusqlite::params![row, now_ms])?;
            serde_json::from_str(payload).map_err(StoreError::from)
        })
        .collect()
}

/// Mark delivered those of `ids` that are undelivered notices of `holder`
/// already [`returned`]; other ids (unknown, another holder's, never
/// returned, already delivered) are ignored. Returns how many were marked.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn ack(
    tx: &Tx,
    holder: &HolderRef,
    ids: &[NoticeId],
    now_ms: i64,
) -> Result<usize, StoreError> {
    let ids: Vec<&str> = ids.iter().map(NoticeId::as_str).collect();
    Ok(tx.conn().execute(
        "UPDATE notices SET delivered_at = ?3 WHERE holder = ?1 AND delivered_at IS NULL \
         AND returned_at IS NOT NULL AND notice_id IN (SELECT value FROM json_each(?2))",
        rusqlite::params![
            serde_json::to_string(holder)?,
            serde_json::to_string(&ids)?,
            now_ms
        ],
    )?)
}

/// `(row_id, payload)` of up to `limit` undelivered notices of `holder`,
/// oldest first; with `filter`, only those it matches. The filter runs in
/// SQL on the stored payload, so only the rows selected are read.
fn select_pending(
    tx: &Tx,
    holder: &HolderRef,
    filter: Option<&NoticeFilter>,
    limit: u32,
) -> Result<Vec<(i64, String)>, StoreError> {
    let holder = serde_json::to_string(holder)?;
    Ok(match filter {
        None => {
            let mut stmt = tx.conn().prepare(
                "SELECT row_id, payload FROM notices WHERE holder = ?1 AND delivered_at IS NULL \
                 ORDER BY created_at, row_id LIMIT ?2",
            )?;
            stmt.query_map(rusqlite::params![holder, limit], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
        }
        Some(f) => {
            // CASE keeps json_extract away from payloads that are not JSON.
            let mut stmt = tx.conn().prepare(
                "SELECT row_id, payload FROM notices WHERE holder = ?1 AND delivered_at IS NULL \
                 AND (CASE WHEN json_valid(payload) THEN \
                        json_extract(payload, '$.lease_id') IN (SELECT value FROM json_each(?2)) \
                        OR json_extract(payload, '$.request_id') IN (SELECT value FROM json_each(?3)) \
                      ELSE 0 END) \
                 ORDER BY created_at, row_id LIMIT ?4",
            )?;
            stmt.query_map(
                rusqlite::params![
                    holder,
                    serde_json::to_string(&f.leases)?,
                    serde_json::to_string(&f.requests)?,
                    limit
                ],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?
        }
    })
}

/// The highest notice row written so far; 0 when there are none. Rows only
/// grow, so a later call returning more means notices were published since.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn last_row(tx: &Tx) -> Result<i64, StoreError> {
    Ok(tx
        .conn()
        .query_row("SELECT COALESCE(MAX(row_id), 0) FROM notices", [], |r| {
            r.get(0)
        })?)
}

/// The distinct recipients of notices written after row `after`. A holder
/// that does not decode is logged and skipped, so one damaged row cannot
/// stop every later wake-up.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn holders_after(tx: &Tx, after: i64) -> Result<Vec<HolderRef>, StoreError> {
    let mut stmt = tx
        .conn()
        .prepare("SELECT DISTINCT holder FROM notices WHERE row_id > ?1")?;
    let holders = stmt
        .query_map([after], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(holders
        .iter()
        .filter_map(|h| match serde_json::from_str(h) {
            Ok(holder) => Some(holder),
            Err(e) => {
                tracing::warn!(holder = %h, error = %e, "skipping notice holder that does not decode");
                None
            }
        })
        .collect())
}

/// Hand every undelivered notice of `from` to `to`, as [`rehome`] does for
/// one lease's notices. Returns how many moved.
///
/// # Errors
/// [`StoreError`] on SQL failure or a damaged row.
pub fn rehome_all(tx: &Tx, from: &HolderRef, to: &HolderRef) -> Result<usize, StoreError> {
    let rows = {
        let mut stmt = tx.conn().prepare(
            "SELECT row_id, payload FROM notices WHERE holder = ?1 AND delivered_at IS NULL \
             ORDER BY created_at, row_id",
        )?;
        stmt.query_map([serde_json::to_string(from)?], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?
    };
    for (row, payload) in &rows {
        let mut notice: Notice = serde_json::from_str(payload)?;
        notice.id = NoticeId::new();
        notice.holder = to.clone();
        publish(tx, &notice)?;
        tx.conn()
            .execute("DELETE FROM notices WHERE row_id = ?1", [row])?;
    }
    Ok(rows.len())
}

/// Hand the undelivered notices of `from` about `lease` to `to`. Each one is
/// written again as a new row, under a fresh id but with its `created_at`
/// (so its place in the order is kept), before the old row goes: the new
/// row id is then above every row written so far, which is what wakes the
/// new holder's waiters. (Deleting first would let SQLite reuse the old,
/// already-seen row id.) Returns how many moved.
///
/// # Errors
/// [`StoreError`] on SQL failure or a damaged row.
pub fn rehome(
    tx: &Tx,
    from: &HolderRef,
    lease: &LeaseId,
    to: &HolderRef,
) -> Result<usize, StoreError> {
    let rows = {
        let mut stmt = tx.conn().prepare(
            "SELECT row_id, payload FROM notices WHERE holder = ?1 AND delivered_at IS NULL \
             ORDER BY created_at, row_id",
        )?;
        stmt.query_map([serde_json::to_string(from)?], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?
    };
    let mut moved = 0;
    for (row, payload) in rows {
        let mut notice: Notice = serde_json::from_str(&payload)?;
        if notice.lease_id.as_ref() != Some(lease) {
            continue;
        }
        notice.id = NoticeId::new();
        notice.holder = to.clone();
        publish(tx, &notice)?;
        tx.conn()
            .execute("DELETE FROM notices WHERE row_id = ?1", [row])?;
        moved += 1;
    }
    Ok(moved)
}
