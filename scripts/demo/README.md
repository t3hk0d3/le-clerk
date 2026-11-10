# The README demo

`docs/demo.gif` shows two Claude Code agents sharing the API dev server's
port. It is a simulation: real sessions wander, so `simulate.py` plays a
scripted transcript in Claude Code's style, with the clerk output agents
really get (Markdown, abridged).

```sh
python3 scripts/demo/simulate.py /tmp/frames.json
python3 scripts/demo/render.py /tmp/frames.json docs/demo.gif \
  "/path/to/JetBrains Mono … Regular.ttf" "two Claude Code agents, one port: Le Clerk" 0 13
```

`render.py` needs Python 3 with Pillow and a monospace TrueType font (its
Bold face is used when it sits beside the Regular one).

`record.py` records a real CLI run instead: two shells racing for the port
through `clerk run`, against a scratch `CLERK_HOME` (needs tmux and port
18080 free; `serve-api` is the stand-in server).
