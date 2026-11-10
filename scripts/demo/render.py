#!/usr/bin/env python3
"""Render frames.json (from record.py) to an animated GIF.

    render.py <frames.json> <out.gif> <monospace .ttf>
"""
import json, re, sys
from PIL import Image, ImageDraw, ImageFont

data = json.load(open(sys.argv[1]))
font = ImageFont.truetype(sys.argv[3], 15)
bold = font
CW = int(font.getlength("M")); CH = 20
BG, FG, BAR = (30, 31, 41), (220, 223, 228), (68, 71, 90)
PAL = {0: (40, 42, 54), 1: (255, 85, 85), 2: (80, 250, 123), 3: (241, 250, 140), 4: (139, 233, 253),
       5: (255, 121, 198), 6: (139, 233, 253), 7: (220, 223, 228), 8: (98, 114, 164), 9: (255, 110, 110),
       10: (105, 255, 148), 11: (241, 250, 140), 12: (189, 147, 249), 13: (255, 146, 223), 14: (164, 255, 255), 15: (255, 255, 255)}
SGR = re.compile(r"\x1b\[([0-9;]*)m")

def cells(line):
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
            k += 1
    for ch in line[i:]:
        out.append((ch, fg, b))
    return out

W, H = data["width"], data["height"]
half = (W - 1) // 2
PAD, TITLE = 14, 30
img_w, img_h = W * CW + 2 * PAD, H * CH + 2 * PAD + TITLE

def draw(fr):
    im = Image.new("RGB", (img_w, img_h), BG)
    d = ImageDraw.Draw(im)
    d.rectangle([0, 0, img_w, TITLE], fill=(24, 25, 33))
    for k, col in enumerate([(255, 95, 86), (255, 189, 46), (39, 201, 63)]):
        d.ellipse([PAD + k * 20, 10, PAD + k * 20 + 11, 21], fill=col)
    title = "two agents, one port: Le Clerk"
    d.text(((img_w - font.getlength(title)) // 2, 7), title, font=font, fill=(150, 154, 170))
    x0 = [PAD, PAD + (half + 1) * CW]
    for p, text in enumerate(fr["panes"]):
        for r, line in enumerate(text.split("\n")[:H]):
            for c, (ch, fg, b) in enumerate(cells(line)[:half]):
                if ch != " ":
                    x, y = x0[p] + c * CW, TITLE + PAD + r * CH
                    d.text((x, y), ch, font=font, fill=fg, stroke_width=1 if b else 0, stroke_fill=fg)
    xs = PAD + half * CW + CW // 2
    d.line([xs, TITLE + PAD, xs, img_h - PAD], fill=BAR, width=1)
    return im

frames, durs, last = [], [], None
fs = data["frames"]
for n, fr in enumerate(fs):
    key = "\0".join(fr["panes"])
    nxt = fs[n + 1]["t"] if n + 1 < len(fs) else fr["t"] + 3.0
    dur = max(40, int((nxt - fr["t"]) * 1000))
    if key == last:
        durs[-1] += dur; continue
    frames.append(draw(fr).convert("P", palette=Image.ADAPTIVE, colors=24)); durs.append(dur); last = key
durs[-1] = 4000
frames[0].save(sys.argv[2], save_all=True, append_images=frames[1:], duration=durs, loop=0, optimize=True, disposal=2)
print(len(frames), "distinct frames,", round(sum(durs) / 1000, 1), "s")
