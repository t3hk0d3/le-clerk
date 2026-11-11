-- Retention. `notices.lease_row`, `unit_row`, `cleanup_deadline` and
-- `claim_deadline` were never written or read; the first two hold foreign keys,
-- which SQLite cannot drop in place, so the table is rebuilt without them
-- (keeping every row and row id).
CREATE TABLE notices_new (
    row_id        INTEGER PRIMARY KEY,
    notice_id     TEXT NOT NULL UNIQUE,
    holder        TEXT NOT NULL,
    kind          TEXT NOT NULL,
    payload       TEXT NOT NULL DEFAULT '{}',
    created_at    INTEGER NOT NULL,
    delivered_at  INTEGER,
    returned_at   INTEGER,
    hook_shown_at INTEGER
);
INSERT INTO notices_new
    (row_id, notice_id, holder, kind, payload, created_at, delivered_at, returned_at,
     hook_shown_at)
SELECT row_id, notice_id, holder, kind, payload, created_at, delivered_at, returned_at,
       hook_shown_at
FROM notices;
DROP TABLE notices;
ALTER TABLE notices_new RENAME TO notices;
CREATE INDEX notices_holder_pending ON notices (holder, delivered_at);

-- The hot queries read the live leases (`state != 'ended'`); a partial index
-- stays small however many ended leases the table keeps.
CREATE INDEX leases_live ON leases (state) WHERE state != 'ended';
