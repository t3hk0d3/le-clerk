#!/bin/sh
# Run the release clerk as the user (not as the calling agent session) on the
# Task 7 temp CLERK_HOME.
for v in $(env | sed -n 's/^\(CLAUDE[A-Z_]*\)=.*/\1/p'); do unset "$v"; done
export CLERK_HOME="${CLERK_HOME:-/tmp/claude-1000/t7h}"
exec /tmp/claude-1000/leclerk-target/release/clerk "$@"
