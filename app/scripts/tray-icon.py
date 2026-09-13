#!/usr/bin/env python3
"""Render the menu-bar (tray) icon: the AttemptDB mark as a template image.

A macOS menu-bar icon is a stencil — black shapes on a transparent ground,
and the system recolours it for light and dark menu bars — so the tile,
gradient and violet of the app icon do not apply. This is the same drawing
as assets/icon/render.py's small size: the session marker, the stem, one
attempt branching off it, and the two lines the branches point at.

    python3 app/scripts/tray-icon.py    # writes app/src-tauri/icons/tray.png

The PNG is 44×44 px for a 22-point icon on a Retina menu bar (tray-icon
sizes it in points). Needs Pillow. The output is committed.
"""

from pathlib import Path

from PIL import Image, ImageDraw

OUT = Path(__file__).resolve().parents[1] / "src-tauri" / "icons" / "tray.png"
SIZE = 44
SS = 8
INK = (0, 0, 0, 255)


def rounded(draw, box, radius):
    draw.rounded_rectangle(box, radius=radius, fill=INK)


def main():
    big = SIZE * SS
    img = Image.new("RGBA", (big, big), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    u = big / 44  # one output pixel
    # The marker: a pill at the top of the stem.
    rounded(d, (8 * u, 6 * u, 15 * u, 19 * u), 3.5 * u)
    # The stem, running past the last branch: the log is append-only.
    rounded(d, (9.5 * u, 16 * u, 13.5 * u, 40 * u), 2 * u)
    # The branch to the attempt that landed.
    rounded(d, (9.5 * u, 26 * u, 21 * u, 30 * u), 2 * u)
    # The two lines: what the session said, and what the attempt changed.
    rounded(d, (19 * u, 8 * u, 38 * u, 13 * u), 2.5 * u)
    rounded(d, (23 * u, 24.5 * u, 38 * u, 29.5 * u), 2.5 * u)
    img = img.resize((SIZE, SIZE), Image.LANCZOS)
    OUT.parent.mkdir(parents=True, exist_ok=True)
    img.save(OUT)
    print(f"wrote {OUT} ({SIZE}x{SIZE})")


if __name__ == "__main__":
    main()
