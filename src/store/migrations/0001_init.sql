-- Le Clerk schema, version 1. Migrations are append-only: later changes go in
-- new files, never in this one.
--
-- Timestamps are unix milliseconds. JSON columns hold serde_json text. Leases,
-- requests and notices reference resources and units by internal integer row
-- ids so that renaming a resource or unit never breaks them.

CREATE TABLE resources (
    row_id      INTEGER PRIMARY KEY,
    id          TEXT NOT NULL,
    scope_kind  TEXT NOT NULL CHECK (scope_kind IN ('user', 'project')),
    -- Empty for user scope, so the unique index below also covers it.
    project_key TEXT NOT NULL DEFAULT '',
    type        TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    notes       TEXT NOT NULL DEFAULT '',
    health      TEXT,
    created_at  INTEGER NOT NULL
);
CREATE UNIQUE INDEX resources_scope_id ON resources (scope_kind, project_key, id);

CREATE TABLE units (
    row_id            INTEGER PRIMARY KEY,
    resource_row      INTEGER NOT NULL REFERENCES resources (row_id) ON DELETE CASCADE,
    id                TEXT NOT NULL,
    settings          TEXT NOT NULL,
    notes             TEXT NOT NULL DEFAULT '',
    -- Device/service/generic identity used for dedup; filled in later.
    identity          TEXT,
    dirty_reason      TEXT,
    dirty_at          INTEGER,
    dirty_prev_holder TEXT,
    retiring          INTEGER NOT NULL DEFAULT 0,
    last_health       TEXT,
    last_health_at    INTEGER,
    created_at        INTEGER NOT NULL,
    UNIQUE (resource_row, id)
);

CREATE TABLE unit_links (
    unit_a INTEGER NOT NULL REFERENCES units (row_id) ON DELETE CASCADE,
    unit_b INTEGER NOT NULL REFERENCES units (row_id) ON DELETE CASCADE,
    PRIMARY KEY (unit_a, unit_b)
);
CREATE INDEX unit_links_b ON unit_links (unit_b);

CREATE TABLE promotions (
    row_id       INTEGER PRIMARY KEY,
    promotion_id TEXT NOT NULL UNIQUE,
    resource_row INTEGER NOT NULL REFERENCES resources (row_id) ON DELETE CASCADE,
    requester    TEXT NOT NULL,
    reason       TEXT NOT NULL DEFAULT '',
    state        TEXT NOT NULL DEFAULT 'pending',
    created_at   INTEGER NOT NULL
);

CREATE TABLE leases (
    row_id            INTEGER PRIMARY KEY,
    lease_id          TEXT NOT NULL UNIQUE,
    unit_row          INTEGER NOT NULL REFERENCES units (row_id) ON DELETE CASCADE,
    resource_row      INTEGER NOT NULL REFERENCES resources (row_id) ON DELETE CASCADE,
    state             TEXT NOT NULL,
    holder            TEXT NOT NULL,
    priority          TEXT NOT NULL,
    interruptible     INTEGER NOT NULL,
    purpose           TEXT NOT NULL DEFAULT '',
    duration_ms       INTEGER NOT NULL,
    created_at        INTEGER NOT NULL,
    ends_at           INTEGER,
    claim_deadline    INTEGER,
    cleanup_deadline  INTEGER,
    end_cause         TEXT
);
CREATE INDEX leases_unit_state ON leases (unit_row, state);

CREATE TABLE requests (
    row_id           INTEGER PRIMARY KEY,
    request_id       TEXT NOT NULL UNIQUE,
    resource_row     INTEGER NOT NULL REFERENCES resources (row_id) ON DELETE CASCADE,
    -- Set when the request targets one specific unit.
    unit_row         INTEGER REFERENCES units (row_id) ON DELETE CASCADE,
    state            TEXT NOT NULL,
    holder           TEXT NOT NULL,
    priority         TEXT NOT NULL,
    interruptible    INTEGER NOT NULL,
    purpose          TEXT NOT NULL DEFAULT '',
    duration_ms      INTEGER NOT NULL,
    created_at       INTEGER NOT NULL,
    ends_at          INTEGER,
    claim_deadline   INTEGER,
    cleanup_deadline INTEGER,
    end_cause        TEXT
);
CREATE INDEX requests_resource_state ON requests (resource_row, state);

CREATE TABLE notices (
    row_id           INTEGER PRIMARY KEY,
    notice_id        TEXT NOT NULL UNIQUE,
    holder           TEXT NOT NULL,
    kind             TEXT NOT NULL,
    lease_row        INTEGER REFERENCES leases (row_id) ON DELETE CASCADE,
    unit_row         INTEGER REFERENCES units (row_id) ON DELETE CASCADE,
    payload          TEXT NOT NULL DEFAULT '{}',
    cleanup_deadline INTEGER,
    claim_deadline   INTEGER,
    created_at       INTEGER NOT NULL,
    delivered_at     INTEGER
);
CREATE INDEX notices_holder_pending ON notices (holder, delivered_at);

CREATE TABLE audit (
    row_id         INTEGER PRIMARY KEY,
    at             INTEGER NOT NULL,
    actor          TEXT NOT NULL,
    user_requested TEXT,
    action         TEXT NOT NULL,
    detail         TEXT NOT NULL DEFAULT '{}'
);
