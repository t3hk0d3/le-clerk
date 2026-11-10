#!/bin/sh
# Run claude isolated: inherited CLAUDE*/MCP_*/AGENT_* removed, user settings
# not read, no plugins, Bash allowed.
. "$(dirname "$0")/env.sh"
for v in $(env | sed -n 's/^\(CLAUDE[A-Z_]*\|MCP_[A-Z_]*\|AGENT_[A-Z_]*\)=.*/\1/p'); do unset "$v"; done
exec claude --setting-sources project --strict-mcp-config --allowedTools=Bash "$@"
