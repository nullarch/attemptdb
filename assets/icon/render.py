#!/usr/bin/env python3
"""Render AttemptDB's icon at every size the platforms ask for.

The mark is what the database actually holds, drawn: a session marker, the
stem that runs down from it, and the attempts branching off — one short and
muted because it never finished, one that landed. The stem runs on past the
last branch: the log is append-only. The violet is the accent the console
and the Event v1 badge already use.

Small sizes are a different drawing, not a shrunk one. Below 48 px the second
branch and the indentation turn to mush, so 16/24/32 get a heavier version
with one branch, which keeps the same silhouette at a taskbar's size.

    python3 assets/icon/render.py     # writes attemptdb.ico, the PNGs, the SVG

Needs Pillow. Everything it writes is committed; this exists so the icon can
be regenerated rather than resurrected from a PNG.
"""

from PIL import Image, ImageDraw
from pathlib import Path

OUT = Path(__file__).resolve().parent
SS = 8  # supersampling: draw large, resample down, get the antialiasing free

BG_TOP = (25, 28, 40)      # slate with a violet bias, so the tile is not "black"
BG_BOTTOM = (15, 16, 22)
EDGE = (255, 255, 255, 34)  # a rim, so the tile survives a black taskbar
HEAD_TOP = (199, 186, 255)
HEAD_BOTTOM = (150, 110, 250)
STEM = (139, 92, 246)
INK = (233, 236, 242)
MUTED = (122, 131, 147)


def lerp(a, b, t):
    return tuple(round(x + (y - x) * t) for x, y in zip(a, b))


def tile(size):
    """The rounded slate tile: a vertical gradient and a hairline rim."""
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    grad = Image.new("RGBA", (1, size))
    for y in range(size):
        grad.putpixel((0, y), lerp(BG_TOP, BG_BOTTOM, y / max(1, size - 1)) + (255,))
    grad = grad.resize((size, size))
    mask = Image.new("L", (size, size), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        [0, 0, size - 1, size - 1], radius=round(size * 0.22), fill=255
    )
    img.paste(grad, (0, 0), mask)
    rim = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    ImageDraw.Draw(rim).rounded_rectangle(
        [0, 0, size - 1, size - 1],
        radius=round(size * 0.22),
        outline=EDGE,
        width=max(1, round(size * 0.008)),
    )
    return Image.alpha_composite(img, rim)


def head(img, box):
    """The session marker: a rounded bar with a vertical gradient."""
    x0, y0, x1, y1 = (round(v) for v in box)
    w, h = max(1, x1 - x0), max(1, y1 - y0)
    grad = Image.new("RGBA", (1, h))
    for y in range(h):
        grad.putpixel((0, y), lerp(HEAD_TOP, HEAD_BOTTOM, y / max(1, h - 1)) + (255,))
    grad = grad.resize((w, h))
    mask = Image.new("L", (w, h), 0)
    ImageDraw.Draw(mask).rounded_rectangle([0, 0, w - 1, h - 1], radius=w / 2, fill=255)
    img.paste(grad, (x0, y0), mask)


def full(size):
    """>= 48 px: the marker, the stem, two attempts. Authored on a 256 grid."""
    s = size * SS
    img = tile(s)
    d = ImageDraw.Draw(img)
    u = s / 256

    def r(x0, y0, x1, y1, radius, fill):
        d.rounded_rectangle([x0 * u, y0 * u, x1 * u, y1 * u], radius=radius * u, fill=fill)

    head(img, (54 * u, 48 * u, 80 * u, 98 * u))
    r(60, 90, 75, 200, 7.5, STEM)                 # the stem, past the last branch
    r(98, 54, 212, 73, 9.5, INK)                  # the session
    r(60, 126, 102, 140, 7, STEM)                 # branch 1
    r(112, 110, 170, 129, 9.5, MUTED)             # the attempt that stopped short
    r(60, 180, 102, 194, 7, STEM)                 # branch 2
    r(112, 164, 206, 183, 9.5, INK)               # the one that landed
    return img.resize((size, size), Image.LANCZOS)


def small(size):
    """16-32 px: one branch, heavier strokes. The silhouette survives."""
    s = size * SS
    img = tile(s)
    d = ImageDraw.Draw(img)
    u = s / 256

    def r(x0, y0, x1, y1, radius, fill):
        d.rounded_rectangle([x0 * u, y0 * u, x1 * u, y1 * u], radius=radius * u, fill=fill)

    head(img, (52 * u, 46 * u, 86 * u, 104 * u))
    r(60, 96, 78, 186, 9, STEM)
    r(60, 166, 112, 186, 9, STEM)
    r(100, 50, 214, 78, 14, INK)
    r(124, 152, 206, 180, 14, INK)
    return img.resize((size, size), Image.LANCZOS)


def draw(size):
    return small(size) if size <= 32 else full(size)


SVG = """<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256" width="256" height="256" role="img" aria-label="AttemptDB">
  <defs>
    <linearGradient id="a-tile" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#191c28"/><stop offset="1" stop-color="#0f1016"/>
    </linearGradient>
    <linearGradient id="a-head" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#c7baff"/><stop offset="1" stop-color="#966efa"/>
    </linearGradient>
  </defs>
  <rect width="256" height="256" rx="56" fill="url(#a-tile)"/>
  <rect x="1" y="1" width="254" height="254" rx="55" fill="none" stroke="#ffffff" stroke-opacity="0.13" stroke-width="2"/>
  <rect x="54" y="48" width="26" height="50" rx="13" fill="url(#a-head)"/>
  <rect x="60" y="90" width="15" height="110" rx="7.5" fill="#8b5cf6"/>
  <rect x="98" y="54" width="114" height="19" rx="9.5" fill="#e9ecf2"/>
  <rect x="60" y="126" width="42" height="14" rx="7" fill="#8b5cf6"/>
  <rect x="112" y="110" width="58" height="19" rx="9.5" fill="#7a8393"/>
  <rect x="60" y="180" width="42" height="14" rx="7" fill="#8b5cf6"/>
  <rect x="112" y="164" width="94" height="19" rx="9.5" fill="#e9ecf2"/>
</svg>
"""

def _dib(img):
    """One ICO frame as a bottom-up 32-bit DIB with the AND mask Windows
    still expects. Pillow writes PNG frames at every size; the shell reads
    PNG only from Vista on, and not everywhere even then, so anything below
    256 goes out as a plain DIB."""
    import struct

    w, h = img.size
    px = img.convert("RGBA").load()
    xor = bytearray()
    for y in range(h - 1, -1, -1):  # bottom-up
        for x in range(w):
            r, g, b, a = px[x, y]
            xor += bytes((b, g, r, a))
    row = ((w + 31) // 32) * 4  # the 1-bit mask, rows padded to 4 bytes
    and_mask = bytes(row * h)
    header = struct.pack(
        "<IiiHHIIiiII", 40, w, h * 2, 1, 32, 0, len(xor) + len(and_mask), 0, 0, 0, 0
    )
    return header + bytes(xor) + and_mask


def write_ico(path, frames):
    """ICONDIR + one entry per size. DIB below 256, PNG at 256 (where it is
    both universally supported and much smaller)."""
    import io
    import struct

    blobs = []
    for img in frames:
        if img.size[0] >= 256:
            buf = io.BytesIO()
            img.save(buf, format="PNG")
            blobs.append(buf.getvalue())
        else:
            blobs.append(_dib(img))
    out = bytearray(struct.pack("<HHH", 0, 1, len(frames)))
    offset = 6 + 16 * len(frames)
    for img, blob in zip(frames, blobs):
        w, h = img.size
        out += struct.pack(
            "<BBBBHHII", w % 256, h % 256, 0, 0, 1, 32, len(blob), offset
        )
        offset += len(blob)
    for blob in blobs:
        out += blob
    path.write_bytes(bytes(out))


if __name__ == "__main__":
    sizes = [16, 24, 32, 48, 64, 128, 256]
    frames = [draw(n) for n in sizes]
    write_ico(OUT / "attemptdb.ico", frames)
    for n in (16, 32, 48, 256, 512, 1024):
        draw(n).save(OUT / f"attemptdb-{n}.png")
    (OUT / "attemptdb.svg").write_text(SVG)
    # The single-file UI export cannot fetch a URL, so it carries the mark
    # inline. Generated here so it can never drift from the master.
    import base64

    b64 = base64.b64encode(SVG.encode()).decode()
    (OUT / "../../crates/attemptdb-ui/assets/favicon.b64").resolve().write_text(b64)
    print("wrote", ", ".join(sorted(p.name for p in OUT.glob("attemptdb*"))))
