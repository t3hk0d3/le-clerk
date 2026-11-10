#!/bin/sh
# Stand-in for `clerk wait --follow`: prints one notice line per flag file
# $1.N that appears (N = 1, 2, ...), forever.
. "$(dirname "$0")/env.sh"
n=1
echo "start $(date +%T) pid=$$ sid=$CLAUDE_CODE_SESSION_ID claude_pid=$CLAUDE_PID" >> "$SP_SCRATCH/streamer.log"
while :; do
  if [ -e "$1.$n" ]; then
    echo "NOTICE $n: lease lse_TEST on board/b1 needs you. Reply with exactly: GOT $n"
    echo "emit $n $(date +%T)" >> "$SP_SCRATCH/streamer.log"
    n=$((n+1))
  fi
  sleep 1
done
