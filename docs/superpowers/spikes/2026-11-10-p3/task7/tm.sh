#!/bin/sh
# tmux helpers for the Task 7 checks (private tmux server -L t7).
#   tm.sh start <name> <session-uuid> [claude args...]  start claude in window <name>
#   tm.sh send <name> <text>                            type a prompt and press Enter
#   tm.sh cap <name> <file>                             save the window's screen and scrollback
#   tm.sh pid <name>                                    the claude process id of the window
T7=$(dirname "$0")
SCRATCH=/tmp/claude-1000/-home-tehkode-dev/3ee8a19f-2334-4c55-8c65-c9833449d6cc/scratchpad
SUBJECT=$SCRATCH/subject
cmd=$1; shift
case $cmd in
start) name=$1; id=$2; shift 2
  tmux -L t7 new-window -d -n "$name" -c "$SUBJECT" \
    "$T7/run-claude.sh --model haiku --allowedTools=mcp__plugin_leclerk_clerk --debug-file $SCRATCH/t7-$name-debug.log $*" 
  tmux -L t7 set-option -w -t "daemon:$name" remain-on-exit on ;;
send) name=$1; shift
  tmux -L t7 send-keys -t "daemon:$name" -l "$*"; sleep 0.5; tmux -L t7 send-keys -t "daemon:$name" Enter ;;
cap) tmux -L t7 capture-pane -p -J -S -300 -t "daemon:$1" > "$2" ;;
pid) tmux -L t7 list-panes -t "daemon:$1" -F '#{pane_pid}' ;;
esac
