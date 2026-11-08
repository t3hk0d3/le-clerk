-- Schema version 2: record who made each unit link. 'auto' links come from
-- identity matching and are recomputed when a unit's settings change; 'manual'
-- links come from an administrator and are kept.
ALTER TABLE unit_links ADD COLUMN origin TEXT NOT NULL DEFAULT 'auto'
    CHECK (origin IN ('auto', 'manual'));
