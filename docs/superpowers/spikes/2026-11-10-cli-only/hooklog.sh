#!/bin/sh
# Hook stand-in: appends the hook's stdin and Claude env to $SP_SCRATCH/hooks.log.
. "$(dirname "$0")/env.sh"
{ printf '%s pid=%s sid=%s input=' "$(date +%T)" "$CLAUDE_PID" "$CLAUDE_CODE_SESSION_ID"; cat; echo; } >> "$SP_SCRATCH/hooks.log"
exit 0
