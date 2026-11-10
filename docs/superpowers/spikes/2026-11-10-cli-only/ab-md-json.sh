#!/bin/sh
# Haiku A/B: the same scenario with the clerk's agent output as Markdown
# (default) or JSON (CLERK_AGENT_FORMAT=json). Isolated: a private
# CLERK_HOME per run, the plugin by --plugin-dir, no user settings.
#   ab-md-json.sh <md|json> <run-number>
# Writes $SP_SCRATCH/ab-<fmt>-<n>/ (screens, clerk log) and prints a score.
. "$(dirname "$0")/env.sh"
fmt=$1; n=$2
R="$SP_SCRATCH/ab-$fmt-$n"; rm -rf "$R"; mkdir -p "$R"
export CLERK_HOME="$(cd "$SP_SCRATCH/.." && pwd)/${fmt}${n}"; rm -rf "$CLERK_HOME"; mkdir -p "$CLERK_HOME/config"
echo 'reconnect_grace = 20' > "$CLERK_HOME/config/config.toml"
[ "$fmt" = json ] && export CLERK_AGENT_FORMAT=json || unset CLERK_AGENT_FORMAT
export SP_RUNNER="$SP/run-e2e.sh" SP_TMUX="ab$fmt$n"
c() { env -u CLAUDE_CODE_SESSION_ID -u CLAUDE_PID "$CLERK_BIN_DIR/clerk" "$@"; }
touch "$R/board-a"
c resource create c6-boards --type device --scope user --description "ESP32-C6 dev boards on the test hub" \
  --usage 'Flash with `idf.py -p {unit.path} flash`. Clean = erase flash.' --tag esp32c6 --tag devboard >/dev/null
c unit add c6-boards c6-a --path "$R/board-a" >/dev/null
mkdir -p "$R/hub/serial/by-id"; touch "$R/hub/serial/by-id/usb-Espressif_USB_JTAG_serial_debug_unit_AA-if00" "$R/hub/serial/by-id/usb-Espressif_USB_JTAG_serial_debug_unit_BB-if00"
t() { "$SP/tm.sh" "$@"; }
t stop 2>/dev/null; t init; t start A; t start B; sleep 12
P="I need one of the shared ESP32-C6 dev boards (the clerk's c6-boards) for about 20 minutes to flash firmware. Lease one through the clerk, following the clerk skill (keep a wait armed as it says). Don't flash anything; once you hold the board or are queued with a wait armed, end your turn and act on clerk notices when they come."
t send A "$P"; sleep 45; t send B "$P"; sleep 50
t cap A "$R/A1.txt"; t cap B "$R/B1.txt"
t send A "I'm done with the board after all: hand it back through the clerk (it is clean)."; sleep 45
t cap A "$R/A2.txt"; t cap B "$R/B2.txt"
t send A "Register the two ESP32-C6 boards plugged into my hub with the clerk. Their serial ports are in $R/hub/serial/by-id/. Follow the clerk skill."; sleep 75
t cap A "$R/A3.txt"
c log --json --limit 60 > "$R/log.json"; c leases --json > "$R/leases.json"
t stop; c daemon stop >/dev/null 2>&1
python3 - "$R" "$fmt" "$n" <<'PY'
import json,sys,os
R,fmt,n=sys.argv[1:4]
log=json.load(open(f"{R}/log.json"))
acts=[e["action"] for e in log]
b_woke = "claimed" in acts and acts.count("granted")>=1
scr={k:open(f"{R}/{k}.txt").read() for k in ("A1","B1","A2","B2","A3")}
regs=[e for e in log if e["action"]=="resource_created" and e["detail"].get("id")!="c6-boards"]
units=[e for e in log if e["action"]=="unit_added" and e["detail"].get("resource")!="c6-boards"]
user_scope=all(e["detail"].get("scope")=="user" for e in regs) if regs else False
score={
 "fmt":fmt,"run":n,
 "A_granted": "granted" in acts,
 "B_queued": "queued" in acts,
 "A_released": any(e["action"]=="lease_ended" and e["detail"].get("cause")=="released" for e in log),
 "B_woke_and_claimed": "claimed" in acts,
 "B_screen_reports_hold": ("hold" in scr["B2"].lower() or "granted" in scr["B2"].lower()),
 "registered_resources": len(regs),"registered_units": len(units),"registration_user_scope": user_scope,
}
print(json.dumps(score))
open(f"{R}/score.json","w").write(json.dumps(score))
PY
