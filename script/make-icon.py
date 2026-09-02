#!/usr/bin/env python3
"""Generate a placeholder app icon.

The real icon is a design task; this produces something recognisable so the
bundle is not shipped with a generic blank tile. It writes a 1024x1024 PNG with
no dependencies beyond the standard library (`sips` turns it into .icns).

Usage: script/make-icon.py out.png
"""

import struct
import sys
import zlib

SIZE = 1024
BACKGROUND = (13, 15, 19)
ACCENT = (91, 157, 255)
WARM = (63, 185, 80)


def chunk(kind: bytes, data: bytes) -> bytes:
    return (
        struct.pack(">I", len(data))
        + kind
        + data
        + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)
    )


def mix(a, b, t):
    return tuple(round(x + (y - x) * t) for x, y in zip(a, b))


def pixel(x: int, y: int):
    """A dark rounded square with two linked nodes: laptop and phone."""
    cx, cy = SIZE / 2, SIZE / 2
    # Rounded-square mask, macOS-ish corner radius.
    radius = SIZE * 0.22
    inset = SIZE * 0.06
    left, top = inset, inset
    right, bottom = SIZE - inset, SIZE - inset
    nearest_x = min(max(x, left + radius), right - radius)
    nearest_y = min(max(y, top + radius), bottom - radius)
    inside_box = left <= x <= right and top <= y <= bottom
    corner = (x - nearest_x) ** 2 + (y - nearest_y) ** 2 <= radius**2
    if not (inside_box and (corner or (left + radius <= x <= right - radius) or (top + radius <= y <= bottom - radius))):
        return (0, 0, 0, 0)

    base = mix(BACKGROUND, (26, 31, 40), y / SIZE)

    # Two nodes and the link between them.
    node_radius = SIZE * 0.11
    top_node = (cx, cy - SIZE * 0.16)
    bottom_node = (cx, cy + SIZE * 0.16)
    for centre, colour in ((top_node, ACCENT), (bottom_node, WARM)):
        distance = ((x - centre[0]) ** 2 + (y - centre[1]) ** 2) ** 0.5
        if distance <= node_radius:
            return (*colour, 255)
        if distance <= node_radius * 1.28:
            return (*mix(base, colour, 0.35), 255)

    if abs(x - cx) <= SIZE * 0.018 and top_node[1] < y < bottom_node[1]:
        return (*mix(base, ACCENT, 0.55), 255)

    return (*base, 255)


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    rows = bytearray()
    for y in range(SIZE):
        rows.append(0)  # PNG filter: none
        for x in range(SIZE):
            rows.extend(pixel(x, y))
    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", SIZE, SIZE, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(bytes(rows), 9))
        + chunk(b"IEND", b"")
    )
    with open(sys.argv[1], "wb") as handle:
        handle.write(png)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
