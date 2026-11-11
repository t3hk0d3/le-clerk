//! Retention: what the database stops keeping, and when.

use crate::store::{StoreError, Tx};

const DAY_MS: i64 = 86_400_000;

/// How long the database keeps what is finished with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    /// Days an ended lease and an audit entry are kept.
    pub lease_days: u64,
    /// Days a delivered notice, and a notice for a holder that is gone, is
    /// kept.
    pub notice_days: u64,
}

/// How many rows a sweep deleted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Swept {
    /// Notices.
    pub notices: usize,
    /// Ended leases.
    pub leases: usize,
    /// Audit entries.
    pub audit: usize,
}

/// `days` before `now_ms`, unix ms.
fn cutoff(now_ms: i64, days: u64) -> i64 {
    let span = i64::try_from(days)
        .unwrap_or(i64::MAX)
        .saturating_mul(DAY_MS);
    now_ms.saturating_sub(span)
}

const DELIVERED_NOTICES: &str = "FROM notices WHERE delivered_at IS NOT NULL AND delivered_at < ?1";

const ABANDONED_NOTICES: &str = "FROM notices WHERE delivered_at IS NULL AND created_at < ?1 \
     AND holder NOT IN (SELECT holder FROM leases WHERE state != 'ended') \
     AND holder NOT IN (SELECT holder FROM requests) \
     AND NOT (json_extract(holder, '$.kind') = 'agent_session' \
              AND json_extract(holder, '$.session_id') IN (SELECT session_id FROM agent_hosts))";

const ENDED_LEASES: &str = "FROM leases WHERE state = 'ended' \
     AND created_at < ?1 AND COALESCE(ends_at, created_at) < ?1 \
     AND row_id NOT IN (SELECT borrower_lease FROM leases \
                        WHERE state != 'ended' AND borrower_lease IS NOT NULL) \
     AND row_id NOT IN (SELECT paused_for_lease FROM leases \
                        WHERE state != 'ended' AND paused_for_lease IS NOT NULL) \
     AND row_id NOT IN (SELECT override_lease FROM requests WHERE override_lease IS NOT NULL)";

const OLD_AUDIT: &str = "FROM audit WHERE at < ?1";

/// Delete the rows `from_where` (a `FROM … WHERE …` clause taking `cutoff`
/// as `?1`) selects. A read finds out first whether there are any: a sweep
/// with nothing to do must not take the database's write lock.
fn delete_from(tx: &Tx, from_where: &str, cutoff: i64) -> Result<usize, StoreError> {
    let any: bool = tx.conn().query_row(
        &format!("SELECT EXISTS (SELECT 1 {from_where})"),
        [cutoff],
        |r| r.get(0),
    )?;
    if !any {
        return Ok(0);
    }
    Ok(tx
        .conn()
        .execute(&format!("DELETE {from_where}"), [cutoff])?)
}

/// Delete what is past `keep` at `now_ms`:
///
/// - notices delivered more than `notice_days` ago;
/// - notices still undelivered after that long whose holder has no live
///   lease, no queued request and (for an agent session) no host: nobody is
///   left to take them;
/// - leases ended more than `lease_days` ago, unless a live lease or a
///   queued request still points at them (their yield asks go with them);
/// - audit entries older than `lease_days`.
///
/// A live lease, and a notice a live holder has not been given, are never
/// deleted. The caller runs this in one transaction.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn sweep(tx: &Tx, now_ms: i64, keep: &Retention) -> Result<Swept, StoreError> {
    let notice_cutoff = cutoff(now_ms, keep.notice_days);
    let lease_cutoff = cutoff(now_ms, keep.lease_days);
    let delivered = delete_from(tx, DELIVERED_NOTICES, notice_cutoff)?;
    let abandoned = delete_from(tx, ABANDONED_NOTICES, notice_cutoff)?;
    let leases = delete_from(tx, ENDED_LEASES, lease_cutoff)?;
    let audit = delete_from(tx, OLD_AUDIT, lease_cutoff)?;
    Ok(Swept {
        notices: delivered + abandoned,
        leases,
        audit,
    })
}
