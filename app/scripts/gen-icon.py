#!/usr/bin/env python3
"""Generate app/assets/kod.icns (and a README hero PNG) from the Kod mark.

Run:  python3 scripts/gen-icon.py        (from the `app/` directory)

The geometry here is the SAME path as assets/logo/kod.svg, in the same 64-unit
space, duplicated ON PURPOSE: the in-app logo is an SVG that gpui rasterizes at
render time, the app icon is a PNG pyramid that `iconutil` demands pre-rendered,
and there is no rasterizer on a stock macOS box to derive one from the other.
Keeping the numbers identical and adjacent is the cheapest way to notice drift.
If you change one, change both.

Depends only on Pillow, which is already present; deliberately no SVG toolchain.
"""

import os
import shutil
import subprocess
import sys
import tempfile

from PIL import Image, ImageDraw

# --- palette (crates/orchestrator-gui/src/theme.rs) --------------------------
APP_BG = (0x0F, 0x12, 0x18, 255)   # ground
MINT = (0x7E, 0xE2, 0xC0, 255)     # ACCENT — the mark
AMBER = (0xE6, 0xC0, 0x7A, 255)    # AMBER — the one lit eye

# --- the mark, in the SVG's 64-unit space ------------------------------------
# ("L", x, y) straight | ("Q", cx, cy, x, y) quadratic. Start point is separate.
START = (15.0, 13.0)
SEGMENTS = [
    ("L", 23.0, 26.0),
    ("Q", 32.0, 23.5, 41.0, 26.0),
    ("L", 49.0, 13.0),
    ("Q", 52.5, 23.0, 52.5, 32.0),
    ("Q", 52.5, 46.0, 42.5, 52.0),
    ("Q", 32.0, 56.5, 21.5, 52.0),
    ("Q", 11.5, 46.0, 11.5, 32.0),
    ("Q", 11.5, 23.0, 15.0, 13.0),
]
# Eye bars are 4.5 units tall, not 3.5: at the 16px icon size a 3.5-unit bar
# landed under one device pixel and vanished in the downsample, which turned the
# whole mark into a mint blob. Height here is legibility at 16px, not taste.
EYE_L = (20.5, 31.0, 28.0, 35.5)
EYE_R = (36.0, 31.0, 43.5, 35.5)


def outline(steps=96):
    """Flatten the path to a polygon. `steps` per curve — high, because this is
    drawn once at 8x and downsampled, so faceting would survive as visible
    chatter on the ear edges."""
    pts = [START]
    cur = START
    for seg in SEGMENTS:
        if seg[0] == "L":
            cur = (seg[1], seg[2])
            pts.append(cur)
        else:
            _, cx, cy, ex, ey = seg
            x0, y0 = cur
            for i in range(1, steps + 1):
                t = i / steps
                u = 1.0 - t
                pts.append((
                    u * u * x0 + 2 * u * t * cx + t * t * ex,
                    u * u * y0 + 2 * u * t * cy + t * t * ey,
                ))
            cur = (ex, ey)
    return pts


def render_mark(px, mark_rgba, supersample=8):
    """The cat mark alone, on transparent, as a square RGBA image `px` on a side.
    Eyes are punched to fully transparent so this composites onto any ground."""
    S = px * supersample
    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)

    # the 64-space mark spans x 11..53, y 10..57; fit it centred with margin.
    span = 47.0
    scale = (S * 0.80) / span
    ox = S / 2 - (32.0 * scale)
    oy = S / 2 - (33.5 * scale)
    def T(p):
        return (ox + p[0] * scale, oy + p[1] * scale)

    d.polygon([T(p) for p in outline()], fill=mark_rgba)
    for (x0, y0, x1, y1) in (EYE_L, EYE_R):
        d.rectangle([T((x0, y0)), T((x1, y1))], fill=(0, 0, 0, 0))
    # refill the right eye amber — the one accent carried over from the ensō.
    d.rectangle([T((EYE_R[0], EYE_R[1])), T((EYE_R[2], EYE_R[3]))], fill=AMBER)

    return img.resize((px, px), Image.LANCZOS)


def render_icon(px):
    """A full macOS app icon: the mark on the dark rounded-square ground, inset
    into the ~80% safe area Apple's grid expects (a full-bleed square icon reads
    as oversized next to every stock app in the Dock)."""
    S = px * 4
    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    inset = S * 0.098
    d.rounded_rectangle(
        [inset, inset, S - inset, S - inset],
        radius=S * 0.180,
        fill=APP_BG,
    )
    img = img.resize((px, px), Image.LANCZOS)
    mark = render_mark(int(px * 0.60), MINT)
    off = (px - mark.width) // 2
    img.alpha_composite(mark, (off, off))
    return img


ICNS_SIZES = [16, 32, 64, 128, 256, 512, 1024]


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    app = os.path.dirname(here)
    assets = os.path.join(app, "assets")
    os.makedirs(assets, exist_ok=True)

    if shutil.which("iconutil") is None:
        sys.exit("iconutil not found — this script only runs on macOS.")

    tmp = tempfile.mkdtemp()
    iconset = os.path.join(tmp, "kod.iconset")
    os.makedirs(iconset)
    # iconutil's required naming: each logical size at 1x and 2x.
    for logical in (16, 32, 128, 256, 512):
        render_icon(logical).save(os.path.join(iconset, f"icon_{logical}x{logical}.png"))
        render_icon(logical * 2).save(os.path.join(iconset, f"icon_{logical}x{logical}@2x.png"))

    out = os.path.join(assets, "kod.icns")
    subprocess.run(["iconutil", "-c", "icns", iconset, "-o", out], check=True)
    shutil.rmtree(tmp)

    # a transparent PNG of the mark for the README header.
    render_mark(512, MINT).save(os.path.join(assets, "kod-mark.png"))

    print(f"wrote {out} ({os.path.getsize(out)} bytes)")
    print(f"wrote {os.path.join(assets, 'kod-mark.png')}")


if __name__ == "__main__":
    main()
