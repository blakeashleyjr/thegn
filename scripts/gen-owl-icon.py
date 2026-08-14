#!/usr/bin/env python3
"""Render the in-app owl mascot sprite (crates/thegn-host/src/owl.rs) as an
SVG app icon. Kept in sync with owl.rs by hand: SPRITE + Prism palette."""

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
    'o': (36, 32, 44),
    'p': (110, 90, 68),
    'q': (148, 122, 90),
    'r': (72, 58, 44),
    'u': (206, 188, 152),
    't': (134, 112, 82),
    'l': (190, 200, 228),
    'e': (242, 158, 34),
    'E': (255, 214, 92),
    'f': (20, 16, 12),
    'h': (228, 190, 62),
    'y': (96, 74, 46),
}

PX = 10           # svg units per sprite pixel
W = len(SPRITE[0])  # 20
H = len(SPRITE)     # 22
CANVAS = 256
owl_w, owl_h = W * PX, H * PX
ox = (CANVAS - owl_w) // 2
oy = (CANVAS - owl_h) // 2

def hexc(rgb):
    return "#%02x%02x%02x" % rgb

rects = []
for r, line in enumerate(SPRITE):
    for c, ch in enumerate(line):
        if ch == '.':
            continue
        x = ox + c * PX
        y = oy + r * PX
        rects.append(
            f'  <rect x="{x}" y="{y}" width="{PX}" height="{PX}" fill="{hexc(PALETTE[ch])}"/>'
        )

svg = f'''<?xml version="1.0" encoding="UTF-8"?>
<!-- thegn owl app icon — generated from crates/thegn-host/src/owl.rs (Prism plumage).
     Regenerate with scripts/gen-owl-icon.py; keep SPRITE/PALETTE in sync with owl.rs. -->
<svg xmlns="http://www.w3.org/2000/svg" width="{CANVAS}" height="{CANVAS}" viewBox="0 0 {CANVAS} {CANVAS}">
  <rect x="8" y="8" width="240" height="240" rx="48" fill="#14161f"/>
  <rect x="8.5" y="8.5" width="239" height="239" rx="47.5" fill="none" stroke="#2a2f45" stroke-width="1"/>
{chr(10).join(rects)}
</svg>
'''

import os
out = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "config", "thegn.svg")
with open(out, "w", encoding="utf-8") as f:
    f.write(svg)
print(f"wrote {out} ({len(rects)} pixels)")
