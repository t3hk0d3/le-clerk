-- Agent sessions' hosts (Part 2, no MCP). The Claude Code process last
-- reported running each session, by pid and start time; a row without a
-- process (pid NULL) is a session its process left. Replaces the resume
-- aliases: the session id itself survives --continue and --resume.
DROP TABLE session_aliases;
CREATE TABLE agent_hosts (
    session_id TEXT PRIMARY KEY,
    pid        INTEGER,
    start_time INTEGER,
    last_seen  INTEGER NOT NULL
);
