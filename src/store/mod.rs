//! SQLite persistence. All SQL lives under this module.

pub mod audit;
pub mod inventory;
pub mod leases;
pub mod notices;

use std::fs;
use std::path::Path;

use rusqlite::Connection;
use rusqlite_migration::{M, Migrations};
use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

use crate::error::ErrorCode;

/// A storage failure.
#[derive(Debug, Error)]
pub enum StoreError {
    /// SQLite reported an error.
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// Applying the schema migrations failed.
    #[error("migration error: {0}")]
    Migration(#[from] rusqlite_migration::Error),
    /// A JSON column could not be encoded or decoded.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// The database directory could not be created.
    #[error("cannot prepare database directory: {0}")]
    Io(#[from] std::io::Error),
    /// A stored value does not parse; the database is damaged.
    #[error("corrupt value in column {0}")]
    Corrupt(&'static str),
}

impl ErrorCode for StoreError {
    fn code(&self) -> &'static str {
        match self {
            StoreError::Sqlite(_) => "database_error",
            StoreError::Migration(_) => "migration_error",
            StoreError::Json(_) => "json_error",
            StoreError::Io(_) => "io_error",
            StoreError::Corrupt(_) => "corrupt_database",
        }
    }
}

fn migrations() -> Migrations<'static> {
    Migrations::new(vec![
        M::up(include_str!("migrations/0001_init.sql")),
        M::up(include_str!("migrations/0002_link_origin.sql")),
        M::up(include_str!("migrations/0003_scheduler.sql")),
        M::up(include_str!("migrations/0004_lease_request.sql")),
        M::up(include_str!("migrations/0005_lease_lifecycle.sql")),
        M::up(include_str!("migrations/0006_interruption.sql")),
        M::up(include_str!("migrations/0007_link_suppression.sql")),
        M::up(include_str!("migrations/0008_cleanup_ending.sql")),
        M::up(include_str!("migrations/0009_override_reason.sql")),
    ])
}

/// The database connection.
#[derive(Debug)]
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating if needed) the database at `path` and migrate it.
    ///
    /// # Errors
    /// [`StoreError`] when the file cannot be created, opened or migrated.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Self::init(Connection::open(path)?)
    }

    /// A private in-memory database, for tests.
    ///
    /// # Errors
    /// [`StoreError`] when migration fails.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self, StoreError> {
        // journal_mode returns a row, so it must be read as a query.
        conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        migrations().to_latest(&mut conn)?;
        Ok(Self { conn })
    }

    /// Run `f` in one transaction: commit on `Ok`, roll back on `Err`.
    ///
    /// # Errors
    /// Whatever `f` returns, or a [`StoreError`] (converted into `E`) when
    /// the transaction cannot begin or commit.
    pub fn tx<T, E: From<StoreError>>(
        &mut self,
        f: impl FnOnce(&Tx<'_>) -> Result<T, E>,
    ) -> Result<T, E> {
        let inner = self.conn.transaction().map_err(StoreError::from)?;
        let tx = Tx { inner };
        let value = f(&tx)?;
        tx.inner.commit().map_err(StoreError::from)?;
        Ok(value)
    }
}

/// An open transaction. Dropping it without commit rolls back.
#[derive(Debug)]
pub struct Tx<'a> {
    inner: rusqlite::Transaction<'a>,
}

impl Tx<'_> {
    /// The underlying connection, for the SQL modules of this crate and for
    /// test fixtures.
    #[must_use]
    pub fn conn(&self) -> &Connection {
        &self.inner
    }
}

/// Encode a unit-variant enum as its serde text (without quotes).
pub(crate) fn enum_text<T: Serialize>(value: &T) -> Result<String, StoreError> {
    match serde_json::to_value(value)? {
        serde_json::Value::String(s) => Ok(s),
        _ => Err(StoreError::Corrupt("enum")),
    }
}

/// Decode text written by [`enum_text`].
pub(crate) fn parse_enum<T: DeserializeOwned>(
    text: &str,
    column: &'static str,
) -> Result<T, StoreError> {
    serde_json::from_value(serde_json::Value::String(text.to_owned()))
        .map_err(|_| StoreError::Corrupt(column))
}
