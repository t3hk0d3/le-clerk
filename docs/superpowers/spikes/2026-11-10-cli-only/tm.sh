#!/bin/sh
# tm.sh init | start <name> [claude args] | send <name> <text> | key <name> <key> | cap <name> <file> | stop
. "$(dirname "$0")/env.sh"
tm() { tmux -L "$SP_TMUX" "$@"; }
cmd=$1; shift
case $cmd in
init) mkdir -p "$SP_SUBJECT"; [ -d "$SP_SUBJECT/.git" ] || git -C "$SP_SUBJECT" init -q
  tm has-session -t "$SP_TMUX" 2>/dev/null || tm new-session -d -s "$SP_TMUX" -n shell -c "$SP_SUBJECT" ;;
start) name=$1; shift
  tm new-window -d -t "$SP_TMUX:" -n "$name" -c "$SP_SUBJECT" "$SP/run-claude.sh --model haiku --debug-file $SP_SCRATCH/$name-debug.log $*"
  tm set-option -w -t "$SP_TMUX:$name" remain-on-exit on ;;
send) name=$1; shift; tm send-keys -t "$SP_TMUX:$name" -l "$*"; sleep 0.5; tm send-keys -t "$SP_TMUX:$name" Enter ;;
key) tm send-keys -t "$SP_TMUX:$1" "$2" ;;
cap) tm capture-pane -p -J -S -300 -t "$SP_TMUX:$1" > "$2" ;;
stop) tm kill-server ;;
esac
