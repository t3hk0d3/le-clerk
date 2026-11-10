#!/bin/sh
# Run claude isolated with the leclerk plugin from this checkout and a
# private clerk: CLERK_HOME and CLERK_BIN_DIR come from the environment.
. "$(dirname "$0")/env.sh"
for v in $(env | sed -n 's/^\(CLAUDE[A-Z_]*\|MCP_[A-Z_]*\|AGENT_[A-Z_]*\)=.*/\1/p'); do unset "$v"; done
REPO=$(cd "$SP/../../../.." && pwd)
PATH="$CLERK_BIN_DIR:$PATH" exec claude --setting-sources project --strict-mcp-config \
  --plugin-dir "$REPO/integrations/claude-code" --allowedTools=Bash,Monitor "$@"
