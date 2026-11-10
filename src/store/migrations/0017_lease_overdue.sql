-- An agent's lease past its end with nobody waiting stays held, marked
-- overdue, until someone wants the unit or the holder extends or releases it.
ALTER TABLE leases ADD COLUMN overdue INTEGER NOT NULL DEFAULT 0;
