#!/bin/sh
# tmux helpers for the manual checklist (docs/claude-code.md), on a private tmux server (-L $T7_TMUX).
# Paths: see env.sh.
#   tm.sh init                                          make the subject project and the tmux session
#   tm.sh start <name> <session-uuid|-> [claude args...]
#                                                       start claude in window <name>
#                                                       (- : no --session-id, e.g. with --continue)
#   tm.sh send <name> <text>                            type a prompt and press Enter
#   tm.sh cap <name> <file>                             save the window's screen and scrollback
#   tm.sh pid <name>                                    the claude process id of the window
#   tm.sh stop                                          kill the private tmux server
. "$(dirname "$0")/env.sh"
tm() { tmux -L "$T7_TMUX" "$@"; }
cmd=$1; shift
case $cmd in
init)
  mkdir -p "$T7_SUBJECT"
  [ -d "$T7_SUBJECT/.git" ] || git -C "$T7_SUBJECT" init -q
  tm has-session -t "$T7_TMUX" 2>/dev/null || tm new-session -d -s "$T7_TMUX" -n shell -c "$T7_SUBJECT" ;;
start) name=$1; id=$2; shift 2
  sid=; [ "$id" = - ] || sid="--session-id $id"
  tm new-window -d -t "$T7_TMUX:" -n "$name" -c "$T7_SUBJECT" \
    "$T7/run-claude.sh $sid --model haiku --allowedTools=Bash,Monitor --debug-file $T7_SCRATCH/t7-$name-debug.log $*"
  tm set-option -w -t "$T7_TMUX:$name" remain-on-exit on ;;
send) name=$1; shift
  tm send-keys -t "$T7_TMUX:$name" -l "$*"; sleep 0.5; tm send-keys -t "$T7_TMUX:$name" Enter ;;
cap) tm capture-pane -p -J -S -300 -t "$T7_TMUX:$1" > "$2" ;;
pid) tm list-panes -t "$T7_TMUX:$1" -F '#{pane_pid}' ;;
stop) tm kill-server ;;
*) echo "usage: tm.sh init|start|send|cap|pid|stop" >&2; exit 2 ;;
esac
