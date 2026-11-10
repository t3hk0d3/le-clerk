#!/bin/sh
# Run the release clerk as the user (not as the calling agent session) on the
# temp CLERK_HOME. Paths: see env.sh.
. "$(dirname "$0")/env.sh"
for v in $(env | sed -n 's/^\(CLAUDE[A-Z_]*\)=.*/\1/p'); do unset "$v"; done
exec "$CLERK_BIN_DIR/clerk" "$@"
