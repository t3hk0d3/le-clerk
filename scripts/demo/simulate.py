#!/usr/bin/env python3
"""Simulate two Claude Code agents sharing one port, for the README demo.

Real sessions wander; this plays a scripted, Claude Code-styled transcript
in two panes. The clerk output in it is what agents really get (Markdown,
abridged). Writes frames.json for render.py.

    simulate.py <out frames.json>
"""
import json
import sys

W, H = 150, 30
PANE = (W - 1) // 2

DIM, BOLD, RESET = "\x1b[90m", "\x1b[1m", "\x1b[0m"
CYAN, GREEN, YELLOW, MAGENTA = "\x1b[36m", "\x1b[32m", "\x1b[33m", "\x1b[35m"


def wrap(text, indent=0, first=""):
    """Wrap `text` to the pane, `first` before the first line."""
    out, line, width = [], first, PANE - 1
    for word in text.split(" "):
        visible = len(strip(line))
        if visible + len(word) + 1 > width and strip(line).strip():
            out.append(line)
            line = " " * indent
        line += ("" if not strip(line).strip() else " ") + word
    out.append(line)
    return out


def strip(s):
    import re
    return re.sub(r"\x1b\[[0-9;]*m", "", s)


class Pane:
    def __init__(self):
        self.lines = []
        self.input = ""

    def screen(self):
        body = self.lines[-(H - 4):]
        body += [""] * (H - 4 - len(body))
        rule = DIM + "─" * (PANE - 1) + RESET
        return "\n".join(body + [rule, f"{BOLD}❯{RESET} {self.input}", rule])


A, B = Pane(), Pane()
frames, t = [], 0.0


def snap(dt):
    global t
    t += dt
    frames.append({"t": round(t, 2), "panes": [A.screen(), B.screen()]})


def type_prompt(p, text):
    for n in range(0, len(text) + 1, 3):
        p.input = text[:n]
        snap(0.05)
    snap(0.4)
    p.input = ""
    p.lines += [""] + wrap(text, 2, f"{BOLD}❯{RESET} ") + [""]
    snap(0.6)


def say(p, text, pause=0.9):
    p.lines += wrap(text, 2, f"{BOLD}●{RESET}") + [""]
    snap(pause)


def tool(p, name, arg, output, pause=1.0, color=GREEN):
    p.lines.append(f"{color}●{RESET} {BOLD}{name}{RESET}({arg})")
    snap(0.5)
    for k, line in enumerate(output):
        p.lines.append(f"  {DIM}{'⎿' if k == 0 else ' '}{RESET}  {line}")
        snap(0.12)
    p.lines.append("")
    snap(pause)


PROMPT = "Start the API dev server so I can try it. It needs the shared API port."
ACQ = 'clerk acquire api-port --for 30m --purpose "API dev server"'

snap(0.8)
type_prompt(A, PROMPT)
say(A, "The port is shared, so I'll lease it through the clerk first.", 0.7)
tool(A, "Bash", ACQ, [
    f"{BOLD}## Granted: api-port/p18080{RESET}",
    "- ends_at: 2026-11-10 16:45 (in 30m)",
    "- lease_id: lse_01M730E06MJPRTMNP28VZS0CEG",
    "- address: 127.0.0.1:18080 (listen)",
    f"{BOLD}### usage{RESET}",
    "Start the API with `serve-api 18080`.",
    f"{CYAN}Next:{RESET} use the unit, then `clerk release …`.",
])
tool(A, "Bash", "serve-api 18080 &", ["API listening on http://127.0.0.1:18080"], 0.6)
say(A, "The API is up on http://127.0.0.1:18080. I hold the port until 16:45.", 1.2)

type_prompt(B, PROMPT)
say(B, "The port is shared, so I'll lease it through the clerk first.", 0.7)
tool(B, "Bash", ACQ, [
    f"{BOLD}## Queued: request req_01M730F1Q8V2{RESET}",
    "- position: 1",
    "- blocked_by: p18080 held by another agent",
    '  for "API dev server", ends 16:45 (in 30m)',
    f"{CYAN}Next:{RESET} run `clerk wait` in the background.",
], 0.9)
tool(B, "Bash", "clerk wait", ["Running in the background"], 0.6, MAGENTA)
say(B, "Another agent is running the API on that port. I'm first in the "
       "queue and will start the server as soon as the port is mine.", 2.5)

type_prompt(A, "Thanks, I'm done with it. Stop the server.")
tool(A, "Bash", "kill %1", ["API stopped"], 0.5)
tool(A, "Bash", "clerk release lse_01M730E06MJPRTMNP28VZS0CEG", [
    f"{BOLD}## Lease lse_01M730E06MJPRTMNP28VZS0CEG{RESET}",
    "- state: ended (cleanup confirmed)",
], 0.6)
say(A, "Server stopped and the port released.", 0.9)

B.lines += [f"{MAGENTA}●{RESET} Background command \"clerk wait\" completed",
            f"  {DIM}⎿{RESET}  {BOLD}## Notice: offered{RESET} · api-port/p18080",
            f"     {BOLD}## Lease lse_01M730H4ZK3B{RESET} · held",
            f"     {CYAN}Next:{RESET} start the API with `serve-api 18080`.", ""]
snap(1.2)
tool(B, "Bash", "serve-api 18080 &", ["API listening on http://127.0.0.1:18080"], 0.6)
say(B, "The port came free and the clerk handed it to me: the API is up on "
       "http://127.0.0.1:18080.", 4.0)

json.dump({"width": W, "height": H, "frames": frames}, open(sys.argv[1], "w"))
print(len(frames), "frames,", round(t, 1), "s")
