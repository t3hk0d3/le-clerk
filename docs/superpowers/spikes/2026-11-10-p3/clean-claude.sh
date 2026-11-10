#!/bin/sh
# Run claude with every inherited CLAUDE*/MCP env var removed, so the spike sees
# a plain terminal-CLI session rather than the parent agent's environment.
for v in $(env | sed -n 's/^\(CLAUDE[A-Z_]*\|MCP_[A-Z_]*\|AGENT_[A-Z_]*\)=.*/\1/p'); do unset "$v"; done
# SPIKE_IDLE_MS (if set) becomes CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT after the scrub.
[ -n "$SPIKE_IDLE_MS" ] && export CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT="$SPIKE_IDLE_MS"
exec claude "$@"
