//! The retention sweep: what it deletes, and what it never touches.

use leclerk::model::{HolderRef, LeaseId, NoticeId};
use leclerk::store::retention::{self, Retention, Swept};
use leclerk::store::{Store, StoreError, Tx};
use rusqlite::params;

const DAY_MS: i64 = 86_400_000;
/// Far enough from the epoch that "91 days ago" is a positive time.
const NOW: i64 = 400 * DAY_MS;
const KEEP: Retention = Retention {
    lease_days: 90,
    notice_days: 7,
};

fn user(tag: &str) -> String {
    serde_json::to_string(&HolderRef::User { tag: tag.into() }).unwrap()
}

fn agent(session: &str) -> String {
    serde_json::to_string(&HolderRef::AgentSession {
        session_id: session.into(),
    })
    .unwrap()
}

fn days_ago(days: i64) -> i64 {
    NOW - days * DAY_MS
}

/// A store with one resource `board` and one unit `u1`.
fn store() -> Store {
    let mut store = Store::open_in_memory().unwrap();
    store
        .tx(|tx| -> Result<(), StoreError> {
            tx.conn().execute_batch(
                "INSERT INTO resources (row_id, id, scope_kind, type, created_at) \
                 VALUES (1, 'board', 'user', 'generic', 0); \
                 INSERT INTO units (row_id, resource_row, id, settings, created_at) \
                 VALUES (1, 1, 'u1', '{}', 0);",
            )?;
            Ok(())
        })
        .unwrap();
    store
}

/// A lease of `holder` on `u1` in `state`, created and ending `days` ago.
fn lease(tx: &Tx, holder: &str, state: &str, days: i64) -> i64 {
    tx.conn()
        .execute(
            "INSERT INTO leases (lease_id, unit_row, resource_row, state, holder, priority, \
             interruptible, purpose, duration_ms, created_at, ends_at) \
             VALUES (?1, 1, 1, ?2, ?3, 'normal', 0, 'test', 60000, ?4, ?4)",
            params![LeaseId::new().as_str(), state, holder, days_ago(days)],
        )
        .unwrap();
    tx.conn().last_insert_rowid()
}

/// A notice for `holder`, created `created` days ago, delivered `delivered`
/// days ago (or not).
fn notice(tx: &Tx, holder: &str, created: i64, delivered: Option<i64>) -> i64 {
    tx.conn()
        .execute(
            "INSERT INTO notices (notice_id, holder, kind, payload, created_at, delivered_at) \
             VALUES (?1, ?2, 'offered', '{}', ?3, ?4)",
            params![
                NoticeId::new().as_str(),
                holder,
                days_ago(created),
                delivered.map(days_ago)
            ],
        )
        .unwrap();
    tx.conn().last_insert_rowid()
}

fn audit(tx: &Tx, days: i64) -> i64 {
    tx.conn()
        .execute(
            "INSERT INTO audit (at, actor, action) VALUES (?1, '{}', 'test')",
            [days_ago(days)],
        )
        .unwrap();
    tx.conn().last_insert_rowid()
}

fn exists(tx: &Tx, table: &str, row: i64) -> bool {
    tx.conn()
        .query_row(
            &format!("SELECT EXISTS (SELECT 1 FROM {table} WHERE row_id = ?1)"),
            [row],
            |r| r.get(0),
        )
        .unwrap()
}

/// Whether row `row` of `table` is still there.
fn present(store: &mut Store, table: &str, row: i64) -> bool {
    store
        .tx(|tx| Ok::<_, StoreError>(exists(tx, table, row)))
        .unwrap()
}

fn sweep(store: &mut Store) -> Swept {
    store.tx(|tx| retention::sweep(tx, NOW, &KEEP)).unwrap()
}

mod sweep {
    use super::*;

    mod on_notices_delivered_8_days_ago {
        use super::*;

        #[test]
        fn deletes_them() {
            let mut s = store();
            let old = s
                .tx(|tx| Ok::<_, StoreError>(notice(tx, &user("a"), 9, Some(8))))
                .unwrap();
            let swept = sweep(&mut s);
            assert!(!present(&mut s, "notices", old));
            assert_eq!(swept.notices, 1);
        }

        #[test]
        fn deletes_them_even_for_a_live_holder() {
            let mut s = store();
            let old = s
                .tx(|tx| {
                    lease(tx, &user("a"), "held", 1);
                    Ok::<_, StoreError>(notice(tx, &user("a"), 9, Some(8)))
                })
                .unwrap();
            sweep(&mut s);
            assert!(!present(&mut s, "notices", old));
        }
    }

    mod on_notices_delivered_6_days_ago {
        use super::*;

        #[test]
        fn keeps_them() {
            let mut s = store();
            let recent = s
                .tx(|tx| Ok::<_, StoreError>(notice(tx, &user("a"), 9, Some(6))))
                .unwrap();
            sweep(&mut s);
            assert!(present(&mut s, "notices", recent));
        }
    }

    mod on_an_undelivered_notice_8_days_old {
        use super::*;

        fn survives(holder: &str, setup: impl FnOnce(&Tx)) -> bool {
            let mut s = store();
            let row = s
                .tx(|tx| {
                    setup(tx);
                    Ok::<_, StoreError>(notice(tx, holder, 8, None))
                })
                .unwrap();
            sweep(&mut s);
            present(&mut s, "notices", row)
        }

        #[test]
        fn deletes_it_when_the_holder_is_unknown() {
            assert!(!survives(&user("nobody"), |_| {}));
        }

        #[test]
        fn deletes_it_when_the_holders_only_lease_ended() {
            assert!(!survives(&user("a"), |tx| {
                lease(tx, &user("a"), "ended", 2);
            }));
        }

        #[test]
        fn keeps_it_when_the_holder_has_a_live_lease() {
            let holder = user("a");
            assert!(survives(&holder, |tx| {
                lease(tx, &user("a"), "held", 1);
            }));
        }

        #[test]
        fn keeps_it_when_the_holder_has_a_queued_request() {
            let holder = user("a");
            assert!(survives(&holder, |tx| {
                tx.conn()
                    .execute(
                        "INSERT INTO requests (request_id, resource_row, state, holder, \
                         priority, interruptible, purpose, duration_ms, created_at) \
                         VALUES ('r1', 1, 'queued', ?1, 'normal', 0, 'test', 60000, 0)",
                        [user("a")],
                    )
                    .unwrap();
            }));
        }

        #[test]
        fn keeps_it_when_an_agent_session_still_has_a_host() {
            let holder = agent("s1");
            assert!(survives(&holder, |tx| {
                tx.conn()
                    .execute(
                        "INSERT INTO agent_hosts (session_id, pid, start_time, last_seen) \
                         VALUES ('s1', 42, 7, 1)",
                        [],
                    )
                    .unwrap();
            }));
        }
    }

    mod on_an_undelivered_notice_6_days_old {
        use super::*;

        #[test]
        fn keeps_it() {
            let mut s = store();
            let row = s
                .tx(|tx| Ok::<_, StoreError>(notice(tx, &user("nobody"), 6, None)))
                .unwrap();
            sweep(&mut s);
            assert!(present(&mut s, "notices", row));
        }
    }

    mod on_an_ended_lease_91_days_old {
        use super::*;

        #[test]
        fn deletes_it() {
            let mut s = store();
            let row = s
                .tx(|tx| Ok::<_, StoreError>(lease(tx, &user("a"), "ended", 91)))
                .unwrap();
            let swept = sweep(&mut s);
            assert!(!present(&mut s, "leases", row));
            assert_eq!(swept.leases, 1);
        }

        #[test]
        fn deletes_its_audit_entries() {
            let mut s = store();
            let entry = s
                .tx(|tx| {
                    lease(tx, &user("a"), "ended", 91);
                    Ok::<_, StoreError>(audit(tx, 91))
                })
                .unwrap();
            let swept = sweep(&mut s);
            assert!(!present(&mut s, "audit", entry));
            assert_eq!(swept.audit, 1);
        }

        #[test]
        fn deletes_its_pending_yield_asks_with_it() {
            let mut s = store();
            s.tx(|tx| {
                let row = lease(tx, &user("a"), "ended", 91);
                tx.conn()
                    .execute(
                        "INSERT INTO requests (row_id, request_id, resource_row, state, holder, \
                         priority, interruptible, purpose, duration_ms, created_at) \
                         VALUES (9, 'r9', 1, 'queued', ?1, 'normal', 0, 'test', 60000, 0)",
                        [user("b")],
                    )
                    .unwrap();
                tx.conn()
                    .execute(
                        "INSERT INTO yield_asks (request_row, lease_row) VALUES (9, ?1)",
                        [row],
                    )
                    .unwrap();
                Ok::<_, StoreError>(())
            })
            .unwrap();
            sweep(&mut s);
            let left: i64 = s
                .tx(|tx| {
                    Ok::<_, StoreError>(
                        tx.conn()
                            .query_row("SELECT COUNT(*) FROM yield_asks", [], |r| r.get(0))
                            .unwrap(),
                    )
                })
                .unwrap();
            assert_eq!(left, 0);
        }
    }

    mod on_an_ended_lease_89_days_old {
        use super::*;

        #[test]
        fn keeps_it_and_its_audit_entries() {
            let mut s = store();
            let (row, entry) = s
                .tx(|tx| Ok::<_, StoreError>((lease(tx, &user("a"), "ended", 89), audit(tx, 89))))
                .unwrap();
            sweep(&mut s);
            assert!(present(&mut s, "leases", row));
            assert!(present(&mut s, "audit", entry));
        }
    }

    mod on_a_live_lease {
        use super::*;

        #[test]
        fn keeps_it_however_old() {
            let mut s = store();
            let rows = s
                .tx(|tx| {
                    Ok::<_, StoreError>(
                        ["offered", "held", "cleaning"]
                            .map(|state| lease(tx, &user(state), state, 300)),
                    )
                })
                .unwrap();
            sweep(&mut s);
            for row in rows {
                assert!(present(&mut s, "leases", row));
            }
        }

        #[test]
        fn keeps_the_ended_lease_it_borrowed_from() {
            let mut s = store();
            let lender = s
                .tx(|tx| {
                    let lender = lease(tx, &user("a"), "ended", 200);
                    let borrower = lease(tx, &user("b"), "held", 1);
                    tx.conn()
                        .execute(
                            "UPDATE leases SET paused_for_lease = ?1 WHERE row_id = ?2",
                            [lender, borrower],
                        )
                        .unwrap();
                    Ok::<_, StoreError>(lender)
                })
                .unwrap();
            sweep(&mut s);
            assert!(present(&mut s, "leases", lender));
        }
    }

    mod on_nothing_to_delete {
        use super::*;

        #[test]
        fn reports_zero_of_everything() {
            let mut s = store();
            assert_eq!(sweep(&mut s), Swept::default());
        }
    }
}
