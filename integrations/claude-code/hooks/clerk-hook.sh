#!/bin/sh
# Runs `clerk hook <event>`; exits 0 silently when the clerk binary
# ($CLERK_BIN, else `clerk` on PATH) is missing, so a hook never fails
# for want of it.
bin=${CLERK_BIN:-clerk}
command -v "$bin" >/dev/null 2>&1 || exit 0
exec "$bin" hook "$1"
