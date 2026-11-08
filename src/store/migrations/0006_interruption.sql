-- Schema version 6: interruption, borrowing, reclaim and override.
--
-- On a lease:
-- yield_request: the queued request (request_id) the holder is being asked to
--   yield for; NULL when no ask is pending. yield_asked_at / yield_deadline:
--   when that ask went out and when the holder's yield window ends.
-- paused_remaining_ms: while paused, the time that was left on the lease.
-- borrower_lease: on a paused lease, the borrower's lease (row_id).
-- paused_for_lease: on a borrower's lease, the paused lease (row_id) whose
--   unit it borrows.
-- borrow_due_at: on a borrower's lease, claim time + its declared duration
--   (extend does not move it); past it the borrower has overrun.
-- overran_sent: BorrowerOverran went out for this borrow.
-- cleanup_confirmed: set when the lease ends: whether cleanup was confirmed.
--
-- On a request:
-- override_lease: the lease (row_id) this override request displaced; such
--   requests are first in queue order.
--
-- yield_asks: which holders were asked for which request, so a holder that
--   refused or let the window pass is never asked again for it.
ALTER TABLE leases ADD COLUMN yield_request TEXT;
ALTER TABLE leases ADD COLUMN yield_asked_at INTEGER;
ALTER TABLE leases ADD COLUMN yield_deadline INTEGER;
ALTER TABLE leases ADD COLUMN paused_remaining_ms INTEGER;
ALTER TABLE leases ADD COLUMN borrower_lease INTEGER REFERENCES leases (row_id) ON DELETE SET NULL;
ALTER TABLE leases ADD COLUMN paused_for_lease INTEGER REFERENCES leases (row_id) ON DELETE SET NULL;
ALTER TABLE leases ADD COLUMN borrow_due_at INTEGER;
ALTER TABLE leases ADD COLUMN overran_sent INTEGER NOT NULL DEFAULT 0;
ALTER TABLE leases ADD COLUMN cleanup_confirmed INTEGER;
ALTER TABLE requests ADD COLUMN override_lease INTEGER REFERENCES leases (row_id) ON DELETE SET NULL;
CREATE TABLE yield_asks (
    request_row INTEGER NOT NULL REFERENCES requests (row_id) ON DELETE CASCADE,
    lease_row   INTEGER NOT NULL REFERENCES leases (row_id) ON DELETE CASCADE,
    PRIMARY KEY (request_row, lease_row)
);
