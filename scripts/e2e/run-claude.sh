#!/bin/sh
# Task 7 live checks: run claude in isolation from the user's own clerk and
# Claude Code setup. Every inherited CLAUDE*/MCP_*/AGENT_* variable is removed
# (the checks may run from inside another Claude Code session), the release
# build of clerk goes first on PATH, the clerk state lives in a temp
# CLERK_HOME, the plugin is loaded for this run only (--plugin-dir) and user
# settings are not read (--setting-sources project). Nothing is installed.
# Paths: see env.sh.
. "$(dirname "$0")/env.sh"
for v in $(env | sed -n 's/^\(CLAUDE[A-Z_]*\|MCP_[A-Z_]*\|AGENT_[A-Z_]*\)=.*/\1/p'); do unset "$v"; done
export PATH="$CLERK_BIN_DIR:$PATH"
exec claude --plugin-dir "$LECLERK_REPO/integrations/claude-code" \
  --setting-sources project "$@"
