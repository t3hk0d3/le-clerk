#!/bin/sh
# Spike hook: append the hook input plus a few env facts to logs/hooks.jsonl.
in=$(cat)
printf '{"event":"%s","env_CLAUDE_CODE_SESSION_ID":"%s","env_CLAUDE_PID":"%s","ppid":"%s","input":%s}\n' "$1" "$CLAUDE_CODE_SESSION_ID" "$CLAUDE_PID" "$PPID" "$in" >> "$(dirname "$0")/logs/hooks.jsonl"
exit 0
