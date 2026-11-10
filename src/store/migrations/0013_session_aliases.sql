-- Agent-session resume aliases (phase 3). A SessionStart resume hands the
-- resumed session's leases, requests and notices to the live proxy session:
-- `resumed` stands for `holder` from then on. A holder resumed again later
-- gets its own row, so ids resolve by following the chain to its end.
CREATE TABLE session_aliases (
    resumed    TEXT PRIMARY KEY,
    holder     TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
