# Sourced by the end-to-end helpers: where things are. Every path can be set from
# the environment; the defaults need no particular user or machine.
#   LECLERK_REPO   the repository checkout (default: found from this file)
#   CLERK_BIN_DIR  the directory holding the release `clerk`
#                  (default: $CARGO_TARGET_DIR/release, else $LECLERK_REPO/target/release)
#   CLERK_HOME     the temp clerk state; keep it short, the socket path must
#                  stay under 107 bytes (default: /tmp/clerk-t7)
#   T7_SCRATCH     where debug logs go and the subject project lives
#                  (default: ${TMPDIR:-/tmp}/clerk-t7-scratch)
#   T7_SUBJECT     the project directory sessions run in (default: $T7_SCRATCH/subject)
#   T7_TMUX        the private tmux server and session name (default: t7)
T7=$(cd "$(dirname "$0")" && pwd)
LECLERK_REPO="${LECLERK_REPO:-$(cd "$T7/../.." && pwd)}"
CLERK_BIN_DIR="${CLERK_BIN_DIR:-${CARGO_TARGET_DIR:-$LECLERK_REPO/target}/release}"
CLERK_HOME="${CLERK_HOME:-/tmp/clerk-t7}"
T7_SCRATCH="${T7_SCRATCH:-${TMPDIR:-/tmp}/clerk-t7-scratch}"
T7_SUBJECT="${T7_SUBJECT:-$T7_SCRATCH/subject}"
T7_TMUX="${T7_TMUX:-t7}"
export LECLERK_REPO CLERK_BIN_DIR CLERK_HOME T7_SCRATCH T7_SUBJECT T7_TMUX
