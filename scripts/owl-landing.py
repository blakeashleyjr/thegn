#!/usr/bin/env python3
"""thegn owl-landing — a native terminal animation of the owl candidate.

An owl swoops in from the left, brakes, flares, and lands on a branch,
then sits — perfectly still apart from the odd blink. That stillness is
the point: like the thegn event loop, the owl is 0% idle and only moves
when something happens.

Same half-block truecolor technique as scripts/mascot-gallery.py: each
terminal cell is a `▀` whose fg is the upper pixel and bg the lower, so
a 64x36 pixel scene needs 64 cols x 18 rows. The scene is just moon,
branch, and owl on a transparent field (the terminal's own background),
with hand-drawn pose sprites blitted along a flight path.

Usage:
    scripts/owl-landing.py             # play (Ctrl+C to leave)
    scripts/owl-landing.py --once      # land, sit a moment, exit
    scripts/owl-landing.py --fps 15    # playback speed (default 12)
    scripts/owl-landing.py --frame 9   # print one frame and exit
    scripts/owl-landing.py --ascii 9   # frame 9 as raw palette chars
    scripts/owl-landing.py --poses     # print each pose sprite
"""

import sys
import time

W, H = 64, 36  # pixel canvas: 64 cols x 18 cell rows

PAL = {
    'm': (224, 230, 248),  # moon, lit
    'k': (164, 174, 204),  # moon, shaded limb
    'W': (134, 100, 64),   # branch, lit top
    'w': (94, 68, 44),     # branch, mid
    'v': (56, 40, 28),     # branch, underside
    'o': (36, 32, 44),     # owl outline (dark plum, not black)
    'p': (110, 90, 68),    # plumage mid
    'q': (148, 122, 90),   # plumage light
    'r': (72, 58, 44),     # plumage shadow
    'u': (206, 188, 152),  # cream chest
    't': (134, 112, 82),   # chest barring
    'l': (190, 200, 228),  # moonlit rim on wing edges
    'e': (242, 158, 34),   # eye: hot amber
    'E': (255, 214, 92),   # eye highlight
    'f': (20, 16, 12),     # pupil / claw tips
    'h': (228, 190, 62),   # beak: bright yellow
    'y': (96, 74, 46),     # talons
}

# ── Owl poses ───────────────────────────────────────────────────────────────
# '.' transparent. Each pose has an anchor (ax, ay): the pixel inside the
# grid that sits on the flight-path point (the feet, roughly), so poses
# swap without the owl jumping around.

# Flying right, wings on the upstroke, feet trailing under the tail.
FLY_UP = ([
    "......ol........................",
    ".....oqlo.......................",
    "....oqqlo.......................",
    "....oqqqlo......................",
    "...oqqqqlo......................",
    "...oqqqqo.......................",
    "..oqqqqqo.......................",
    "..oqqqqqoo......................",
    ".oqqqqqqqoooooooooo.............",
    ".oopppppppppppppppppoooooo......",
    "..orpppppppppppppppppppppoehoo..",
    "...orrpppppppppuuuuupppppoo.....",
    ".....oorpppppuuuuuuuoo..........",
    ".......ooqqooooyyfoo............",
    ".........oooo...................",
], (15, 13))

# Flying right, wings on the downstroke.
FLY_DN = ([
    "....ollllllllloo................",
    "..oopppppppppppppooooo..........",
    ".oqpppppppppppppppppppooo.......",
    ".orpppppppppppppppppppppoehoo...",
    "..orrpppppppppuuuuupppppoo......",
    "....oorpppppuuuuuuuoo...........",
    "......oorqqqoooyyfoo............",
    ".........oqqqqlo................",
    "..........oqqqlo................",
    "...........oqqqlo...............",
    "............oqqllo..............",
    ".............oqllo..............",
    "..............ollo..............",
    "...............oo...............",
], (15, 6))

# Braking: body rocking upright, both wings thrown up, legs coming down.
BRAKE = ([
    "....olo........olo........",
    "...oqllo......oqllo.......",
    "...oqqlo......oqqlo.......",
    "..oqqqlo......oqqqlo......",
    "..oqqqo........oqqqo......",
    "...oqqqoo....ooqqqo.......",
    "....oqqqooooooqqqo........",
    ".....oppppppppppoo........",
    "....oopoeEffoqppo.........",
    "....opooeehhooppo.........",
    "...orppuuuhhuuppo.........",
    "...orppuuuuuuuupo.........",
    "....orpuutuutuoo..........",
    ".....orruuuuroo...........",
    "......orrppro.............",
    ".....oyyo..oyyo...........",
    "....oyyfo..oyfo...........",
], (9, 16))

# Full flare: wings high and wide, tail fanned, talons reaching forward.
FLARE = ([
    ".ol..............................lo.",
    ".olo............................olo.",
    "..oqlo........................oqlo..",
    "..oqqlo......................oqqlo..",
    "...oqqlo....................oqqlo...",
    "...oqqqoo..................ooqqqo...",
    "....oqqqqoo..............ooqqqqo....",
    ".....ooqqqqoo..........ooqqqqoo.....",
    ".......ooqqqqooooooooooqqqqoo.......",
    ".........oqppppppppppppppqo.........",
    "........oqooooooqqqqooooooqo........",
    "........oqoeEffoqqqqoffEeoqo........",
    "........oqoeeeeoqhhqoeeeeoqo........",
    "........oqoooqqquhhuqqqoooqo........",
    ".........orpuuuuuhhuuuupro..........",
    "..........orpuuuuuuuuuupo...........",
    "...........orruuuuuuuurro...........",
    "............oyyoo..ooyyo............",
    "...........oyyfo....oyyfo...........",
    "..........oyfo........oyfo..........",
], (17, 19))

# Touchdown: feet on the branch, wings still half open for balance.
TOUCH = ([
    ".ol..................lo..",
    ".olqo..............oqlo..",
    "..oqqlo............oqqlo.",
    "...oqqqoqqqqqqqqqqoqqqo..",
    "....oqqqqqqqqqqqqqqqqo...",
    "...oqooooooqqqqooooooqo..",
    "...oqoeEffoqqqqoffEeoqo..",
    "...oqoeeeeoqhhqoeeeeoqo..",
    "...oqoooqqquhhuqqqoooqo..",
    "....oqpqquuuhhuuuqqpqo...",
    "....orpuuttuttuttuupo....",
    "....orputtuuttuuttupo....",
    ".....orpuuttuuttuupo.....",
    ".....orruuttuuttuuro.....",
    "......orruutuuturro......",
    ".......orrpppppro........",
    "........yyy...yyy........",
], (12, 16))

# Settling: the just-landed crouch — the perched bird compressed one row
# before it draws itself up to full height.
SETTLE = ([
    "olo..............olo",
    "oqlo............oqlo",
    ".oqqo..........oqqo.",
    ".oqqqqqqqqqqqqqqqqo.",
    "oqqqqqqqqqqqqqqqqqqo",
    "oqooooooqqqqooooooqo",
    "oqoeEffoqqqqoffEeoqo",
    "oqoeeeeoqhhqoeeeeoqo",
    "oqoooqqquhhuqqqoooqo",
    "oqpqqquuuhhuuuqqqpqo",
    "oppuuuutuuuutuuuuppo",
    "oppuututtuuttutuuppo",
    "oppuuttuuttuuttuuppo",
    "oppuutttuuuutttuuppo",
    "orpuuttuttuttuutupro",
    "orpuuuttuuuuttuuupro",
    ".orpuuutuuuutuuupro.",
    "..orrpppppppppprro..",
    "....yyy......yyy....",
    "...fof......fof.....",
], (9, 18))

# Perched sentinel: horn tufts, heavy scowling brow, hot convergent
# glare, hooked beak — utterly still, and not to be trifled with.
# Square-shouldered and narrow: flat crown, straight flanks, taller
# than it is wide.
PERCHED = ([
    "olo..............olo",
    "oqlo............oqlo",
    ".oqqo..........oqqo.",
    ".oqqqqqqqqqqqqqqqqo.",
    "oqqqqqqqqqqqqqqqqqqo",
    "oqooooooqqqqooooooqo",
    "oqoeEffoqqqqoffEeoqo",
    "oqoeeeeoqhhqoeeeeoqo",
    "oqoooqqquhhuqqqoooqo",
    "oqpqqquuuhhuuuqqqpqo",
    "oppuuuutuuuutuuuuppo",
    "oppuututtuuttutuuppo",
    "oppuuttuuttuuttuuppo",
    "oppuutttuuuutttuuppo",
    "orpuuttuttuttuutupro",
    "orpuuuttuuuuttuuupro",
    ".orpuuutuuuutuuupro.",
    ".orrpuuuuuuuuuuprro.",
    "..orrpppppppppprro..",
    "....yyy......yyy....",
    "...fof......fof.....",
], (9, 19))

# Blink: the brow stays; the eyes narrow to amber slits.
BLINK = ([
    "olo..............olo",
    "oqlo............oqlo",
    ".oqqo..........oqqo.",
    ".oqqqqqqqqqqqqqqqqo.",
    "oqqqqqqqqqqqqqqqqqqo",
    "oqooooooqqqqooooooqo",
    "oqooooooqqqqooooooqo",
    "oqoeeeeoqhhqoeeeeoqo",
    "oqoooqqquhhuqqqoooqo",
    "oqpqqquuuhhuuuqqqpqo",
    "oppuuuutuuuutuuuuppo",
    "oppuututtuuttutuuppo",
    "oppuuttuuttuuttuuppo",
    "oppuutttuuuutttuuppo",
    "orpuuttuttuttuutupro",
    "orpuuuttuuuuttuuupro",
    ".orpuuutuuuutuuupro.",
    ".orrpuuuuuuuuuuprro.",
    "..orrpppppppppprro..",
    "....yyy......yyy....",
    "...fof......fof.....",
], (9, 19))

POSES = {
    "fly_up": FLY_UP, "fly_dn": FLY_DN, "brake": BRAKE, "flare": FLARE,
    "touch": TOUCH, "settle": SETTLE, "perched": PERCHED, "blink": BLINK,
}

# ── Scene ───────────────────────────────────────────────────────────────────

MOON = [  # waxing crescent, open to the right
    "....mmm...",
    "..mmmmm...",
    ".mmmmm....",
    ".mmmm.....",
    "mmmm......",
    "mmmm......",
    ".mmmm.....",
    ".mmmmmk...",
    "..mmmmmk..",
    "....mmmk..",
]
MOON_AT = (9, 3)

BRANCH = [  # enters from the right edge, tapers to the landing tip
    "...........................W",
    ".............WWWWWWWWWWWWwww",
    ".WWWWWWWWWWWWwwwwwwwwwwwvvvv",
    "WWwwwwwwwwwwwwwvvvvvvvvvvvvv",
    ".vvv....vvv...vvvvvvvvvvvvvv",
    "..........................vv",
]
BRANCH_AT = (W - 28, 25)
FEET = (42, 27)  # where the owl's feet land: the branch tip's top edge


def blit(grid, sprite, x, y):
    """Copy sprite rows onto grid at (x, y), skipping transparent pixels."""
    for dy, row in enumerate(sprite):
        gy = y + dy
        if not 0 <= gy < H:
            continue
        line = grid[gy]
        for dx, ch in enumerate(row):
            gx = x + dx
            if ch != '.' and 0 <= gx < W:
                line[gx] = ch
        grid[gy] = line


def compose(pose_name, px, py, tick):
    """Build one frame on a transparent field: moon, branch, owl."""
    del tick  # scene is static apart from the owl
    grid = [['.'] * W for _ in range(H)]
    blit(grid, MOON, *MOON_AT)
    blit(grid, BRANCH, *BRANCH_AT)
    sprite, (ax, ay) = POSES[pose_name]
    blit(grid, sprite, px - ax, py - ay)
    return grid


# ── Timeline ────────────────────────────────────────────────────────────────
# The swoop: enter high on the left, dip below the perch, rise and flare
# up onto the tip (ease-out spacing is baked into the hand-placed points).

FX, FY = FEET
LANDING = [
    ("fly_dn", -26, 15), ("fly_dn", -21, 16), ("fly_up", -16, 17),
    ("fly_up", -11, 19), ("fly_dn", -6, 20), ("fly_dn", -1, 22),
    ("fly_up", 4, 24), ("fly_up", 9, 25), ("fly_dn", 13, 27),
    ("fly_dn", 17, 28), ("fly_up", 21, 29), ("fly_dn", 25, 30),
    ("brake", 28, 30), ("brake", 31, 29), ("flare", 35, 28),
    ("flare", 39, 27), ("flare", FX, FY), ("touch", FX, FY),
    ("touch", FX, FY), ("settle", FX, FY), ("settle", FX, FY),
]

# Idle: stillness, a single blink, stillness, a double blink. Loops.
IDLE = (
    [("perched", FX, FY)] * 42 + [("blink", FX, FY)] * 2
    + [("perched", FX, FY)] * 30 + [("blink", FX, FY)] * 2
    + [("perched", FX, FY)] * 3 + [("blink", FX, FY)] * 2
)


def frame_at(i):
    if i < len(LANDING):
        return LANDING[i]
    return IDLE[(i - len(LANDING)) % len(IDLE)]


# ── Rendering ───────────────────────────────────────────────────────────────

def render(grid):
    """Half-block mosaic with transparency: '.' shows the terminal bg."""
    out = []
    for top, bot in zip(grid[0::2], grid[1::2]):
        cells = []
        last = None
        for t, b in zip(top, bot):
            tc, bc = PAL.get(t), PAL.get(b)
            if tc and bc:
                cell = ("\x1b[38;2;%d;%d;%dm\x1b[48;2;%d;%d;%dm"
                        % (*tc, *bc), "▀")
            elif tc:
                cell = ("\x1b[0m\x1b[38;2;%d;%d;%dm" % tc, "▀")
            elif bc:
                cell = ("\x1b[0m\x1b[38;2;%d;%d;%dm" % bc, "▄")
            else:
                cell = ("\x1b[0m", " ")
            if cell[0] != last:  # only emit codes on change
                cells.append(cell[0])
                last = cell[0]
            cells.append(cell[1])
        out.append("".join(cells) + "\x1b[0m\x1b[K")
    return "\n".join(out)


def animate(fps, once):
    delay = 1.0 / fps
    # ~4s of idle after landing in --once mode
    stop = len(LANDING) + 4 * fps if once else None
    sys.stdout.write("\x1b[?1049h\x1b[?25l")  # alt screen, hide cursor
    try:
        i = 0
        while stop is None or i < stop:
            pose, x, y = frame_at(i)
            sys.stdout.write("\x1b[H" + render(compose(pose, x, y, i)))
            sys.stdout.flush()
            time.sleep(delay)
            i += 1
    except KeyboardInterrupt:
        pass
    finally:
        sys.stdout.write("\x1b[0m\x1b[?25h\x1b[?1049l")  # restore
        sys.stdout.flush()


def validate():
    for name, (sprite, (ax, ay)) in POSES.items():
        for i, row in enumerate(sprite):
            assert set(row) <= set(PAL) | {'.'}, f"{name} row {i}: bad char"
        assert 0 <= ay < len(sprite), name
        assert 0 <= ax < max(len(r) for r in sprite), name


def main(argv):
    validate()
    fps = 12
    if "--fps" in argv:
        fps = int(argv[argv.index("--fps") + 1])
    if "--poses" in argv:
        for name, (sprite, anchor) in POSES.items():
            print(f"{name} anchor={anchor}")
            pad = [r.ljust(W, '.') for r in sprite]
            if len(pad) % 2:
                pad.append('.' * W)
            print(render([list(r) for r in pad]))
        return
    if "--ascii" in argv:
        pose, x, y = frame_at(int(argv[argv.index("--ascii") + 1]))
        for row in compose(pose, x, y, 0):
            print("".join(row))
        return
    if "--frame" in argv:
        pose, x, y = frame_at(int(argv[argv.index("--frame") + 1]))
        print(render(compose(pose, x, y, 0)))
        return
    animate(fps, "--once" in argv)


if __name__ == "__main__":
    main(sys.argv[1:])
