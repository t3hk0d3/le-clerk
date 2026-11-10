#!/bin/sh
# Stand-in for a blocking `clerk wait`: logs its start and environment,
# blocks until flag file $1 exists, then prints a notice and exits 0.
. "$(dirname "$0")/env.sh"
flag=$1
{ echo "start $(date +%T) pid=$$ ppid=$PPID sid=$CLAUDE_CODE_SESSION_ID claude_pid=$CLAUDE_PID flag=$flag"
  p=$$; while [ "$p" -gt 1 ]; do printf '  %s %s\n' "$p" "$(tr '\0' ' ' < /proc/$p/cmdline | cut -c1-80)"; p=$(awk '{print $4}' /proc/$p/stat); done
} >> "$SP_SCRATCH/waiter.log"
trap 'echo "signal $(date +%T) pid=$$" >> "$SP_SCRATCH/waiter.log"; exit 1' TERM INT HUP
while [ ! -e "$flag" ]; do sleep 1; done
echo "exit $(date +%T) pid=$$ flag=$flag" >> "$SP_SCRATCH/waiter.log"
echo "NOTICE: lease lse_TEST on board/b1 was granted to you. Reply with exactly: WOKE $(basename "$flag")"
