//! Persisted agent-session hosts (`agent_hosts`).

use crate::model::Host;
use crate::store::{StoreError, Tx};

fn start(host: Host) -> i64 {
    i64::try_from(host.start_time).unwrap_or(i64::MAX)
}

/// Record `host` as one of the processes running `session_id`.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn add_host(tx: &Tx, session_id: &str, host: Host, now_ms: i64) -> Result<(), StoreError> {
    tx.conn().execute(
        "INSERT OR REPLACE INTO agent_hosts (session_id, pid, start_time, last_seen) \
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![session_id, host.pid, start(host), now_ms],
    )?;
    Ok(())
}

/// Record that `host` no longer runs `session_id`.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn remove_host(tx: &Tx, session_id: &str, host: Host) -> Result<(), StoreError> {
    tx.conn().execute(
        "DELETE FROM agent_hosts WHERE session_id = ?1 AND pid = ?2 AND start_time = ?3",
        rusqlite::params![session_id, host.pid, start(host)],
    )?;
    Ok(())
}

/// Forget every host of `session_id`.
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

/// Every (session, host) pair recorded.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn hosts(tx: &Tx) -> Result<Vec<(String, Host)>, StoreError> {
    let mut stmt = tx
        .conn()
        .prepare("SELECT session_id, pid, start_time FROM agent_hosts ORDER BY session_id")?;
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
