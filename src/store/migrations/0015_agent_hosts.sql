-- Agent sessions' hosts (Part 2, no MCP): the Claude Code processes running
-- each session, by pid and start time (usually one; two when the session is
-- resumed in a second terminal). Replaces the resume aliases: the session
-- id itself survives --continue and --resume.
DROP TABLE session_aliases;
CREATE TABLE agent_hosts (
    session_id TEXT NOT NULL,
    pid        INTEGER NOT NULL,
    start_time INTEGER NOT NULL,
    last_seen  INTEGER NOT NULL,
    PRIMARY KEY (session_id, pid, start_time)
);
