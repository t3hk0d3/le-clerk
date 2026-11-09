-- Descriptions and usage templates: the free-form notes columns get their
-- real meaning. Existing text is kept as it is and is NOT validated: the
-- length, line and placeholder limits apply to new writes only. `admin
-- doctor` reports migrated text that breaks them; it must be corrected
-- (update_resource / update_unit) before an export can be re-imported.
ALTER TABLE resources RENAME COLUMN notes TO usage;
ALTER TABLE units RENAME COLUMN notes TO description;
