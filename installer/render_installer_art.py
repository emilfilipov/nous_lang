#!/usr/bin/env python3
"""Render the branded WiX installer bitmaps for the Lullaby Windows installer.

WixUI dialogs use two fixed-size 24-bit BMPs:
    banner.bmp   493x58   top strip on most wizard pages
    dialog.bmp   493x312  full background on the Welcome and Exit pages

Both carry the Lullaby mark (the "L cradling a crescent moon") on a soft pastel
ground, matching the visual identity. Drawn with shapes only (no font dependency),
supersampled and downsampled for clean edges. Run:

    python installer/render_installer_art.py
"""
from __future__ import annotations

from pathlib import Path

from PIL import Image, ImageDraw

HERE = Path(__file__).resolve().parent
SS = 3                      # supersample factor
CREAM = (255, 248, 239)
LAV = (139, 111, 240)       # #8b6ff0
G0, G1, G2 = (244, 240, 255), (234, 246, 255), (253, 238, 238)  # cream-lavender / sky / peach


def lerp(a, b, t):
    return tuple(round(a[i] + (b[i] - a[i]) * t) for i in range(3))


def gradient(w: int, h: int) -> Image.Image:
    small = Image.new("RGB", (64, 64))
    px = small.load()
    for y in range(64):
        for x in range(64):
            t = (x + y) / 126
            px[x, y] = lerp(G0, G1, t * 2) if t < 0.5 else lerp(G1, G2, (t - 0.5) * 2)
    return small.resize((w, h), Image.LANCZOS)


def draw_mark(img: Image.Image, cx: float, cy: float, size: float, color) -> None:
    """Draw the L+crescent+star mark centred at (cx, cy), `size` px tall."""
    s = size / 120.0
    layer = Image.new("RGBA", img.size, (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)

    def P(x, y):
        return (cx + (x - 60) * s, cy + (y - 54) * s)

    lw = max(1, round(13 * s))
    pts = [P(43, 30), P(43, 78), P(74, 78)]
    d.line(pts, fill=color + (255,), width=lw, joint="curve")
    r = lw / 2
    for (px, py) in (pts[0], pts[2]):
        d.ellipse([px - r, py - r, px + r, py + r], fill=color + (255,))

    moon = Image.new("RGBA", img.size, (0, 0, 0, 0))
    md = ImageDraw.Draw(moon)
    mx, my = P(71, 55)
    md.ellipse([mx - 18 * s, my - 18 * s, mx + 18 * s, my + 18 * s], fill=color + (255,))
    a = moon.split()[3]
    cxo, cyo = P(80, 47)
    ImageDraw.Draw(a).ellipse([cxo - 15.5 * s, cyo - 15.5 * s, cxo + 15.5 * s, cyo + 15.5 * s], fill=0)
    moon.putalpha(a)
    layer = Image.alpha_composite(layer, moon)

    sx, sy = P(94, 40)
    ss = 6.5 * s
    b = 2.2 * s
    ImageDraw.Draw(layer).polygon(
        [(sx, sy - ss), (sx + b, sy - b), (sx + ss, sy), (sx + b, sy + b),
         (sx, sy + ss), (sx - b, sy + b), (sx - ss, sy), (sx - b, sy - b)],
        fill=color + (255,),
    )
    img.alpha_composite(layer)


def render(w: int, h: int, mark_center, mark_size, tile_right=None) -> Image.Image:
    W, H = w * SS, h * SS
    base = gradient(W, H).convert("RGBA")
    if tile_right is not None:                 # keep a clean light band for overlaid text
        band = Image.new("RGBA", (W - int(tile_right * SS), H), CREAM + (255,))
        base.alpha_composite(band, (int(tile_right * SS), 0))
    cx, cy = mark_center
    draw_mark(base, cx * SS, cy * SS, mark_size * SS, LAV)
    return base.resize((w, h), Image.LANCZOS).convert("RGB")


def main() -> None:
    # Banner: mark on the right; WiX draws the page title over the left.
    render(493, 58, mark_center=(462, 29), mark_size=40).save(HERE / "banner.bmp")
    # Dialog: art on the left ~164px; right stays a light cream band for text.
    render(493, 312, mark_center=(82, 116), mark_size=118, tile_right=164).save(HERE / "dialog.bmp")
    print("wrote banner.bmp (493x58), dialog.bmp (493x312)")


if __name__ == "__main__":
    main()
