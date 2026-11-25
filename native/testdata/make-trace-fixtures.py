#!/usr/bin/env python3
"""Regenerates the vector-tracing fixtures in this directory.

    python3 make-trace-fixtures.py [output-directory]

Both images are drawn here from arithmetic only: no third-party pixels, no
photograph and no library beyond the standard library's zlib. They are
deterministic, so a regenerated file is byte-identical to the committed one.

`trace-logo.png` is the near-exact case: five flat colours, hard edges and no
antialiasing, so a faithful trace reproduces it closely. `trace-photo.png` is
the photographic case: smooth gradients, curved boundaries and per-pixel grain,
so it carries thousands of colours and a trace can only approximate it.

This script is test tooling, not part of the app.
"""

import math
import struct
import sys
import zlib
from pathlib import Path

out = Path(sys.argv[1] if len(sys.argv) > 1 else ".")


def png(width, height, rows):
    """Encodes 8-bit RGB rows (each a bytes object of width * 3 bytes)."""
    raw = b"".join(b"\x00" + row for row in rows)

    def chunk(kind, body):
        head = struct.pack(">I", len(body)) + kind
        return head + body + struct.pack(">I", zlib.crc32(head[4:] + body) & 0xFFFFFFFF)

    ihdr = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


# ---- trace-logo.png: five flat colours, hard edges ------------------------

LOGO_W, LOGO_H = 240, 180
WHITE = (255, 255, 255)
RED = (220, 40, 40)
BLUE = (40, 90, 200)
GREEN = (40, 170, 90)
INK = (20, 20, 20)


def logo_pixel(x, y):
    # A disc, a rectangle, a triangle and a bar, all on white. Each shape is
    # tested in isolation so the boundaries stay hard.
    if (x - 60) ** 2 + (y - 60) ** 2 <= 40 ** 2:
        return RED
    if 120 <= x < 220 and 20 <= y < 80:
        return BLUE
    # Triangle (120,100) (220,100) (170,165): below the top edge, inside the
    # two sloping sides.
    if 100 <= y < 165:
        half = (y - 100) * 50 / 65
        if 170 - half <= x <= 170 + half:
            return GREEN
    if 20 <= x < 100 and 150 <= y < 162:
        return INK
    return WHITE


logo_rows = []
for y in range(LOGO_H):
    row = bytearray()
    for x in range(LOGO_W):
        row.extend(logo_pixel(x, y))
    logo_rows.append(bytes(row))


# ---- trace-photo.png: gradients, curved boundaries and grain --------------

PHOTO_W, PHOTO_H = 320, 240
HORIZON = 148


def photo_pixel(x, y, noise):
    # Sky: a vertical gradient that warms towards the horizon.
    t = min(y, HORIZON) / HORIZON
    r = 96 + 120 * t
    g = 142 + 78 * t
    b = 205 + 35 * t
    # Sun and its glow.
    d = math.hypot(x - 236, y - 52)
    if d < 26:
        r, g, b = 255, 238, 196
    elif d < 70:
        glow = (70 - d) / 44 * 0.55
        r, g, b = r + (255 - r) * glow, g + (240 - g) * glow, b + (200 - b) * glow
    # Two hills, the near one darker.
    far = HORIZON - 26 * math.sin((x + 40) / 90.0) - 8 * math.sin(x / 23.0)
    near = HORIZON + 34 - 40 * math.sin((x + 130) / 130.0) - 6 * math.cos(x / 17.0)
    if y >= near:
        shade = 0.72 + 0.16 * (y - near) / (PHOTO_H - near)
        r, g, b = 58 * shade, 112 * shade, 62 * shade
    elif y >= far:
        shade = 0.8 + 0.2 * (y - far) / max(near - far, 1)
        r, g, b = 104 * shade, 146 * shade, 88 * shade
    # Per-pixel grain plus a low-frequency mottle, both deterministic.
    grain = noise
    mottle = 4 * math.sin(x / 11.0) * math.cos(y / 13.0)
    return tuple(
        max(0, min(255, int(round(value + grain + mottle))))
        for value in (r, g, b)
    )


photo_rows = []
state = 0x2545F491
for y in range(PHOTO_H):
    row = bytearray()
    for x in range(PHOTO_W):
        state = (state * 1103515245 + 12345) & 0xFFFFFFFF
        noise = ((state >> 16) % 15) - 7
        row.extend(photo_pixel(x, y, noise))
    photo_rows.append(bytes(row))


out.mkdir(parents=True, exist_ok=True)
(out / "trace-logo.png").write_bytes(png(LOGO_W, LOGO_H, logo_rows))
(out / "trace-photo.png").write_bytes(png(PHOTO_W, PHOTO_H, photo_rows))
print(f"wrote {out / 'trace-logo.png'} and {out / 'trace-photo.png'}")
