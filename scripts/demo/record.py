#!/usr/bin/env python3
"""Record the README demo: two "agents" want the same port for an API dev
server. Plays the scene in a private tmux server against a scratch
CLERK_HOME, captures both panes every 100 ms and writes frames.json.

    record.py <clerk binary> <scratch dir>
"""
import json, os, shutil, subprocess, sys, time

CLERK, SCRATCH = sys.argv[1], sys.argv[2]
HERE = os.path.dirname(os.path.abspath(__file__))
HOME = os.path.join(SCRATCH, "h")
T = ["tmux", "-L", "clerkdemo"]
W, H = 150, 20
env = {k: v for k, v in os.environ.items() if not k.startswith("CLAUDE")}
env.update(CLERK_HOME=HOME, PATH=f"{os.path.dirname(CLERK)}:{HERE}:{env['PATH']}")

def sh(*a, **k):
    return subprocess.run(a, env=env, check=True, capture_output=True, text=True, **k).stdout

subprocess.run(T + ["kill-server"], capture_output=True)
shutil.rmtree(HOME, ignore_errors=True)
os.makedirs(HOME)
sh(CLERK, "resource", "create", "api-port", "--type", "service", "--scope", "user",
   "--description", "Port for the API dev server", "--tag", "http",
   "--usage", "Run the API on {unit.address}.")
sh(CLERK, "unit", "add", "api-port", "p18080", "--host", "127.0.0.1", "--port", "18080", "--listen")

def shell(name, secs):
    return (f"env SERVE_SECS={secs} PS1='\\[\\e[1;36m\\]{name}\\[\\e[0m\\] $ ' "
            "bash --norc --noprofile")
subprocess.run(T + ["new-session", "-d", "-s", "d", "-x", str(W), "-y", str(H), shell("agent-A", 5)], env=env, check=True)
subprocess.run(T + ["split-window", "-h", "-t", "d", shell("agent-B", 60)], env=env, check=True)
subprocess.run(T + ["set", "-g", "status", "off"], env=env, check=True)
time.sleep(0.8)

frames, t0 = [], time.time()
def snap():
    panes = []
    for p in ("d.0", "d.1"):
        panes.append(subprocess.run(T + ["capture-pane", "-e", "-p", "-t", p], capture_output=True, text=True).stdout)
    frames.append({"t": round(time.time() - t0, 2), "panes": panes})
def idle(secs):
    end = time.time() + secs
    while time.time() < end:
        snap(); time.sleep(0.1)
def type_in(pane, text):
    for n in range(0, len(text), 3):
        subprocess.run(T + ["send-keys", "-t", pane, "-l", "--", text[n:n + 3]])
        snap(); time.sleep(0.06)
    idle(0.3)
    subprocess.run(T + ["send-keys", "-t", pane, "Enter"])

CMD = 'clerk run api-port --for 30m --purpose "API dev server" -- serve-api'
idle(0.8)
type_in("d.0", CMD); idle(1.5)
type_in("d.1", CMD); idle(8)
subprocess.run(T + ["kill-server"], capture_output=True)
sh(CLERK, "daemon", "stop")
json.dump({"width": W, "height": H, "frames": frames}, open(os.path.join(SCRATCH, "frames.json"), "w"))
print(len(frames), "frames")
