//! Notices: events stored for a holder until it reads them.
//!
//! The whole notice is kept as JSON in `payload`; the foreign-key columns of
//! the table stay empty so that a notice outlives the unit or lease it talks
//! about (a `QueueDropped` for a removed unit must still reach its holder).

use serde::{Deserialize, Serialize};

use crate::model::{
    Caller, DirtyInfo, HolderRef, LeaseId, LeaseState, NoticeId, RequestId, ResourceId, UnitId,
};
use crate::store::{StoreError, Tx, audit, enum_text, leases};

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

/// The notice in `payload`; `None`, logged, when it does not decode. Such a
/// row stays where it is, undelivered.
fn decode(tx: &Tx, row: i64, payload: &str) -> Option<Notice> {
    match serde_json::from_str(payload) {
        Ok(notice) => Some(notice),
        Err(e) => {
            tx.log_skipped("notices", row, &e);
            None
        }
    }
}

/// `(row_id, notice)` of the first `limit` rows of `(row_id, payload)` that
/// decode. Rows after those are not read, and rows that do not decode do not
/// count: however many there are, they cannot hold up the notices behind.
fn decode_up_to(
    tx: &Tx,
    mut rows: rusqlite::Rows<'_>,
    limit: u32,
) -> Result<Vec<(i64, Notice)>, StoreError> {
    let mut out = Vec::new();
    while out.len() < limit as usize {
        let Some(r) = rows.next()? else {
            break;
        };
        let row: i64 = r.get(0)?;
        let payload: String = r.get(1)?;
        if let Some(notice) = decode(tx, row, &payload) {
            out.push((row, notice));
        }
    }
    Ok(out)
}

/// Undelivered notices of `holder`, oldest first, without marking them.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn pending(tx: &Tx, holder: &HolderRef) -> Result<Vec<Notice>, StoreError> {
    let mut stmt = tx.conn().prepare(
        "SELECT row_id, payload FROM notices WHERE holder = ?1 AND delivered_at IS NULL \
         ORDER BY row_id",
    )?;
    let rows = stmt.query([serde_json::to_string(holder)?])?;
    Ok(decode_up_to(tx, rows, u32::MAX)?
        .into_iter()
        .map(|(_, n)| n)
        .collect())
}

/// Take up to `limit` undelivered notices of `holder`, in insertion order
/// (never by the clock or by id), and mark them delivered in the same
/// transaction. The holder matches structurally: its JSON encoding is the
/// serde derive's, whose field order is fixed. `delivered_at` is set to
/// `now_ms`.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn take(
    tx: &Tx,
    holder: &HolderRef,
    limit: u32,
    now_ms: i64,
) -> Result<Vec<Notice>, StoreError> {
    take_matching(tx, holder, None, limit, now_ms)
}

/// Which of a holder's notices a wait is about: those naming one of these
/// leases or requests and, with `kinds`, of one of those kinds. Used by a
/// CLI command that acts for an agent session, so the session's other
/// notices stay pending for it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoticeFilter {
    pub leases: Vec<LeaseId>,
    pub requests: Vec<RequestId>,
    /// `None`: every kind.
    #[serde(default)]
    pub kinds: Option<Vec<NoticeKind>>,
}

impl NoticeFilter {
    /// True when `n` names one of the filter's leases or requests and is
    /// of one of its kinds.
    #[must_use]
    pub fn matches(&self, n: &Notice) -> bool {
        let named = n.lease_id.as_ref().is_some_and(|l| self.leases.contains(l))
            || n.request_id
                .as_ref()
                .is_some_and(|r| self.requests.contains(r));
        named && self.kinds.as_ref().is_none_or(|k| k.contains(&n.kind))
    }
}

/// [`take`], but with `filter` only the notices it matches are taken (up to
/// `limit`, oldest first); the others stay pending. The filter runs in SQL
/// on the stored payload, so only the rows taken are read and parsed. A row
/// that does not decode is logged and left undelivered.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn take_matching(
    tx: &Tx,
    holder: &HolderRef,
    filter: Option<&NoticeFilter>,
    limit: u32,
    now_ms: i64,
) -> Result<Vec<Notice>, StoreError> {
    let rows = live(tx, select_pending(tx, holder, filter, limit)?, now_ms)?;
    let mut mark = tx
        .conn()
        .prepare("UPDATE notices SET delivered_at = ?2 WHERE row_id = ?1")?;
    rows.into_iter()
        .map(|(row, notice)| {
            mark.execute(rusqlite::params![row, now_ms])?;
            Ok(notice)
        })
        .collect()
}

/// Whether later state made `notice` obsolete, judged from the current state
/// of the lease it names (a notice naming no lease, or one that is gone,
/// stands):
/// - a cleanup notice (`CleanupWindowStarted`, `CleanupEnding`) for a lease
///   no longer cleaning;
/// - an `Offered` for a lease no longer offered;
/// - an `ExpiryWarning` for a lease that ended, started cleaning, or was
///   extended past the end the warning was about. A lease overdue under
///   soft expiry still has that end: its warning is live.
fn obsolete(tx: &Tx, notice: &Notice) -> Result<bool, StoreError> {
    let Some(id) = &notice.lease_id else {
        return Ok(false);
    };
    let Some(lease) = leases::find_lease(tx, id)? else {
        return Ok(false);
    };
    Ok(match notice.kind {
        NoticeKind::CleanupWindowStarted | NoticeKind::CleanupEnding => {
            lease.state != LeaseState::Cleaning
        }
        NoticeKind::Offered => lease.state != LeaseState::Offered,
        NoticeKind::ExpiryWarning => {
            matches!(lease.state, LeaseState::Ended | LeaseState::Cleaning)
                || matches!((lease.ends_at, notice.deadline), (Some(end), Some(warned)) if end > warned)
        }
        _ => false,
    })
}

/// The rows of `rows` that are not [`obsolete`]. The others are marked
/// delivered at `now_ms` and audited as `superseded`, so they are neither
/// returned now nor pending later.
fn live(tx: &Tx, rows: Vec<(i64, Notice)>, now_ms: i64) -> Result<Vec<(i64, Notice)>, StoreError> {
    let mut keep = Vec::with_capacity(rows.len());
    for (row, notice) in rows {
        if !obsolete(tx, &notice)? {
            keep.push((row, notice));
            continue;
        }
        tx.conn().execute(
            "UPDATE notices SET delivered_at = ?2 WHERE row_id = ?1",
            rusqlite::params![row, now_ms],
        )?;
        let clerk = Caller {
            holder: HolderRef::User {
                tag: "clerk".to_owned(),
            },
            project: None,
            override_reason: None,
        };
        audit::record(
            tx,
            now_ms,
            &clerk,
            "superseded",
            &serde_json::json!({
                "notice": notice.id,
                "kind": notice.kind,
                "holder": notice.holder,
                "lease": notice.lease_id,
            }),
        )?;
    }
    Ok(keep)
}

/// Acknowledged delivery (MCP `lease_wait`): up to `limit` undelivered
/// notices of `holder`, oldest first, returned **without** marking them
/// delivered. Each is marked returned (`returned_at = now_ms`, kept from
/// its first return), and stays pending until [`ack`] names it, so a result
/// the client threw away loses nothing. With `filter`, only the notices it
/// matches.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn returned(
    tx: &Tx,
    holder: &HolderRef,
    filter: Option<&NoticeFilter>,
    limit: u32,
    now_ms: i64,
) -> Result<Vec<Notice>, StoreError> {
    let rows = live(tx, select_pending(tx, holder, filter, limit)?, now_ms)?;
    let mut mark = tx
        .conn()
        .prepare("UPDATE notices SET returned_at = COALESCE(returned_at, ?2) WHERE row_id = ?1")?;
    rows.into_iter()
        .map(|(row, notice)| {
            mark.execute(rusqlite::params![row, now_ms])?;
            Ok(notice)
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

/// Undelivered notices of `holder` that no hook has shown yet, oldest
/// first, changing nothing. Showing is not delivery: see [`mark_shown`].
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn unshown(tx: &Tx, holder: &HolderRef) -> Result<Vec<Notice>, StoreError> {
    let mut stmt = tx.conn().prepare(
        "SELECT row_id, payload FROM notices WHERE holder = ?1 AND delivered_at IS NULL \
         AND hook_shown_at IS NULL ORDER BY row_id",
    )?;
    let rows = stmt.query([serde_json::to_string(holder)?])?;
    Ok(decode_up_to(tx, rows, u32::MAX)?
        .into_iter()
        .map(|(_, n)| n)
        .collect())
}

/// Mark those of `ids` that are undelivered, not yet shown notices of
/// `holder` as shown by a hook (`hook_shown_at = now_ms`); other ids are
/// ignored. The notices stay pending: delivery is unaffected. Returns how
/// many were marked.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn mark_shown(
    tx: &Tx,
    holder: &HolderRef,
    ids: &[NoticeId],
    now_ms: i64,
) -> Result<usize, StoreError> {
    let ids: Vec<&str> = ids.iter().map(NoticeId::as_str).collect();
    Ok(tx.conn().execute(
        "UPDATE notices SET hook_shown_at = ?3 WHERE holder = ?1 AND delivered_at IS NULL \
         AND hook_shown_at IS NULL AND notice_id IN (SELECT value FROM json_each(?2))",
        rusqlite::params![
            serde_json::to_string(holder)?,
            serde_json::to_string(&ids)?,
            now_ms
        ],
    )?)
}

/// `(row_id, notice)` of up to `limit` undelivered notices of `holder`,
/// oldest first; with `filter`, only those it matches. The filter runs in
/// SQL on the stored payload, so only the rows selected are read. There is
/// no SQL `LIMIT`: rows that do not decode must not use it up.
fn select_pending(
    tx: &Tx,
    holder: &HolderRef,
    filter: Option<&NoticeFilter>,
    limit: u32,
) -> Result<Vec<(i64, Notice)>, StoreError> {
    let holder = serde_json::to_string(holder)?;
    match filter {
        None => {
            let mut stmt = tx.conn().prepare(
                "SELECT row_id, payload FROM notices WHERE holder = ?1 AND delivered_at IS NULL \
                 ORDER BY row_id",
            )?;
            decode_up_to(tx, stmt.query([holder])?, limit)
        }
        Some(f) => {
            // CASE keeps json_extract away from payloads that are not JSON.
            let mut stmt = tx.conn().prepare(
                "SELECT row_id, payload FROM notices WHERE holder = ?1 AND delivered_at IS NULL \
                 AND (CASE WHEN json_valid(payload) THEN \
                        json_extract(payload, '$.lease_id') IN (SELECT value FROM json_each(?2)) \
                        OR json_extract(payload, '$.request_id') IN (SELECT value FROM json_each(?3)) \
                      ELSE 0 END) \
                 AND (?4 IS NULL OR kind IN (SELECT value FROM json_each(?4))) \
                 ORDER BY row_id",
            )?;
            let rows = stmt.query(rusqlite::params![
                holder,
                serde_json::to_string(&f.leases)?,
                serde_json::to_string(&f.requests)?,
                f.kinds.as_ref().map(serde_json::to_string).transpose()?,
            ])?;
            decode_up_to(tx, rows, limit)
        }
    }
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
/// one lease's notices, keeping whether a hook already showed it (the same
/// conversation took over). A row that does not decode stays behind, logged.
/// Returns how many moved.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn rehome_all(tx: &Tx, from: &HolderRef, to: &HolderRef) -> Result<usize, StoreError> {
    let rows = {
        let mut stmt = tx.conn().prepare(
            "SELECT row_id, payload, hook_shown_at FROM notices WHERE holder = ?1 \
             AND delivered_at IS NULL ORDER BY row_id",
        )?;
        stmt.query_map([serde_json::to_string(from)?], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<i64>>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?
    };
    let mut moved = 0;
    for (row, payload, shown) in &rows {
        let Some(mut notice) = decode(tx, *row, payload) else {
            continue;
        };
        notice.id = NoticeId::new();
        notice.holder = to.clone();
        publish(tx, &notice)?;
        if shown.is_some() {
            tx.conn().execute(
                "UPDATE notices SET hook_shown_at = ?2 WHERE notice_id = ?1",
                rusqlite::params![notice.id.as_str(), shown],
            )?;
        }
        tx.conn()
            .execute("DELETE FROM notices WHERE row_id = ?1", [row])?;
        moved += 1;
    }
    Ok(moved)
}

/// Hand the undelivered notices of `from` about `lease` to `to`. Each one is
/// written again as a new row, under a fresh id, before the old row goes:
/// the new row id is then above every row written so far, which is what
/// wakes the new holder's waiters. (Deleting first would let SQLite reuse
/// the old, already-seen row id.) They keep their order among themselves and
/// come after what `to` already has. A row that does not decode stays
/// behind, logged. Returns how many moved.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn rehome(
    tx: &Tx,
    from: &HolderRef,
    lease: &LeaseId,
    to: &HolderRef,
) -> Result<usize, StoreError> {
    let rows = {
        let mut stmt = tx.conn().prepare(
            "SELECT row_id, payload FROM notices WHERE holder = ?1 AND delivered_at IS NULL \
             ORDER BY row_id",
        )?;
        stmt.query_map([serde_json::to_string(from)?], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?
    };
    let mut moved = 0;
    for (row, payload) in rows {
        let Some(mut notice) = decode(tx, row, &payload) else {
            continue;
        };
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
