#!/usr/bin/env python3
"""Render the Agent Timeline demo GIF (and the README still) from real screens.

Nothing here is drawn by hand. The frames are browser screenshots of
`attempt ui --demo` — the bundled, clearly labelled build-history demo, which
is deterministic, so anyone can reproduce them:

    attempt ui --demo --no-open --port 8810
    # capture the viewport at each route, in this order:
    #   /?demo=1
    #   /attention?demo=1          (with the "why" disclosure opened)
    #   /work?demo=1
    #   /attempt/<the superseded attempt>?demo=1
    #   /card.svg?demo=1
    # save them as frames/00.jpg .. 04.jpg

This script only crops, scales, captions and sequences them:

    python3 docs/media/ui/render.py <frames-dir> docs/media/ui-demo.gif

It also writes the still used in the README next to the GIF
(`agent-timeline.png`), which is frame 0 at full width.

Requires Pillow, like docs/media/demo/render.py.
"""
import os
import sys

from PIL import Image, ImageChops, ImageDraw, ImageFont

HERE = os.path.dirname(os.path.abspath(__file__))
FRAMES = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "frames")
OUT_GIF = sys.argv[2] if len(sys.argv) > 2 else os.path.join(HERE, "..", "ui-demo.gif")
OUT_PNG = os.path.join(os.path.dirname(OUT_GIF), "agent-timeline.png")

# ---- look ----------------------------------------------------------------
WIDTH = 1000                  # rendered width; the README displays it smaller
PAGE_CSS_WIDTH = 1400         # `main { max-width: 1400px }` — crop the dead space
BG = (19, 20, 22)             # attemptdb-ui --bg (dark)
CARD = (28, 30, 33)           # --card
INK = (232, 232, 230)         # --ink
MUTED = (154, 154, 150)       # --muted
ACCENT = (122, 162, 255)      # --accent (dark theme)
LINE = (44, 47, 51)           # --line
CAPTION_H = 34
PAD = 14
FONT_PATH = "/System/Library/Fonts/Helvetica.ttc"
MONO_PATH = "/System/Library/Fonts/Menlo.ttc"

# (frame file, route, caption, hold in milliseconds)
STORY = [
    ("00.jpg", "/",
     "current work, what needs you, live execution, the attempt path", 5200),
    ("01.jpg", "/attention",
     "one unanswered permission request — with its evidence and what the rule cannot see", 5200),
    ("02.jpg", "/work",
     "active / blocked / recently finished, over inferred work units", 5200),
    ("03.jpg", "/attempt/att_90db7973",
     "the attempt that failed, and the attempt that superseded it", 5200),
    ("04.jpg", "attempt ui export card.svg",
     "a sanitized summary card for a README or an issue", 4400),
]


def font(path, size):
    try:
        return ImageFont.truetype(path, size)
    except OSError:
        return ImageFont.load_default()


def card_gap_cut(im, box):
    """Cut the page at the gap between two cards, so a frame never ends
    halfway through one."""
    x0, y0, x1, y1 = box
    span = range(x0 + 6, min(x1, im.size[0]) - 6, 9)
    run = 0
    for y in range(y1 - 4, y0 + 200, -1):
        row = (im.getpixel((x, y)) for x in span)
        if all(abs(r - BG[0]) < 9 and abs(g - BG[1]) < 9 and abs(b - BG[2]) < 9 for r, g, b in row):
            run += 1
            if run >= 8:
                return (x0, y0, x1, y + run // 2)
        else:
            run = 0
    return box


def content_box(im):
    """Crop away the browser's dead space: the page is `main`-wide, and the
    document ends at the gap after the last whole card."""
    w, h = im.size
    scale = w / 1751.0  # the captured viewport's CSS width
    right = min(w, int(PAGE_CSS_WIDTH * scale))
    ref = Image.new("RGB", im.size, im.getpixel((10, h - 10)))
    mask = ImageChops.difference(im, ref).convert("L").point(lambda p: 255 if p > 14 else 0)
    box = mask.getbbox() or (0, 0, w, h)
    return card_gap_cut(im, (0, 0, right, min(h, box[3] + 10)))


def card_box(im):
    """The summary card is served as a standalone SVG, so the viewer paints a
    white page around it. The card sits at the top left; walk out from inside
    it until the page turns white. (A whole-image bounding box would also
    catch the window's own dark edge.)"""
    w, h = im.size
    grey = im.convert("L")
    bright = lambda x, y: grey.getpixel((x, y)) > 150
    probe_y = max(4, h // 8)
    right = w
    for x in range(8, w):
        if bright(x, probe_y) and bright(min(w - 1, x + 3), probe_y):
            right = x
            break
    probe_x = max(4, right // 2)
    bottom = h
    for y in range(8, h):
        if bright(probe_x, y) and bright(probe_x, min(h - 1, y + 3)):
            bottom = y
            break
    return (0, 0, right, bottom)


def frame(path, route, caption, page=True, height=None):
    im = Image.open(path).convert("RGB")
    if page:
        im = im.crop(content_box(im))
        im = im.resize((WIDTH, max(1, int(im.size[1] * WIDTH / im.size[0]))), Image.LANCZOS)
        body_h = im.size[1] if height is None else height - CAPTION_H
        canvas = Image.new("RGB", (WIDTH, body_h + CAPTION_H), BG)
        canvas.paste(im, (0, 0))
    else:
        # The exported card, shown on the product's own ground.
        im = im.crop(card_box(im))
        target = int(WIDTH * 0.86)
        im = im.resize((target, max(1, int(im.size[1] * target / im.size[0]))), Image.LANCZOS)
        body_h = im.size[1] + 72 if height is None else height - CAPTION_H
        canvas = Image.new("RGB", (WIDTH, body_h + CAPTION_H), BG)
        canvas.paste(im, ((WIDTH - target) // 2, max(0, (body_h - im.size[1]) // 2)))
    d = ImageDraw.Draw(canvas)
    y = body_h
    d.line([(0, y), (WIDTH, y)], fill=LINE)
    d.rectangle([0, y + 1, WIDTH, canvas.size[1]], fill=CARD)
    mono = font(MONO_PATH, 11)
    sans = font(FONT_PATH, 11)
    d.text((PAD, y + 11), route, font=mono, fill=ACCENT)
    x = PAD + int(d.textlength(route, font=mono)) + 12
    d.text((x, y + 11), caption, font=sans, fill=MUTED)
    return canvas


def main():
    paths = []
    for name, route, caption, hold in STORY:
        path = os.path.join(FRAMES, name)
        if not os.path.exists(path):
            sys.exit(f"missing frame {path} — see the capture recipe in this file's docstring")
        paths.append(path)
    # Every frame is the same size, so the animation never jumps.
    height = max(frame(p, r, c, page=(i < 4)).size[1]
                 for i, (p, (_, r, c, _)) in enumerate(zip(paths, STORY)))
    frames = [frame(p, r, c, page=(i < 4), height=height)
              for i, (p, (_, r, c, _)) in enumerate(zip(paths, STORY))]
    durations = [hold for _, _, _, hold in STORY]

    # One palette for the whole animation: per-frame quantisation makes the
    # dark UI flicker between frames.
    montage = Image.new("RGB", (WIDTH, height * len(frames)))
    for i, f in enumerate(frames):
        montage.paste(f, (0, i * height))
    palette = montage.quantize(colors=200, method=Image.MEDIANCUT)
    frames = [f.quantize(palette=palette, dither=Image.NONE) for f in frames]

    frames[0].save(
        OUT_GIF,
        save_all=True,
        append_images=frames[1:],
        duration=durations,
        loop=0,
        optimize=True,
        disposal=1,
    )
    still = Image.open(os.path.join(FRAMES, STORY[0][0])).convert("RGB")
    still = still.crop(content_box(still))
    still.save(OUT_PNG, optimize=True)
    print(f"{OUT_GIF}  {os.path.getsize(OUT_GIF) // 1024} KB, "
          f"{sum(durations) / 1000:.0f}s, {len(frames)} frames")
    print(f"{OUT_PNG}  {os.path.getsize(OUT_PNG) // 1024} KB, {still.size[0]}x{still.size[1]}")


if __name__ == "__main__":
    main()
