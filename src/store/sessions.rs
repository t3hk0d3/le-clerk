//! Persisted agent-session hosts (`agent_hosts`).

use crate::model::Host;
use crate::store::{StoreError, Tx};

/// Record `host` as the process running `session_id`.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn set_host(tx: &Tx, session_id: &str, host: Host, now_ms: i64) -> Result<(), StoreError> {
    tx.conn().execute(
        "INSERT OR REPLACE INTO agent_hosts (session_id, pid, start_time, last_seen) \
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![
            session_id,
            host.pid,
            i64::try_from(host.start_time).unwrap_or(i64::MAX),
            now_ms
        ],
    )?;
    Ok(())
}

/// Record that no process runs `session_id` any more.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn clear_host(tx: &Tx, session_id: &str, now_ms: i64) -> Result<(), StoreError> {
    tx.conn().execute(
        "INSERT OR REPLACE INTO agent_hosts (session_id, pid, start_time, last_seen) \
         VALUES (?1, NULL, NULL, ?2)",
        rusqlite::params![session_id, now_ms],
    )?;
    Ok(())
}

/// Forget `session_id`.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn forget(tx: &Tx, session_id: &str) -> Result<(), StoreError> {
    tx.conn().execute(
        "DELETE FROM agent_hosts WHERE session_id = ?1",
        [session_id],
    )?;
    Ok(())
}

/// Every session with a recorded host.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn hosts(tx: &Tx) -> Result<Vec<(String, Host)>, StoreError> {
    let mut stmt = tx.conn().prepare(
        "SELECT session_id, pid, start_time FROM agent_hosts \
         WHERE pid IS NOT NULL ORDER BY session_id",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                Host {
                    pid: r.get(1)?,
                    start_time: u64::try_from(r.get::<_, i64>(2)?).unwrap_or(0),
                },
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}
