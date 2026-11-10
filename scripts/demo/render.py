#!/usr/bin/env python3
"""Render frames.json (from record.py or record-agents.py) to an animated GIF.

    render.py <frames.json> <out.gif> <monospace .ttf> [title] [max frame ms] [font px]
"""
import json, re, sys
from PIL import Image, ImageDraw, ImageFont

data = json.load(open(sys.argv[1]))
TITLE_TEXT = sys.argv[4] if len(sys.argv) > 4 else "two agents, one port: Le Clerk"
MAX_MS = int(sys.argv[5]) if len(sys.argv) > 5 else 0
font = ImageFont.truetype(sys.argv[3], int(sys.argv[6]) if len(sys.argv) > 6 else 15)
# The font's bold face, when it sits beside the regular one; else faked.
_bold_path = sys.argv[3].replace("Regular", "Bold")
try:
    bold = ImageFont.truetype(_bold_path, font.size) if _bold_path != sys.argv[3] else None
except OSError:
    bold = None
CW = int(font.getlength("M")); CH = int(font.size * 1.33)
BG, FG, BAR = (30, 31, 41), (220, 223, 228), (68, 71, 90)
PAL = {0: (40, 42, 54), 1: (255, 85, 85), 2: (80, 250, 123), 3: (241, 250, 140), 4: (139, 233, 253),
       5: (255, 121, 198), 6: (139, 233, 253), 7: (220, 223, 228), 8: (98, 114, 164), 9: (255, 110, 110),
       10: (105, 255, 148), 11: (241, 250, 140), 12: (189, 147, 249), 13: (255, 146, 223), 14: (164, 255, 255), 15: (255, 255, 255)}
SGR = re.compile(r"\x1b\[([0-9;]*)m")
# OSC sequences (hyperlinks, titles): dropped, their text kept.
OSC = re.compile(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)")
# Glyphs the font may lack, drawn as near equivalents.
GLYPHS = {"✻": "*", "✢": "*", "✳": "*", "✶": "*", "✽": "*", "⏺": "●", "⎿": "└",
          "⏵": "▸", "↯": "!", "⧉": "#"}

def cells(line):
    line = OSC.sub("", line)
    out, fg, b, i = [], FG, False, 0
    for m in SGR.finditer(line):
        for ch in line[i:m.start()]:
            out.append((ch, fg, b))
        i = m.end()
        codes = [int(c) for c in m.group(1).split(";") if c] or [0]
        k = 0
        while k < len(codes):
            c = codes[k]
            if c == 0: fg, b = FG, False
            elif c == 1: b = True
            elif c == 22: b = False
            elif c == 39: fg = FG
            elif 30 <= c <= 37: fg = PAL[c - 30]
            elif 90 <= c <= 97: fg = PAL[c - 90 + 8]
            elif c == 38 and k + 2 < len(codes) and codes[k + 1] == 5:
                fg = PAL.get(codes[k + 2], FG); k += 2
            elif c == 38 and k + 4 < len(codes) and codes[k + 1] == 2:
                fg = tuple(codes[k + 2:k + 5]); k += 4
            elif c == 48 and k + 1 < len(codes):
                k += 4 if codes[k + 1] == 2 else 2
            k += 1
    for ch in line[i:]:
        out.append((ch, fg, b))
    return out

W, H = data["width"], data["height"]
half = (W - 1) // 2
PAD, TITLE = 14, 30
LABELS = ["agent A", "agent B"]
img_w, img_h = W * CW + 2 * PAD, H * CH + 2 * PAD + TITLE + CH

def draw(fr):
    im = Image.new("RGB", (img_w, img_h), BG)
    d = ImageDraw.Draw(im)
    d.rectangle([0, 0, img_w, TITLE], fill=(24, 25, 33))
    for k, col in enumerate([(255, 95, 86), (255, 189, 46), (39, 201, 63)]):
        d.ellipse([PAD + k * 20, 10, PAD + k * 20 + 11, 21], fill=col)
    title = TITLE_TEXT
    d.text(((img_w - font.getlength(title)) // 2, 7), title, font=font, fill=(150, 154, 170))
    x0 = [PAD, PAD + (half + 1) * CW]
    for p, text in enumerate(fr["panes"]):
        for r, line in enumerate(text.split("\n")[:H]):
            for c, (ch, fg, b) in enumerate(cells(line)[:half]):
                ch = GLYPHS.get(ch, ch)
                if ch != " ":
                    x, y = x0[p] + c * CW, TITLE + PAD + CH + r * CH
                    if b and bold:
                        d.text((x, y), ch, font=bold, fill=fg)
                    else:
                        d.text((x, y), ch, font=font, fill=fg, stroke_width=1 if b else 0,
                               stroke_fill=fg)
    for p, label in enumerate(LABELS):
        d.text((x0[p] + (half * CW - font.getlength(label)) // 2, TITLE + 2), label,
               font=font, fill=(139, 233, 253))
    xs = PAD + half * CW + CW // 2
    d.line([xs, TITLE + PAD, xs, img_h - PAD], fill=BAR, width=1)
    return im

frames, durs, last, palette = [], [], None, None
fs = data["frames"]
for n, fr in enumerate(fs):
    key = "\0".join(fr["panes"])
    nxt = fs[n + 1]["t"] if n + 1 < len(fs) else fr["t"] + 3.0
    dur = max(40, int((nxt - fr["t"]) * 1000))
    if MAX_MS:
        dur = min(dur, MAX_MS)
    if key == last:
        durs[-1] = min(durs[-1] + dur, MAX_MS) if MAX_MS else durs[-1] + dur
        continue
    rgb = draw(fr)
    if palette is None:
        # One palette for every frame (from the busiest, last frame), so
        # unchanged regions stay identical and only changes are stored.
        palette = draw(fs[-1]).quantize(colors=32, method=Image.Quantize.MEDIANCUT)
    frames.append(rgb.quantize(palette=palette, dither=Image.Dither.NONE))
    durs.append(dur); last = key
durs[-1] = 4000
frames[0].save(sys.argv[2], save_all=True, append_images=frames[1:], duration=durs, loop=0, optimize=False, disposal=1)
print(len(frames), "distinct frames,", round(sum(durs) / 1000, 1), "s")
