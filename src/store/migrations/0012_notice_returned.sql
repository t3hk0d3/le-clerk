-- Acknowledged delivery for MCP (phase 3): `lease_wait` returns a notice
-- without delivering it, and only an acknowledgement marks it delivered.
-- `returned_at` records when a notice was first returned unacknowledged.
ALTER TABLE notices ADD COLUMN returned_at INTEGER;
