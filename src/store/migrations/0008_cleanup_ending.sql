-- Schema version 8: the cleanup-ending notice.
--
-- cleanup_ending_sent: the CleanupEnding notice for the current cleanup
--   deadline went out (or was skipped because the window had already
--   passed); reset when the deadline moves (cleanup extension, restart
--   grace).
ALTER TABLE leases ADD COLUMN cleanup_ending_sent INTEGER NOT NULL DEFAULT 0;
