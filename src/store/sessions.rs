//! Persisted agent-session resume aliases (`session_aliases`).

use crate::store::{StoreError, Tx};

/// Record that session `resumed` now stands for `holder` (replacing an
/// earlier record for `resumed`).
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn insert_alias(tx: &Tx, resumed: &str, holder: &str, now_ms: i64) -> Result<(), StoreError> {
    tx.conn().execute(
        "INSERT OR REPLACE INTO session_aliases (resumed, holder, created_at) \
         VALUES (?1, ?2, ?3)",
        rusqlite::params![resumed, holder, now_ms],
    )?;
    Ok(())
}

/// Every alias as `(resumed, holder)`, oldest first.
///
/// # Errors
/// [`StoreError`] on SQL failure.
pub fn aliases(tx: &Tx) -> Result<Vec<(String, String)>, StoreError> {
    let mut stmt = tx
        .conn()
        .prepare("SELECT resumed, holder FROM session_aliases ORDER BY created_at, resumed")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}
