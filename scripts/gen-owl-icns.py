#!/usr/bin/env python3
"""Render the in-app owl mascot sprite (crates/thegn-host/src/owl.rs) as a macOS
`.icns` app icon — the Darwin counterpart to `gen-owl-icon.py`'s SVG.

Pure stdlib (zlib + struct): no rasterizer, no `iconutil`, so it also runs on
Linux/CI. The sprite is axis-aligned 10x10 blocks on a 256-unit canvas, so every
size is rendered directly from the pixel data rather than by scaling a bitmap —
small sizes stay crisp. Only the rounded-rect corners are anti-aliased.

Keep SPRITE + PALETTE in sync with owl.rs (same hand-sync contract as
gen-owl-icon.py). Output: packaging/macos/thegn.icns (committed, so `install.sh`
needs no Python on the user's Mac).
"""

import os
import struct
import zlib

# 20x22 pixel sprite, copied verbatim from owl.rs SPRITE.
SPRITE = [
    "olo..............olo",  # horn tufts
    "oqlo............oqlo",
    ".oqqo..........oqqo.",
    ".oqqqqqqqqqqqqqqqqo.",  # flat crown
    "oqqqqqqqqqqqqqqqqqqo",
    "oqooooooqqqqooooooqo",  # scowl brow band
    "oqoeEffoqqqqoffEeoqo",  # eyes
    "oqoeeeeoqhhqoeeeeoqo",  # lower eye ring, beak
    "oqoooqqquhhuqqqoooqo",  # facial disc edge
    "oqpqqquuuhhuuuqqqpqo",  # beak tip
    "oppuuuutuuuutuuuuppo",  # chest, chevron barring
    "oppuututtuuttutuuppo",
    "oppuuttuuttuuttuuppo",
    "oppuutttuuuutttuuppo",
    "orpuuttuttuttuutupro",
    "orpuuuttuuuuttuuupro",
    ".orpuuutuuuutuuupro.",  # taper
    ".orrpuuuuuuuuuuprro.",
    "..orrpppppppppprro..",
    "....yyy......yyy....",  # talons
    "....fof......fof....",  # claw tips
    "....................",  # pad row
]

# Prism palette (the default preset), copied from owl.rs palette(PresetId::Prism).
PALETTE = {
    "o": (36, 32, 44),
    "p": (110, 90, 68),
    "q": (148, 122, 90),
    "r": (72, 58, 44),
    "u": (206, 188, 152),
    "t": (134, 112, 82),
    "l": (190, 200, 228),
    "e": (242, 158, 34),
    "E": (255, 214, 92),
    "f": (20, 16, 12),
    "h": (228, 190, 62),
    "y": (96, 74, 46),
}

BG = (20, 22, 31)  # #14161f — the SVG's plate
EDGE = (42, 47, 69)  # #2a2f45 — hairline plate border

CANVAS = 256.0  # design-space units (matches config/thegn.svg)
PX = 10.0  # design units per sprite pixel
PLATE_INSET = 8.0  # plate rect: x=y=8, 240x240, rx=48
PLATE_SIZE = 240.0
PLATE_RADIUS = 48.0

# icns element types keyed by rendered pixel size. `ic10` is the 1024 "512@2x"
# slot; the retina duplicates (ic11/ic12/ic13/ic14) share a bitmap with their
# non-retina same-size sibling, which is exactly how iconutil packs an iconset.
ICNS_TYPES = {
    16: ["icp4"],
    32: ["icp5", "ic11"],
    64: ["ic12"],
    128: ["ic07"],
    256: ["ic08", "ic13"],
    512: ["ic09", "ic14"],
    1024: ["ic10"],
}


def plate_coverage(px, py, size):
    """Alpha coverage (0.0-1.0) of the rounded plate at output pixel (px, py).

    Exact for the interior/exterior; 4x4 supersampled inside the corner boxes,
    which is the only place the shape is not axis-aligned.
    """
    scale = size / CANVAS
    x0 = PLATE_INSET * scale
    y0 = x0
    x1 = (PLATE_INSET + PLATE_SIZE) * scale
    y1 = x1
    r = PLATE_RADIUS * scale
    # Centers of the four corner arcs.
    cx = x0 + r if px < x0 + r else (x1 - r if px > x1 - r else None)
    cy = y0 + r if py < y0 + r else (y1 - r if py > y1 - r else None)
    if cx is None or cy is None:
        # Straight edge or interior: coverage is a plain box test with
        # fractional edge coverage.
        ax = max(0.0, min(px + 1.0, x1) - max(px, x0))
        ay = max(0.0, min(py + 1.0, y1) - max(py, y0))
        return max(0.0, min(1.0, ax)) * max(0.0, min(1.0, ay))
    hits = 0
    for sy in range(4):
        for sx in range(4):
            dx = px + (sx + 0.5) / 4.0 - cx
            dy = py + (sy + 0.5) / 4.0 - cy
            if dx * dx + dy * dy <= r * r:
                hits += 1
    return hits / 16.0


def blend(dst, src, alpha):
    return tuple(round(d + (s - d) * alpha) for d, s in zip(dst, src))


def render(size):
    """RGBA bytes for one square icon of `size` px."""
    scale = size / CANVAS
    owl_w = len(SPRITE[0]) * PX
    owl_h = len(SPRITE) * PX
    ox = (CANVAS - owl_w) // 2  # 28
    oy = (CANVAS - owl_h) // 2  # 18

    # Plate first (with its hairline border blended in at >=128px, where a
    # 1-unit stroke is at least half an output pixel and reads as an edge).
    rows = []
    edge_lo = (PLATE_INSET + 0.5) * scale
    edge_hi = (PLATE_INSET + PLATE_SIZE - 0.5) * scale
    draw_edge = size >= 128
    for y in range(size):
        row = bytearray()
        for x in range(size):
            a = plate_coverage(x, y, size)
            if a <= 0.0:
                row += b"\x00\x00\x00\x00"
                continue
            color = BG
            if draw_edge and (
                x < edge_lo or x > edge_hi or y < edge_lo or y > edge_hi
            ):
                color = EDGE
            row += bytes(color) + bytes((round(a * 255),))
        rows.append(row)

    # Sprite pixels: opaque blocks snapped to the output grid, so a sprite pixel
    # never lands half on / half off a device pixel.
    for r, line in enumerate(SPRITE):
        py0 = round((oy + r * PX) * scale)
        py1 = round((oy + (r + 1) * PX) * scale)
        if py1 <= py0:
            py1 = py0 + 1
        for c, ch in enumerate(line):
            if ch == ".":
                continue
            rgb = PALETTE[ch]
            px0 = round((ox + c * PX) * scale)
            px1 = round((ox + (c + 1) * PX) * scale)
            if px1 <= px0:
                px1 = px0 + 1
            block = bytes(rgb) + b"\xff"
            for y in range(max(0, py0), min(size, py1)):
                row = rows[y]
                for x in range(max(0, px0), min(size, px1)):
                    o = x * 4
                    a = row[o + 3] / 255.0
                    if a >= 1.0:
                        row[o : o + 4] = block
                    else:
                        # Overhang past the plate edge (the horn tufts at 16px):
                        # keep the plate's own alpha so the silhouette stays clean.
                        row[o : o + 3] = bytes(blend(row[o : o + 3], rgb, 1.0))
    return rows


def png(size, rows):
    raw = b"".join(b"\x00" + bytes(row) for row in rows)

    def chunk(tag, data):
        body = tag + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def main():
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    out_dir = os.path.join(root, "packaging", "macos")
    os.makedirs(out_dir, exist_ok=True)

    elements = []
    for size in sorted(ICNS_TYPES):
        data = png(size, render(size))
        for tag in ICNS_TYPES[size]:
            elements.append(
                tag.encode("ascii") + struct.pack(">I", len(data) + 8) + data
            )
    body = b"".join(elements)
    icns = b"icns" + struct.pack(">I", len(body) + 8) + body

    out = os.path.join(out_dir, "thegn.icns")
    with open(out, "wb") as f:
        f.write(icns)
    print(f"wrote {out} ({len(icns)} bytes, {len(elements)} elements)")


if __name__ == "__main__":
    main()
