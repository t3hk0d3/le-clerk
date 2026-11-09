-- Descriptions and usage templates: the free-form notes columns get their
-- real meaning. Existing text is kept as it is; the length and placeholder
-- limits apply to new writes only.
ALTER TABLE resources RENAME COLUMN notes TO usage;
ALTER TABLE units RENAME COLUMN notes TO description;
