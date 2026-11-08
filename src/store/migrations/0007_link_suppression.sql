-- Schema version 7: administrator unlinks.
--
-- link_suppressions: unit pairs (lower row id first) an administrator declared
-- to be different things. Identity matching never auto-links a suppressed
-- pair; an administrator's link clears the suppression again.
CREATE TABLE link_suppressions (
    unit_a INTEGER NOT NULL REFERENCES units (row_id) ON DELETE CASCADE,
    unit_b INTEGER NOT NULL REFERENCES units (row_id) ON DELETE CASCADE,
    PRIMARY KEY (unit_a, unit_b)
);
CREATE INDEX link_suppressions_b ON link_suppressions (unit_b);
