-- Schema version 4: remember which queued request an offered lease came
-- from, so a lapsed offer can name the dropped request in its notice.
ALTER TABLE leases ADD COLUMN request_id TEXT;
