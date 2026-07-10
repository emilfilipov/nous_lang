#!/usr/bin/env python3
"""Render the Lullaby social-preview card (Open Graph / Twitter `summary_large_image`).

A 1200x630 PNG carrying the plain `lullaby` wordmark and the tagline on the soft
pastel ground of the visual identity. This is the image social platforms and the
GitHub repository show when the site or repo is shared. Drawn from the canonical
brand palette and the bundled Nunito variable font (pinned to the ExtraBold 800
weight for the wordmark, matching the web/CSS lockup), supersampled for crisp
text. Run:

    python assets/brand/render_social_card.py

Output: assets/brand/lullaby-social-card.png (also copied into the web repo's
`public/og.png` by the site build / by hand).
"""
from __future__ import annotations

from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

HERE = Path(__file__).resolve().parent
FONT = HERE / "nunito.woff2"
OUT = HERE / "lullaby-social-card.png"

W, H = 1200, 630
SS = 2  # supersample factor for crisp glyph edges

# Canonical palette (documents/brand_guidelines.md).
CREAM = (255, 248, 239)
BG_TOP = (253, 249, 243)   # warm cream
BG_BOT = (235, 227, 255)   # light lavender
LAV_INK = (139, 111, 240)  # #8b6ff0 — wordmark
PLUM = (55, 42, 84)        # #372a54 — tagline
DOTS = [
    (196, 181, 253),       # lavender  #c4b5fd
    (254, 202, 202),       # peach     #fecaca
    (255, 220, 166),       # moonglow  #ffdca6
    (186, 230, 253),       # sky       #bae6fd
]

WORDMARK = "lullaby"
TAGLINE = "Serious systems code. Sweet dreams."
TRACKING = -0.037  # em, matching the wordmark SVG's letter-spacing -3 at size 80


def load_font(size: int, weight: int) -> ImageFont.FreeTypeFont:
    font = ImageFont.truetype(str(FONT), size)
    try:
        font.set_variation_by_axes([weight])
    except Exception:
        pass
    return font


def tracked_width(draw: ImageDraw.ImageDraw, text: str, font: ImageFont.FreeTypeFont, track_px: float) -> float:
    total = 0.0
    for i, ch in enumerate(text):
        total += draw.textlength(ch, font=font)
        if i < len(text) - 1:
            total += track_px
    return total


def draw_tracked(draw, x, y, text, font, fill, track_px):
    for ch in text:
        draw.text((x, y), ch, font=font, fill=fill)
        x += draw.textlength(ch, font=font) + track_px


def vertical_gradient(w: int, h: int, top, bottom) -> Image.Image:
    base = Image.new("RGB", (1, h))
    px = base.load()
    for y in range(h):
        t = y / (h - 1)
        px[0, y] = tuple(round(top[i] + (bottom[i] - top[i]) * t) for i in range(3))
    return base.resize((w, h))


def render() -> Image.Image:
    w, h = W * SS, H * SS
    img = vertical_gradient(w, h, BG_TOP, BG_BOT).convert("RGB")
    draw = ImageDraw.Draw(img)

    # Wordmark: size it so the tracked word spans ~62% of the card width.
    target = 0.62 * w
    size = 10
    font = load_font(size, 800)
    while tracked_width(draw, WORDMARK, font, size * TRACKING) < target and size < 20 * SS * 20:
        size += 4 * SS
        font = load_font(size, 800)
    # step back to just under the target for a clean margin
    while size > 8 and tracked_width(draw, WORDMARK, font, size * TRACKING) > target:
        size -= 2
        font = load_font(size, 800)

    track_px = size * TRACKING
    word_w = tracked_width(draw, WORDMARK, font, track_px)
    ascent, descent = font.getmetrics()
    word_h = ascent + descent
    wx = (w - word_w) / 2
    wy = h * 0.40 - word_h / 2
    draw_tracked(draw, wx, wy, WORDMARK, font, LAV_INK, track_px)

    # Tagline: centered under the wordmark, SemiBold plum ink.
    tsize = round(size * 0.235)
    tfont = load_font(tsize, 600)
    tw = draw.textlength(TAGLINE, font=tfont)
    tx = (w - tw) / 2
    ty = wy + word_h + h * 0.045
    draw.text((tx, ty), TAGLINE, font=tfont, fill=PLUM)

    # A quiet brand signature: a centered row of four pastel dots below the tagline.
    r = 9 * SS
    gap = 30 * SS
    row_w = len(DOTS) * (2 * r) + (len(DOTS) - 1) * gap
    dx = (w - row_w) / 2 + r
    dy = ty + tsize * 1.9
    for color in DOTS:
        draw.ellipse([dx - r, dy - r, dx + r, dy + r], fill=color)
        dx += 2 * r + gap

    return img.resize((W, H), Image.LANCZOS)


def main() -> None:
    render().save(OUT, "PNG")
    print(f"wrote {OUT} ({W}x{H})")


if __name__ == "__main__":
    main()
