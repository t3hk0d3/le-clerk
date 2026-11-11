//! SQLite persistence. All SQL lives under this module.

pub mod audit;
pub mod inventory;
pub mod leases;
pub mod notices;
pub mod retention;
pub mod sessions;

use std::cell::RefCell;
use std::collections::HashSet;
use std::fmt::Display;
use std::fs;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension};
use rusqlite_migration::{M, MigrationDefinitionError, Migrations};
use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

use crate::error::ErrorCode;

/// What this binary's stored values can say: enum texts and JSON shapes. Bump
/// it when a change writes a value an older binary could not read.
pub const VOCABULARY_VERSION: i64 = 1;

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
    /// A newer binary wrote values this one cannot read.
    #[error(
        "the clerk database has vocabulary {found} but this clerk knows only {known}: a newer \
         clerk wrote it; run that clerk instead"
    )]
    TooNew {
        /// The database's vocabulary version.
        found: i64,
        /// This binary's ([`VOCABULARY_VERSION`]).
        known: i64,
    },
    /// A newer binary migrated the schema past the last migration this one
    /// knows.
    #[error(
        "the clerk database schema is newer than this clerk knows: a newer clerk migrated it; \
         run that clerk instead"
    )]
    SchemaTooNew,
}

impl StoreError {
    /// Whether a newer binary wrote the database, by vocabulary or schema,
    /// so that this one must leave it alone.
    #[must_use]
    pub fn is_too_new(&self) -> bool {
        matches!(self, Self::TooNew { .. } | Self::SchemaTooNew)
    }
}

impl ErrorCode for StoreError {
    fn code(&self) -> &'static str {
        match self {
            StoreError::Sqlite(_) => "database_error",
            StoreError::Migration(_) => "migration_error",
            StoreError::Json(_) => "json_error",
            StoreError::Io(_) => "io_error",
            StoreError::Corrupt(_) => "corrupt_database",
            StoreError::TooNew { .. } | StoreError::SchemaTooNew => "store_too_new",
        }
    }
}

/// The schema migrations, oldest first. Public so tests can stop at an
/// earlier version.
#[must_use]
pub fn migrations() -> Migrations<'static> {
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
        M::up(include_str!("migrations/0010_description_usage.sql")),
        M::up(include_str!("migrations/0011_resource_tags.sql")),
        M::up(include_str!("migrations/0012_notice_returned.sql")),
        M::up(include_str!("migrations/0013_session_aliases.sql")),
        M::up(include_str!("migrations/0014_notice_hook_shown.sql")),
        M::up(include_str!("migrations/0015_agent_hosts.sql")),
        M::up(include_str!("migrations/0016_request_skip_checks.sql")),
        M::up(include_str!("migrations/0017_lease_overdue.sql")),
        M::up(include_str!("migrations/0018_vocabulary.sql")),
        M::up(include_str!("migrations/0019_retention.sql")),
    ])
}

/// The database connection.
#[derive(Debug)]
pub struct Store {
    conn: Connection,
    /// Rows already logged as undecodable, by table and row id: reads repeat
    /// every tick, the log should not.
    skipped: RefCell<HashSet<(&'static str, i64)>>,
}

impl Store {
    /// Open (creating if needed) the database at `path` and migrate it.
    ///
    /// # Errors
    /// [`StoreError`] when the file cannot be created, opened or migrated;
    /// [`StoreError::TooNew`] or [`StoreError::SchemaTooNew`] when a newer
    /// binary wrote it.
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
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // journal_mode returns a row, so it must be read as a query.
        conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // Before migrating: a binary this old must leave the database as it
        // found it.
        let stored = stored_vocabulary(&conn)?;
        if let Some(found) = stored.filter(|v| *v > VOCABULARY_VERSION) {
            return Err(StoreError::TooNew {
                found,
                known: VOCABULARY_VERSION,
            });
        }
        migrations().to_latest(&mut conn).map_err(|e| match e {
            rusqlite_migration::Error::MigrationDefinition(
                MigrationDefinitionError::DatabaseTooFarAhead,
            ) => StoreError::SchemaTooNew,
            other => other.into(),
        })?;
        if stored_vocabulary(&conn)? != Some(VOCABULARY_VERSION) {
            conn.execute(
                "INSERT OR REPLACE INTO store_vocabulary (id, vocabulary_version) VALUES (1, ?1)",
                [VOCABULARY_VERSION],
            )?;
        }
        Ok(Self {
            conn,
            skipped: RefCell::default(),
        })
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
        let tx = Tx {
            inner,
            skipped: &self.skipped,
        };
        let value = f(&tx)?;
        tx.inner.commit().map_err(StoreError::from)?;
        Ok(value)
    }
}

/// An open transaction. Dropping it without commit rolls back.
#[derive(Debug)]
pub struct Tx<'a> {
    inner: rusqlite::Transaction<'a>,
    skipped: &'a RefCell<HashSet<(&'static str, i64)>>,
}

impl Tx<'_> {
    /// The underlying connection, for the SQL modules of this crate and for
    /// test fixtures.
    #[must_use]
    pub fn conn(&self) -> &Connection {
        &self.inner
    }

    /// Log that row `row` of `table` does not decode and is being skipped,
    /// the first time this store meets it.
    pub(crate) fn log_skipped(&self, table: &'static str, row: i64, error: &dyn Display) {
        if self.skipped.borrow_mut().insert((table, row)) {
            tracing::warn!(table, row, error = %error, "skipping a row that does not decode");
        }
    }
}

/// The vocabulary `conn`'s database records; `None` when it has no record
/// (not migrated that far, or the row is gone).
fn stored_vocabulary(conn: &Connection) -> Result<Option<i64>, StoreError> {
    let has_column: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info('store_vocabulary') \
         WHERE name = 'vocabulary_version')",
        [],
        |r| r.get(0),
    )?;
    if !has_column {
        return Ok(None);
    }
    Ok(conn
        .query_row(
            "SELECT vocabulary_version FROM store_vocabulary WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .optional()?)
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
