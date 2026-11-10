-- A request whose holder vouched for a specific unit (`--skip-checks`): it is
-- granted without running the unit's health checks.
ALTER TABLE requests ADD COLUMN skip_checks INTEGER NOT NULL DEFAULT 0;
