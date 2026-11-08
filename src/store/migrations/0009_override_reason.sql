-- Rename the audit column to match the CLI flag (--override "<reason>").
ALTER TABLE audit RENAME COLUMN user_requested TO override_reason;
