#!/usr/bin/env python3
"""Renders the Go-Notes application icons.

The icons are checked in, because the build must not need a network or an
image toolchain — the same reason nothing else here is fetched at build time.
This script is how they are regenerated, and it is the only description of the
design: run it and the PNGs and the SVG favicon beside it are rewritten.

Deliberately standard library only (`zlib`, `struct`), so it runs with any
Python 3 and adds no dependency to a project that is otherwise Rust and one npm
bundle.

    python3 crates/ui/render-icons.py

It lives beside the icons rather than among them because Trunk copies the
whole `icons/` directory into the bundle, and a build script has no business
being served to a browser.

The glyph is a page with a folded corner over the accent colour: recognisable
at 32px in a browser tab, and still a page rather than a smudge when Android
masks it into a circle.
"""

import os
import struct
import zlib

# --- the design, in unit coordinates ---------------------------------------

ACCENT_TOP = (0x8B, 0x7A, 0xF5)
ACCENT_BOTTOM = (0x5A, 0x48, 0xD6)
PAGE = (0xFF, 0xFF, 0xFF)
FOLD = (0xD8, 0xD2, 0xFA)
RULE = (0x6E, 0x5A, 0xE8)

# Corner rounding for the plain icons. Maskable icons are square to the edge:
# the platform applies its own mask, and rounding twice leaves a pale rim.
CORNER = 0.22

# The page, and how much of the top-right corner is folded away.
PAGE_BOX = (0.30, 0.20, 0.70, 0.80)
FOLD_SIZE = 0.16

# Ruled lines: (y centre, x start, x end).
RULES = [(0.40, 0.375, 0.625), (0.52, 0.375, 0.625), (0.64, 0.375, 0.555)]
RULE_THICKNESS = 0.05

SUPERSAMPLE = 3


def lerp(a, b, t):
    return tuple(round(x + (y - x) * t) for x, y in zip(a, b))


def in_rounded_square(x, y, radius):
    if radius <= 0:
        return True
    # Distance to the nearest corner circle centre, when in a corner region.
    cx = radius if x < radius else (1 - radius if x > 1 - radius else x)
    cy = radius if y < radius else (1 - radius if y > 1 - radius else y)
    return (x - cx) ** 2 + (y - cy) ** 2 <= radius**2


def in_page(x, y):
    x0, y0, x1, y1 = PAGE_BOX
    if not (x0 <= x <= x1 and y0 <= y <= y1):
        return False
    # The folded corner is cut off the top right along a diagonal.
    return (x - (x1 - FOLD_SIZE)) + ((y0 + FOLD_SIZE) - y) <= FOLD_SIZE


def in_fold(x, y):
    x0, y0, x1, y1 = PAGE_BOX
    if not (x1 - FOLD_SIZE <= x <= x1 and y0 <= y <= y0 + FOLD_SIZE):
        return False
    return (x - (x1 - FOLD_SIZE)) + ((y0 + FOLD_SIZE) - y) >= FOLD_SIZE


def in_rule(x, y):
    half = RULE_THICKNESS / 2
    for cy, start, end in RULES:
        if abs(y - cy) > half:
            continue
        # Rounded ends, so the lines do not look stamped out with a punch.
        if start <= x <= end:
            return True
        for cap in (start, end):
            if (x - cap) ** 2 + (y - cy) ** 2 <= half**2:
                return True
    return False


def sample(x, y, scale, offset, rounded):
    """Colour at a unit-square point, or None for transparent."""
    if not in_rounded_square(x, y, CORNER if rounded else 0.0):
        return None

    background = lerp(ACCENT_TOP, ACCENT_BOTTOM, y)

    # The glyph is drawn in its own space so a maskable icon can shrink it into
    # the safe zone without redrawing anything.
    gx = (x - offset) / scale
    gy = (y - offset) / scale
    if not (0 <= gx <= 1 and 0 <= gy <= 1):
        return background

    if in_rule(gx, gy):
        return RULE
    if in_fold(gx, gy):
        return FOLD
    if in_page(gx, gy):
        return PAGE
    return background


def render(size, rounded=True, glyph_scale=1.0):
    offset = (1 - glyph_scale) / 2
    rows = []
    step = 1.0 / (size * SUPERSAMPLE)

    for py in range(size):
        row = bytearray()
        for px in range(size):
            r = g = b = a = 0
            for sy in range(SUPERSAMPLE):
                for sx in range(SUPERSAMPLE):
                    x = (px * SUPERSAMPLE + sx + 0.5) * step
                    y = (py * SUPERSAMPLE + sy + 0.5) * step
                    colour = sample(x, y, glyph_scale, offset, rounded)
                    if colour is None:
                        continue
                    r += colour[0]
                    g += colour[1]
                    b += colour[2]
                    a += 255
            samples = SUPERSAMPLE * SUPERSAMPLE
            if a == 0:
                row += bytes(4)
                continue
            covered = a // 255
            row += bytes(
                (r // covered, g // covered, b // covered, a // samples)
            )
        rows.append(bytes(row))
    return rows


def write_png(path, rows):
    def chunk(tag, payload):
        return (
            struct.pack(">I", len(payload))
            + tag
            + payload
            + struct.pack(">I", zlib.crc32(tag + payload) & 0xFFFFFFFF)
        )

    size = len(rows)
    # Filter type 0 (none) in front of every scanline; the images are small and
    # flat enough that a cleverer filter would not pay for the code.
    raw = b"".join(b"\x00" + row for row in rows)
    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )
    with open(path, "wb") as handle:
        handle.write(png)


SVG = """<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100" role="img" aria-label="Go-Notes">
  <defs>
    <linearGradient id="g" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#8b7af5" />
      <stop offset="1" stop-color="#5a48d6" />
    </linearGradient>
  </defs>
  <rect width="100" height="100" rx="22" fill="url(#g)" />
  <path d="M30 20 H54 L70 36 V80 H30 Z" fill="#ffffff" />
  <path d="M54 20 L70 36 H54 Z" fill="#d8d2fa" />
  <g stroke="#6e5ae8" stroke-width="5" stroke-linecap="round">
    <path d="M37.5 40 H62.5" />
    <path d="M37.5 52 H62.5" />
    <path d="M37.5 64 H55.5" />
  </g>
</svg>
"""


def main():
    here = os.path.join(os.path.dirname(os.path.abspath(__file__)), "icons")
    os.makedirs(here, exist_ok=True)

    # Plain icons round their own corners; the maskable one fills the square and
    # keeps the glyph inside Android's 80% safe zone.
    for size in (192, 512):
        write_png(os.path.join(here, f"icon-{size}.png"), render(size))
    write_png(
        os.path.join(here, "icon-maskable-512.png"),
        render(512, rounded=False, glyph_scale=0.72),
    )
    # iOS masks the icon itself, so this one is square to the edge too.
    write_png(
        os.path.join(here, "apple-touch-icon-180.png"), render(180, rounded=False)
    )
    write_png(os.path.join(here, "favicon-32.png"), render(32))

    with open(os.path.join(here, "icon.svg"), "w") as handle:
        handle.write(SVG)


if __name__ == "__main__":
    main()
