-- What the stored values can say: enum texts and JSON shapes. It is separate
-- from the schema version because a new value needs no migration; a binary
-- that does not know the vocabulary refuses the database (Store::open).
CREATE TABLE store_vocabulary (
    id                 INTEGER PRIMARY KEY CHECK (id = 1),
    vocabulary_version INTEGER NOT NULL
);
INSERT INTO store_vocabulary (id, vocabulary_version) VALUES (1, 1);

-- The boot a host process ran in; NULL for hosts recorded before this
-- column, which are not taken for dead on that account.
ALTER TABLE agent_hosts ADD COLUMN boot_id TEXT;
