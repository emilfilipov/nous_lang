#!/usr/bin/env python3
"""Render the Lullaby wordmark to a transparent PNG.

The canonical wordmark is [`lullaby-mark.svg`](lullaby-mark.svg), but that SVG
uses a `<text>` element and falls back to a generic sans-serif wherever Nunito is
absent (e.g. a GitHub README). This raster copy bakes the real Nunito ExtraBold
(800) outlines in lavender `#8b6ff0` on a transparent ground, tightly cropped, so
the identity renders faithfully in those contexts. Run:

    python assets/brand/render_wordmark.py

Output: assets/brand/lullaby-wordmark.png
"""
from __future__ import annotations

from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

HERE = Path(__file__).resolve().parent
FONT = HERE / "nunito.woff2"
OUT = HERE / "lullaby-wordmark.png"

WORD = "lullaby"
LAV_INK = (139, 111, 240)  # #8b6ff0
TRACKING = -0.037          # em, matching lullaby-mark.svg letter-spacing -3 @ size 80
SIZE = 320                 # glyph size before crop (high-res for crisp downscale)
PAD = 24                   # transparent padding around the tight ink bbox


def load_font(size: int, weight: int) -> ImageFont.FreeTypeFont:
    font = ImageFont.truetype(str(FONT), size)
    try:
        font.set_variation_by_axes([weight])
    except Exception:
        pass
    return font


def render() -> Image.Image:
    font = load_font(SIZE, 800)
    track_px = SIZE * TRACKING

    scratch = Image.new("RGBA", (10, 10))
    d0 = ImageDraw.Draw(scratch)
    width = sum(d0.textlength(ch, font=font) for ch in WORD) + track_px * (len(WORD) - 1)
    ascent, descent = font.getmetrics()
    canvas_w = int(width + PAD * 2)
    canvas_h = int(ascent + descent + PAD * 2)

    img = Image.new("RGBA", (canvas_w, canvas_h), (0, 0, 0, 0))
    draw = ImageDraw.Draw(img)
    x = float(PAD)
    for ch in WORD:
        draw.text((x, PAD), ch, font=font, fill=LAV_INK + (255,))
        x += draw.textlength(ch, font=font) + track_px

    # Tight-crop to the actual ink, then re-pad evenly.
    bbox = img.getbbox()
    if bbox:
        img = img.crop(bbox)
        padded = Image.new("RGBA", (img.width + PAD * 2, img.height + PAD * 2), (0, 0, 0, 0))
        padded.alpha_composite(img, (PAD, PAD))
        img = padded
    return img


def main() -> None:
    img = render()
    img.save(OUT, "PNG")
    print(f"wrote {OUT} ({img.width}x{img.height})")


if __name__ == "__main__":
    main()
