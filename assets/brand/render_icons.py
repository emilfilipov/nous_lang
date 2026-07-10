#!/usr/bin/env python3
"""Render the Lullaby brand raster icons from the canonical geometry.

Draws the filled app icon (a plain lowercase "l" monogram in cream on a soft
pastel tile) at high resolution, then downsamples to every size a Windows
app/installer/favicon needs and writes a multi-size ``.ico`` plus PNGs.

Geometry matches ``lullaby-icon.svg`` (a 120-unit design space). Run from anywhere:

    python assets/brand/render_icons.py

Outputs (next to this script):
    lullaby.ico            multi-size 16/24/32/48/64/128/256 (app + installer + favicon)
    lullaby-icon-256.png   256px app icon
    lullaby-icon-512.png   512px app icon
"""
from __future__ import annotations

from pathlib import Path

from PIL import Image, ImageDraw

HERE = Path(__file__).resolve().parent
UNIT = 120           # design space
SS = 1024            # supersample canvas (rendered, then downsampled)
S = SS / UNIT        # scale factor design -> supersample

CREAM = (255, 248, 239, 255)
# tile gradient stops (top-left -> mid -> bottom-right)
G0 = (217, 204, 255)   # #d9ccff
G1 = (196, 181, 253)   # #c4b5fd
G2 = (191, 230, 251)   # #bfe6fb


def lerp(a: tuple[int, int, int], b: tuple[int, int, int], t: float) -> tuple[int, int, int]:
    return tuple(round(a[i] + (b[i] - a[i]) * t) for i in range(3))


def diagonal_gradient(size: int) -> Image.Image:
    """A smooth 3-stop diagonal gradient, built small and upscaled (cheap + smooth)."""
    small = 128
    g = Image.new("RGB", (small, small))
    px = g.load()
    for y in range(small):
        for x in range(small):
            t = (x + y) / (2 * (small - 1))
            px[x, y] = lerp(G0, G1, t * 2) if t < 0.5 else lerp(G1, G2, (t - 0.5) * 2)
    return g.resize((size, size), Image.LANCZOS)


def sc(v: float) -> float:
    return v * S


def quad_bezier(p0, p1, p2, steps: int = 24) -> list[tuple[float, float]]:
    """Sample a quadratic Bezier curve into a polyline (design-space points)."""
    pts = []
    for i in range(steps + 1):
        t = i / steps
        u = 1 - t
        x = u * u * p0[0] + 2 * u * t * p1[0] + t * t * p2[0]
        y = u * u * p0[1] + 2 * u * t * p1[1] + t * t * p2[1]
        pts.append((x, y))
    return pts


# Canonical "l" monogram geometry (matches lullaby-icon.svg: M51 32 V68 Q51 84 68 84).
MONO_TOP = (51.0, 32.0)      # top of the stem (open, rounded end)
MONO_KNEE = (51.0, 68.0)     # where the stem meets the foot curve
MONO_CTRL = (51.0, 84.0)     # quadratic control point
MONO_END = (68.0, 84.0)      # foot tip (open, rounded end)
MONO_W = 14.0                # stroke width in design units (matches the SVG)


def monogram_points() -> list[tuple[float, float]]:
    """The full "l" polyline: vertical stem, then the curved foot."""
    return [MONO_TOP, MONO_KNEE] + quad_bezier(MONO_KNEE, MONO_CTRL, MONO_END)


def render_master() -> Image.Image:
    img = Image.new("RGBA", (SS, SS), (0, 0, 0, 0))

    # --- tile: rounded-rect gradient with a soft inner hairline ---
    tile_mask = Image.new("L", (SS, SS), 0)
    ImageDraw.Draw(tile_mask).rounded_rectangle(
        [sc(4), sc(4), sc(116), sc(116)], radius=sc(28), fill=255
    )
    grad = diagonal_gradient(SS).convert("RGBA")
    img.paste(grad, (0, 0), tile_mask)
    ImageDraw.Draw(img).rounded_rectangle(
        [sc(4.75), sc(4.75), sc(115.25), sc(115.25)],
        radius=sc(27.25), outline=(255, 255, 255, 140), width=max(1, round(sc(1.5))),
    )

    # --- mark layer (cream): the plain lowercase "l" monogram ---
    mark = Image.new("RGBA", (SS, SS), (0, 0, 0, 0))
    d = ImageDraw.Draw(mark)

    lw = round(sc(MONO_W))
    pts = [(sc(x), sc(y)) for (x, y) in monogram_points()]
    d.line(pts, fill=CREAM, width=lw, joint="curve")
    r = lw / 2
    for (cx, cy) in (pts[0], pts[-1]):          # round the two open ends
        d.ellipse([cx - r, cy - r, cx + r, cy + r], fill=CREAM)

    return Image.alpha_composite(img, mark)


def main() -> None:
    master = render_master()
    sizes = [16, 24, 32, 48, 64, 128, 256]
    frames = [master.resize((s, s), Image.LANCZOS) for s in sizes]
    ico_path = HERE / "lullaby.ico"
    frames[-1].save(ico_path, format="ICO", sizes=[(s, s) for s in sizes])
    master.resize((256, 256), Image.LANCZOS).save(HERE / "lullaby-icon-256.png")
    master.resize((512, 512), Image.LANCZOS).save(HERE / "lullaby-icon-512.png")
    print(f"wrote {ico_path.name} ({', '.join(str(s) for s in sizes)}), "
          f"lullaby-icon-256.png, lullaby-icon-512.png")


if __name__ == "__main__":
    main()
