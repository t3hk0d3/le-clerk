-- Schema version 3: scheduler bookkeeping on leases and requests.
--
-- project_key: the requester's project ('' when none), used to decide what a
--   holder may be told about other parties.
-- override_reason: the stated reason of an override request (behaviour comes
--   with interruption; stored from the start so the audit trail is complete).
-- dirty_info: on a lease, the unit's dirty record handed over at grant or
--   offer time (JSON DirtyInfo), so an offer that lapses can restore it.
ALTER TABLE leases ADD COLUMN project_key TEXT NOT NULL DEFAULT '';
ALTER TABLE leases ADD COLUMN override_reason TEXT;
ALTER TABLE leases ADD COLUMN dirty_info TEXT;
ALTER TABLE requests ADD COLUMN project_key TEXT NOT NULL DEFAULT '';
ALTER TABLE requests ADD COLUMN override_reason TEXT;
CREATE INDEX leases_state ON leases (state);
CREATE INDEX requests_state ON requests (state);
