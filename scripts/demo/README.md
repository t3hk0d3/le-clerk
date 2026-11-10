# The README demo

`docs/demo.gif` is a real run, scripted: two shells (“agents”) start the same
API dev server through `clerk run` on one port, against a scratch
`CLERK_HOME`.

```sh
python3 scripts/demo/record.py "$(command -v clerk)" /tmp/clerk-demo
python3 scripts/demo/render.py /tmp/clerk-demo/frames.json docs/demo.gif /path/to/Mono.ttf
```

Needs tmux, Python 3 with Pillow, a monospace TrueType font, and port 18080
free. `serve-api` is the stand-in server.
