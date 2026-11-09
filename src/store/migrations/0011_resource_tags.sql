-- Search tags on resources (spec 4.8). Existing resources get none.
CREATE TABLE resource_tags (
    resource_row INTEGER NOT NULL REFERENCES resources (row_id) ON DELETE CASCADE,
    tag TEXT NOT NULL,
    PRIMARY KEY (resource_row, tag)
);

CREATE INDEX resource_tags_by_tag ON resource_tags (tag);
