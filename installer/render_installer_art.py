#!/usr/bin/env python3
"""Render the branded WiX installer bitmaps for the Lullaby Windows installer.

WixUI dialogs use two fixed-size 24-bit BMPs:
    banner.bmp   493x58   top strip on most wizard pages
    dialog.bmp   493x312  full background on the Welcome and Exit pages

Both carry the Lullaby `l` monogram (a rounded vertical stroke with a small foot)
on a soft pastel ground, matching the visual identity. The full `lullaby` wordmark
is the identity everywhere a lockup fits; these fixed, compact bitmap slots use the
`l` monogram, drawn with shapes only (no font dependency), supersampled and
downsampled for clean edges. Run:

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


# Canonical "l" monogram geometry (matches assets/brand/lullaby-icon.svg:
# M51 32 V68 Q51 84 68 84), expressed in the shared 120-unit design space.
MONO_TOP = (51.0, 32.0)
MONO_KNEE = (51.0, 68.0)
MONO_CTRL = (51.0, 84.0)
MONO_END = (68.0, 84.0)
MONO_W = 14.0


def quad_bezier(p0, p1, p2, steps: int = 24):
    """Sample a quadratic Bezier curve into a polyline (design-space points)."""
    pts = []
    for i in range(steps + 1):
        t = i / steps
        u = 1 - t
        x = u * u * p0[0] + 2 * u * t * p1[0] + t * t * p2[0]
        y = u * u * p0[1] + 2 * u * t * p1[1] + t * t * p2[1]
        pts.append((x, y))
    return pts


def draw_mark(img: Image.Image, cx: float, cy: float, size: float, color) -> None:
    """Draw the plain `l` monogram centred at (cx, cy), `size` px tall."""
    s = size / 120.0
    layer = Image.new("RGBA", img.size, (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)

    def P(x, y):
        return (cx + (x - 60) * s, cy + (y - 58) * s)

    lw = max(1, round(MONO_W * s))
    design_pts = [MONO_TOP, MONO_KNEE] + quad_bezier(MONO_KNEE, MONO_CTRL, MONO_END)
    pts = [P(x, y) for (x, y) in design_pts]
    d.line(pts, fill=color + (255,), width=lw, joint="curve")
    r = lw / 2
    for (px, py) in (pts[0], pts[-1]):          # round the two open ends
        d.ellipse([px - r, py - r, px + r, py + r], fill=color + (255,))

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
