#!/usr/bin/env python3
"""Draw ``bus.png``, the spacecraft outline the dashboard preset annotates.

Kept in-tree so the asset is reproducible rather than an opaque binary: run
``python3 make_bus_diagram.py`` from this directory to regenerate it. Pure
stdlib (zlib + struct), so it needs no image library.

Shapes only, no text. The labels come from the connectors and the live
widgets the leader lines point at, which is the whole idea — the picture says
where things are, the panel says what they are doing.
"""

import struct
import zlib

W, H = 420, 260
BG = (24, 24, 33)
BODY = (69, 71, 90)
EDGE = (147, 153, 178)
PANEL = (49, 50, 68)
WHEEL = (137, 180, 250)
TORQUER = (250, 179, 135)
SENSOR = (166, 227, 161)

canvas = [[BG for _ in range(W)] for _ in range(H)]


def rect(x0, y0, x1, y1, color, outline=None):
    for y in range(max(0, y0), min(H, y1)):
        for x in range(max(0, x0), min(W, x1)):
            canvas[y][x] = color
    if outline is None:
        return
    for x in range(max(0, x0), min(W, x1)):
        for y in (y0, y1 - 1):
            if 0 <= y < H:
                canvas[y][x] = outline
    for y in range(max(0, y0), min(H, y1)):
        for x in (x0, x1 - 1):
            if 0 <= x < W:
                canvas[y][x] = outline


def disc(cx, cy, r, color):
    for y in range(max(0, cy - r), min(H, cy + r + 1)):
        for x in range(max(0, cx - r), min(W, cx + r + 1)):
            if (x - cx) ** 2 + (y - cy) ** 2 <= r * r:
                canvas[y][x] = color


# Solar panels, then the bus on top of them.
rect(10, 105, 140, 155, PANEL, EDGE)
rect(280, 105, 410, 155, PANEL, EDGE)
for x in range(20, 140, 16):
    rect(x, 105, x + 1, 155, EDGE)
for x in range(290, 410, 16):
    rect(x, 105, x + 1, 155, EDGE)

rect(145, 70, 275, 190, BODY, EDGE)

# Reaction-wheel triad, low in the bus.
for i, cx in enumerate((172, 210, 248)):
    disc(cx, 155, 13, WHEEL)
    disc(cx, 155, 5, BODY)

# Magnetorquer rods along the upper bus wall.
for y in (86, 96, 106):
    rect(158, y, 262, y + 4, TORQUER)

# Sensor head on the +Z face.
rect(196, 52, 224, 70, SENSOR, EDGE)
disc(210, 46, 9, SENSOR)


def png(path):
    raw = b"".join(
        b"\x00" + b"".join(struct.pack("3B", *px) for px in row) for row in canvas
    )

    def chunk(tag, data):
        body = tag + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    with open(path, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n")
        f.write(chunk(b"IHDR", struct.pack(">IIBBBBB", W, H, 8, 2, 0, 0, 0)))
        f.write(chunk(b"IDAT", zlib.compress(raw, 9)))
        f.write(chunk(b"IEND", b""))


if __name__ == "__main__":
    png("bus.png")
    print(f"wrote bus.png ({W}x{H})")
