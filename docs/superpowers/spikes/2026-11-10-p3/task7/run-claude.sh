#!/bin/sh
# Task 7 live checks: run claude in isolation from the user's own clerk and
# Claude Code setup. Every inherited CLAUDE*/MCP_*/AGENT_* variable is removed
# (the checks ran from inside another Claude Code session), the release build
# of clerk goes first on PATH, the clerk state lives in a temp CLERK_HOME, the
# plugin is loaded for this run only (--plugin-dir) and user settings are not
# read (--setting-sources project). Nothing is installed.
for v in $(env | sed -n 's/^\(CLAUDE[A-Z_]*\|MCP_[A-Z_]*\|AGENT_[A-Z_]*\)=.*/\1/p'); do unset "$v"; done
export PATH="${T7_CLERK_BIN:-/tmp/claude-1000/leclerk-target/release}:$PATH"
export CLERK_HOME="${CLERK_HOME:-/tmp/claude-1000/t7h}"
exec claude --plugin-dir /home/tehkode/dev/leclerk/integrations/claude-code \
  --setting-sources project "$@"
