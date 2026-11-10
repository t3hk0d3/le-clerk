# CLI-only spike: tests Claude Code's own mechanisms, no clerk, no plugin.
#   SP_SCRATCH  logs, flag files and the subject project (default: ${TMPDIR:-/tmp}/clerk-sp)
#   SP_SUBJECT  the project sessions run in; use a folder Claude Code already trusts (default: $SP_SCRATCH/subject)
#   SP_TMUX     private tmux server name (default: sp)
SP=$(cd "$(dirname "$0")" && pwd)
SP_SCRATCH="${SP_SCRATCH:-${TMPDIR:-/tmp}/clerk-sp}"
SP_SUBJECT="${SP_SUBJECT:-$SP_SCRATCH/subject}"
SP_TMUX="${SP_TMUX:-sp}"
export SP SP_SCRATCH SP_SUBJECT SP_TMUX
