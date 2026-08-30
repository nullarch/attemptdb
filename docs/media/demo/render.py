#!/usr/bin/env python3
"""Render a terminal-session GIF from captured, real command output.

Every line of output comes from files captured by running the real binary
against the sanitised public snapshot (and `attempt doctor` with the home
directory elided). This script only animates them: it types the command,
reveals the output line by line, pauses, and moves on. Nothing is invented.
"""
import os
import re
import sys
from PIL import Image, ImageDraw, ImageFont

HERE = os.path.dirname(os.path.abspath(__file__))  # captures live next to this script
OUT_GIF = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "demo.gif")
OUT_PNG = os.path.splitext(OUT_GIF)[0] + "-still.png"

# ---- look ----------------------------------------------------------------
SCALE = 2                     # render at 2x, downsample for crisp text
COLS, ROWS = 104, 30
FONT_PX = 13 * SCALE
LINE_H = int(FONT_PX * 1.42)
PAD_X, PAD_Y = 22 * SCALE, 14 * SCALE
TITLE_H = 34 * SCALE
FONT_PATH = "/System/Library/Fonts/Menlo.ttc"

BG = (19, 20, 22)          # attemptdb-ui --bg (dark)
CHROME = (28, 30, 33)      # --card
INK = (232, 232, 230)      # --ink
MUTED = (154, 154, 150)    # --muted
ACCENT = (122, 162, 255)   # --accent
OK = (76, 194, 122)
FAIL = (255, 107, 107)
WARN = (240, 176, 74)
SUP = (179, 157, 219)
LIVE = (77, 208, 225)

font = ImageFont.truetype(FONT_PATH, FONT_PX)
CHAR_W = font.getlength("M")
W = int(PAD_X * 2 + CHAR_W * COLS)
H = int(TITLE_H + PAD_Y * 2 + LINE_H * ROWS)

# ---- content --------------------------------------------------------------
def read(name, limit=None):
    with open(os.path.join(HERE, name), encoding="utf-8") as f:
        lines = [l.rstrip("\n") for l in f]
    return lines[:limit] if limit else lines

KV = re.compile(r"^(\S+)(\s{2,})(\S)")

def clip(lines, width=COLS):
    """Wrap long lines at word boundaries with a hanging indent: key/value
    rows continue under the value column, everything else two columns in
    from its own indentation. Table rows (box drawing) are clipped instead."""
    out = []
    for l in lines:
        if len(l) <= width:
            out.append(l)
            continue
        if l.lstrip().startswith(("│", "┌", "└", "├", "╞")):
            out.append(l[: width - 1] + "…")
            continue
        m = KV.match(l)
        indent = m.start(3) if m else len(l) - len(l.lstrip()) + 2
        indent = min(indent, width // 3)
        first, rest = l[:width], l[width:]
        cut = first.rfind(" ")
        if cut > indent + 8:
            first, rest = l[:cut], l[cut + 1 :]
        out.append(first)
        while rest:
            chunk = " " * indent + rest
            if len(chunk) <= width:
                out.append(chunk)
                break
            cut = chunk.rfind(" ", indent + 8, width)
            if cut <= indent:
                cut = width
            out.append(chunk[:cut])
            rest = chunk[cut:].lstrip()
    return out

# Each scene: (command shown, output lines, hold seconds after output)
SCENES = [
    ("attempt doctor", clip(read("doctor.txt")), 2.6),
    ("attempt timeline", clip(read("timeline.txt")), 4.0),
    ("attempt query \"SELECT attempt_id, outcome, failure_class, approach FROM attempts WHERE outcome = 'failed' LIMIT 4\"", clip(read("failures.txt")), 3.6),
    ("attempt why att_a9c319da", clip(read("why.txt")), 5.0),
    ("attempt trace att_a9c319da", clip(read("trace.txt", 11)), 3.4),
]

# ---- colouring rules (token-level, no ANSI in the captured text) ---------
RULES = [
    (re.compile(r"✓|succeeded|active|verified|COMPATIBLE|ok\b"), OK),
    (re.compile(r"✗|failed|nonzero_exit|file_not_found|string_mismatch|untrusted|missing"), FAIL),
    (re.compile(r"↻|superseded|▶|in progress|no stop seen|abandoned|configured"), WARN),
    (re.compile(r"\b(att|ses|ev|prj)_[0-9a-f-]+"), ACCENT),
    (re.compile(r"conf [0-9.]+|confidence\s+[0-9.]+|tier1-v0|deterministic"), SUP),
    (re.compile(r"^(claim|uncertainty|evidence|outcome|failure_class|edge_kind|approach)\b"), LIVE),
    (re.compile(r"^note:.*|\(activity before the first prompt\)|\(prompt, .*\)|Minimal coverage|Partial coverage|Full coverage"), MUTED),
]

def spans(line):
    """Colour spans for a line: list of (start, end, colour), non-overlapping, first rule wins."""
    taken = [None] * len(line)
    for rx, colour in RULES:
        for m in rx.finditer(line):
            for i in range(m.start(), m.end()):
                if taken[i] is None:
                    taken[i] = colour
    out, i = [], 0
    while i < len(line):
        c = taken[i]
        j = i
        while j < len(line) and taken[j] == c:
            j += 1
        out.append((i, j, c or INK))
        i = j
    return out

# ---- drawing ----------------------------------------------------------------
def frame(buffer, cursor_line=None, cursor_col=None, cursor_on=True):
    img = Image.new("RGB", (W, H), BG)
    d = ImageDraw.Draw(img)
    # title bar
    d.rectangle([0, 0, W, TITLE_H], fill=CHROME)
    for k, col in enumerate([(255, 95, 86), (255, 189, 46), (39, 201, 63)]):
        cx = PAD_X + k * 22 * SCALE
        r = 6 * SCALE
        d.ellipse([cx - r, TITLE_H / 2 - r, cx + r, TITLE_H / 2 + r], fill=col)
    title = "attempt — the database for what agents tried"
    tw = font.getlength(title)
    d.text(((W - tw) / 2, (TITLE_H - FONT_PX) / 2 - 2), title, font=font, fill=MUTED)
    # body: soft-wrap every line like a terminal; the cursor sits at the end
    # of the last wrapped segment of its line
    rows = []
    for idx, line in enumerate(buffer):
        segs = clip([line])
        for k, seg in enumerate(segs):
            rows.append((idx, seg, k == len(segs) - 1))
    visible = rows[-ROWS:]
    y = TITLE_H + PAD_Y
    for idx, line, last in visible:
        x = PAD_X
        for s, e, colour in spans(line):
            seg = line[s:e]
            d.text((x, y), seg, font=font, fill=colour)
            x += font.getlength(seg)
        if cursor_on and cursor_line is not None and idx == cursor_line and last:
            d.rectangle([x, y + 2, x + CHAR_W, y + FONT_PX + 2], fill=INK)
        y += LINE_H
    return img.resize((W // SCALE, H // SCALE), Image.LANCZOS)

frames, durations = [], []
def emit(img, ms):
    frames.append(img)
    durations.append(ms)

buffer = []
PROMPT = "$ "
for i, (cmd, output, hold) in enumerate(SCENES):
    buffer.append(PROMPT)
    line_idx = len(buffer) - 1
    # blink once before typing
    emit(frame(buffer, line_idx, len(PROMPT), True), 420)
    emit(frame(buffer, line_idx, len(PROMPT), False), 320)
    # type two characters per frame
    for j in range(0, len(cmd) + 1, 2):
        typed = cmd[: min(j + 2, len(cmd))]
        buffer[line_idx] = PROMPT + typed
        emit(frame(buffer, line_idx, len(PROMPT) + len(typed), True), 46)
    buffer[line_idx] = PROMPT + cmd
    emit(frame(buffer, line_idx, len(PROMPT) + len(cmd), True), 380)
    # reveal output
    for k, line in enumerate(output):
        buffer.append(line)
        # first lines slower, then faster; blank lines instant
        ms = 20 if not line.strip() else (110 if k < 3 else 55)
        emit(frame(buffer), ms)
    buffer.append("")
    still = frame(buffer)
    emit(still, int(hold * 1000))
    still.save(os.path.splitext(OUT_GIF)[0] + f"-scene{i + 1}.png")
    if i < len(SCENES) - 1:
        # clear for the next scene, keep the transcript feel with a short cut
        buffer = []

# final hold
emit(frame(buffer), 2500)

frames[0].save(
    OUT_GIF,
    save_all=True,
    append_images=frames[1:],
    duration=durations,
    loop=0,
    optimize=True,
    disposal=1,
)
# a still of the `why` scene for the README fallback / social preview
frames[-1].save(OUT_PNG)
total = sum(durations) / 1000
print(f"{OUT_GIF}: {len(frames)} frames, {total:.1f}s, {os.path.getsize(OUT_GIF)/1e6:.2f} MB, {W//SCALE}x{H//SCALE}")
