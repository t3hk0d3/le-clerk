// Shared by every test crate; each one uses only a subset.
#![allow(dead_code)]

use assert_cmd::Command;
use leclerk::model::{Resource, Unit};
use leclerk::store::Tx;
use tempfile::TempDir;

/// An isolated clerk environment: every command it builds runs with
/// `CLERK_HOME` pointing into a private tempdir.
pub struct TestEnv {
    home: TempDir,
}

impl TestEnv {
    pub fn new() -> Self {
        Self {
            home: TempDir::new().expect("create tempdir"),
        }
    }

    pub fn home(&self) -> &std::path::Path {
        self.home.path()
    }

    /// A `clerk` command bound to this environment.
    pub fn cmd(&self) -> Command {
        let mut cmd = Command::cargo_bin("clerk").expect("clerk binary");
        cmd.env("CLERK_HOME", self.home.path());
        cmd
    }
}

/// Insert a lease row for `unit` directly, bypassing the scheduler.
/// `state` is the stored text of a `LeaseState`, e.g. `"held"`.
pub fn insert_lease(tx: &Tx, unit: &Unit, state: &str) {
    tx.conn()
        .execute(
            "INSERT INTO leases (lease_id, unit_row, resource_row, state, holder, priority, \
             interruptible, purpose, duration_ms, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'normal', 0, 'test', 60000, 0)",
            rusqlite::params![
                leclerk::model::LeaseId::new().as_str(),
                unit.row,
                unit.resource_row,
                state,
                r#"{"kind":"user","tag":"holder"}"#,
            ],
        )
        .expect("insert lease");
}

/// Insert a queued request row for `resource`, optionally pinned to `unit`.
pub fn insert_queued_request(tx: &Tx, resource: &Resource, unit: Option<&Unit>) {
    tx.conn()
        .execute(
            "INSERT INTO requests (request_id, resource_row, unit_row, state, holder, priority, \
             interruptible, purpose, duration_ms, created_at) \
             VALUES (?1, ?2, ?3, 'queued', ?4, 'normal', 0, 'test', 60000, 0)",
            rusqlite::params![
                leclerk::model::RequestId::new().as_str(),
                resource.row,
                unit.map(|u| u.row),
                r#"{"kind":"user","tag":"waiter"}"#,
            ],
        )
        .expect("insert request");
}
