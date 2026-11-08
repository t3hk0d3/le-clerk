-- Schema version 5: lease lifecycle timers. cleanup_deadline already exists
-- (0001).
--
-- warning_sent: the expiry warning for the current ends_at went out (or was
--   skipped because the lease was already past its end); reset on extend.
-- cleanup_cause: why the lease entered cleaning (EndCause text); the lease
--   ends with this cause when cleanup is confirmed or its window passes.
-- extension_used: the one cleanup extension was granted.
ALTER TABLE leases ADD COLUMN warning_sent INTEGER NOT NULL DEFAULT 0;
ALTER TABLE leases ADD COLUMN cleanup_cause TEXT;
ALTER TABLE leases ADD COLUMN extension_used INTEGER NOT NULL DEFAULT 0;
