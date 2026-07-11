#!/usr/bin/env python3
"""thegn mascot gallery — persona candidates for the loading-splash sprite.

Renders every candidate as a numbered, named option so you can scroll
through and pick. Each option is a hand-authored 28x20 indexed-palette
pixel sprite (same format as crates/thegn-host/src/mascot.rs) drawn with
the half-block mosaic trick: each terminal cell is a `▀` whose fg is the
upper pixel and bg the lower pixel.

Structure: 11 concepts (including the current knight) x 2 compositions
x 5 palettes = 110 options.

A drill-down series (deep one-off designs for shortlisted concepts, each
with its own palette) lives in scripts/mascot_gallery_drill.py and is
numbered after the base gallery.

Usage:
    scripts/mascot-gallery.py               # render every option
    scripts/mascot-gallery.py owl           # only options whose name matches
    scripts/mascot-gallery.py 37            # render option #37 alone
    scripts/mascot-gallery.py --list        # just the numbered names
    scripts/mascot-gallery.py --drill       # drill-down series only
    scripts/mascot-gallery.py --base        # base 110 only
Pipe through `less -R` to scroll: scripts/mascot-gallery.py | less -R
"""

import sys

PX_W = 28
PX_H = 20

# ── Palettes ────────────────────────────────────────────────────────────────
# Eight roles, same letters as mascot.rs:
#   a outline · b mid body · c light/highlight · d accent (gilt)
#   e bright accent · f near-black · g shadow · h dark accent
THEMES = [
    ("Gilt Iron", {  # the current brand palette
        'a': (56, 58, 76), 'b': (110, 114, 138), 'c': (162, 168, 192),
        'd': (198, 152, 64), 'e': (236, 200, 110), 'f': (20, 20, 28),
        'g': (80, 82, 100), 'h': (146, 104, 44),
    }),
    ("Verdigris", {  # weathered bronze / patina greens
        'a': (40, 56, 50), 'b': (96, 132, 116), 'c': (150, 186, 166),
        'd': (170, 130, 70), 'e': (214, 182, 120), 'f': (14, 22, 18),
        'g': (64, 92, 80), 'h': (120, 95, 55),
    }),
    ("Ember", {  # charcoal + forge fire
        'a': (48, 38, 36), 'b': (98, 82, 78), 'c': (142, 122, 114),
        'd': (216, 110, 48), 'e': (255, 182, 84), 'f': (16, 12, 10),
        'g': (70, 58, 54), 'h': (160, 72, 36),
    }),
    ("Moonlit", {  # slate night + silver
        'a': (44, 50, 70), 'b': (92, 104, 140), 'c': (168, 182, 214),
        'd': (204, 212, 232), 'e': (246, 250, 255), 'f': (12, 14, 24),
        'g': (64, 74, 104), 'h': (142, 152, 182),
    }),
    ("Heather", {  # dusk purple + gold
        'a': (56, 44, 72), 'b': (110, 92, 140), 'c': (164, 146, 192),
        'd': (198, 152, 64), 'e': (236, 200, 110), 'f': (18, 14, 26),
        'g': (80, 66, 104), 'h': (146, 104, 44),
    }),
]

# ── Sprites ─────────────────────────────────────────────────────────────────
# Rows of palette-index chars; '.' is transparent. Rows shorter than PX_W
# are right-padded with '.'; longer rows are a hard error.

KNIGHT_SUTTON_HOO = [  # the current mascot.rs sprite, verbatim
    "............eeee............",
    "...........deeeed...........",
    ".......cccccdhhdccccc.......",
    ".....acccccccccccccccca.....",
    "....abbbbbbbbbbbbbbbbbba....",
    "...abbbbbbbbbbbbbbbbbbbba...",
    "...abbbbbbbbbbbbbbbbbbbba...",
    "...agggggggggggggggggggga...",
    "...adddddddddeeddddddddda...",
    "...abbffffffddddffffffbba...",
    "...abbffffffdhhdffffffbba...",
    "...abbbbggggdhhdggggbbbba...",
    "...abbbbgggddhhddgggbbbba...",
    "...abbdddddddeedddddddbba...",
    "....abbbbddffffffddbbbba....",
    ".....abbbbbggggggbbbbba.....",
    ".......agggggggggggga.......",
    "..........gagagaga..........",
    "....agagagagagagagagagag....",
    "..gagagagagagagagagagagaga..",
]

KNIGHT_SPANGENHELM = [  # conical helm, nasal bar, mail aventail — no face
    ".............ee.............",
    "............deed............",
    "...........cbbbbc...........",
    "..........cbbddbbc..........",
    ".........abbbddbbba.........",
    "........abbbbddbbbba........",
    ".......abbbbbddbbbbba.......",
    "......abbbbbbddbbbbbba......",
    "......abbbbbbddbbbbbba......",
    "......adddddddddddddda......",
    "......abffffbddbffffba......",
    "......abffffbdhbffffba......",
    "......abbbbbbdhbbbbbba......",
    ".......abbbbbdhbbbbba.......",
    ".......agbbbbddbbbbga.......",
    "........aggggddgggga........",
    ".........gagagagaga.........",
    ".......gagagagagagaga.......",
    ".....agagagagagagagagag.....",
    "...gagagagagagagagagagaga...",
]

RAVEN_WATCHER = [  # head in profile, gilt eye, heavy beak
    "............................",
    ".........abbbba.............",
    ".......abbbbbbbba...........",
    "......abbcbbbbbbba..........",
    ".....abbcbbbbbbbbba.........",
    "....abbbbeebbbbbbbba........",
    "...aabbbbeebbbbbbbbba.......",
    ".aahhabbbbbbbbbbbbbbba......",
    "aahhhhabbbbbbbbbbbbbbba.....",
    ".aahhabbbbbbbbbbbbbbbba.....",
    "...aabbbbbbbbbbbbbbbbba.....",
    ".....abbbbbbbbbbbbbbbba.....",
    "......abbbbbbbbbbbbbbbba....",
    ".......abbbbbbbbbbbbbba.....",
    "........abbbbbbbbbbbbba.....",
    ".........abbbbbbbbbbba......",
    "..........abbbbbbbbba.......",
    "...........agbbggbba........",
    "............................",
    "............................",
]

RAVEN_PERCHED = [  # full bird on a branch, folded wing
    "............................",
    "..........abba..............",
    "........abbbbba.............",
    ".......abbeebbba............",
    ".....aahbbeebbbba...........",
    "....ahhhabbbbbbbba..........",
    ".....aahabbbbbbbbba.........",
    "........abbbbbbbbbba........",
    "........abbgbbbbbbbba.......",
    ".......abbggbbbbbbbbba......",
    ".......abggbbbbbbbbbbba.....",
    "......abbggbbbbbbbbbba......",
    "......abgggbbbbbbbbba.......",
    ".....abbggbbbbbbbbba........",
    ".....abbbbbbbbbbbaaa........",
    "....abbbbbbbbbaa............",
    "....abbbbbbba...............",
    ".......ada..ada.............",
    ".....hhhhhhhhhhhhhhhh.......",
    "............................",
]

OWL_SENTINEL = [  # frontal bust: ear tufts, ringed gilt eyes, speckled chest
    "....ab................ba....",
    "...abbb..............bbba...",
    "...abbbbbbbbbbbbbbbbbbbba...",
    "..abbbbbbbbbbbbbbbbbbbbbba..",
    "..abbeeeeeebbbbbbeeeeeebba..",
    "..abeeeeeeeebbbbeeeeeeeeba..",
    "..abeeeffeeebddbeeeffeeeba..",
    "..abeeeeeeeebddbeeeeeeeeba..",
    "..abbeeeeeebbddbbeeeeeebba..",
    "..abbbbbbbbbbddbbbbbbbbbba..",
    "..agbgbgbgbgbggbgbgbgbgbga..",
    "..abgbgbgbgbgbbgbgbgbgbgba..",
    "..abbgbgbgbgbggbgbgbgbgbba..",
    "...abbgbgbgbgbbgbgbgbgba....",
    "....abbbgbgbgbggbgbgbba.....",
    ".....abbbbbbbbbbbbbbba......",
    ".......abbbbbbbbbbba........",
    ".........aabbbbbbaa.........",
    "..........hdh..hdh..........",
    "........hhhhhhhhhhhh........",
]

OWL_NIGHT_WATCH = [  # small owl on a branch, crescent moon, stars
    "....eee.....................",
    "...ee.......................",
    "..ee...............ab..ba...",
    "..ee..............abbbbbba..",
    "..ee..............abeebeeba.",
    "...ee.............abeebeeba.",
    "....eee...........abbddbba..",
    "..................abbbbbba..",
    "............e.....agbgbgba..",
    "..................abgbgbba..",
    "..................agbgbgba..",
    "...................abbbba...",
    ".......e............abba....",
    "...................hdhdh....",
    "..........hhhhhhhhhhhhhhhh..",
    ".........hh.................",
    "........hh..........e.......",
    "....e.......................",
    "............................",
    "............................",
]

HALL_MEAD_HALL = [  # gabled longhouse, glowing door, gilt finial
    "............ee..............",
    "...........dbbd.............",
    "..........dbbbbd............",
    ".........gbbbbbbg...........",
    "........gbbbbbbbbg..........",
    ".......gbbggggggbbg.........",
    "......gbbggggggggbbg........",
    ".....gbbggggggggggbbg.......",
    "....gbbggggggggggggbbg......",
    "...gbbggggggggggggggbbg.....",
    "..gbbggggggggggggggggbbg....",
    "..abbbbbbbbbbbbbbbbbbbba....",
    "...abccbbccbeebccbbccba.....",
    "...abccbbccbeebccbbccba.....",
    "...abccbbccbeebccbbccba.....",
    "...abbbbbbbdeedbbbbbbba.....",
    "...abbbbbbbdeedbbbbbbba.....",
    "...aaaaaaaaaaaaaaaaaaaa.....",
    "..hhhhhhhhhhhhhhhhhhhhhh....",
    "............................",
]

HALL_ON_THE_HILL = [  # night scene: hall silhouette, warm windows, stars
    "..e.......e.........e.......",
    "......e........e.......e....",
    "............................",
    "...e.....abbbbbba......e....",
    "........abbbbbbbba..........",
    ".......abbbbbbbbbba.........",
    "......abbbbbbbbbbbba........",
    ".....abbbbbbbbbbbbbba.......",
    ".....abbbdbbeebbdbbba.......",
    ".....abbbdbbeebbdbbba.......",
    "....ggbbbbbbbbbbbbbbgg......",
    "...gggggggggggggggggggg.....",
    "..gggggggggggggggggggggg....",
    ".gggggggggggggggggggggggg...",
    "gggggggggggggggggggggggggg..",
    "............................",
    "............................",
    "............................",
    "............................",
    "............................",
]

LONGSHIP_UNDER_SAIL = [  # striped sail, shield row, waves
    ".............bee............",
    ".............b..............",
    ".....acdcdcdcbdcdcdca.......",
    ".....acdcdcdcbdcdcdca.......",
    ".....acdcdcdcbdcdcdca.......",
    ".....acdcdcdcbdcdcdca.......",
    ".....acdcdcdcbdcdcdca.......",
    ".....acdcdcdcbdcdcdca.......",
    ".....acdcdcdcbdcdcdca.......",
    "......aaaaaaabaaaaaa........",
    ".............b..............",
    "..a..........b.........a....",
    "..ab.........b........ba....",
    "..abbadadadadadadadabba.....",
    "...abbbbbbbbbbbbbbbbba......",
    "....aggggggggggggggga.......",
    "............................",
    "..g.gg.ggg.gg.g..gg.gg.g....",
    ".gggggggggggggggggggggggg...",
    "............................",
]

LONGSHIP_DRAGON_PROW = [  # carved prow head rising from the waterline
    "..............abbbba........",
    ".............abbbbbba.......",
    "............abbeebbbba......",
    "............abbeebbbbba.....",
    ".............abbbbddba......",
    "..............abbbba........",
    "..............abbba.........",
    ".............abbba..........",
    "............abbba...........",
    "...........abbba............",
    "..........abbba.............",
    ".........abbbba.............",
    "........abbbbba.............",
    ".......abbbbbba.............",
    "......abbbbbbbbaaa..........",
    ".....abbbbbbbbbbbbaaaa......",
    "....abbbbbbbbbbbbbbbbaaa....",
    "..g.gg.ggg.gg.g..gg.gg.g....",
    ".gggggggggggggggggggggggg...",
    "............................",
]

HOUND_HALL_HOUND = [  # seated in profile, gilt collar
    "............................",
    ".....abba...................",
    "....abbbbaa.................",
    "...abbebbbba................",
    "...abbbbbbba................",
    "..aabbbbbbba................",
    ".a..addddbba................",
    "....abbbbbba................",
    "....abbbbbbba...............",
    "....abbbbbbbbaa.............",
    "....abbbbbbbbbbaaa..........",
    "....abbbbbbbbbbbbbaa........",
    "....abbbbbbbbbbbbbbba.......",
    "....abbbbbbbbbbbbbbbba......",
    "....abbbbbbbbbbbbbbbba..a...",
    "....abbabbbabbbbbbbbbaba....",
    "....abba.abba....abbaba.....",
    "....abba.abba....abbba......",
    "....hdda.hdda....hdda.......",
    "..hhhhhhhhhhhhhhhhhhhhhh....",
]

HOUND_PORTRAIT = [  # frontal head, studded collar
    "............................",
    "....ab..............ba......",
    "...abba............abba.....",
    "...abbba..........abbba.....",
    "...abbbbaaaaaaaaaabbbba.....",
    "...abbbbbbbbbbbbbbbbbba.....",
    "....abbbbbbbbbbbbbbbba......",
    "....abbeebbbbbbbbeebba......",
    "....abbeebbbbbbbbeebba......",
    "....abbbbbbccccbbbbbba......",
    ".....abbbbbccccbbbbba.......",
    ".....abbbbccffccbbbba.......",
    "......abbbccffccbbba........",
    "......abbbccccccbbba........",
    ".......abbbccccbbba.........",
    ".......addddddddddda........",
    ".......adeddeddedda.........",
    "............................",
    "............................",
    "............................",
]

FALCON_HOODED = [  # hooded falcon on a gauntlet, plumed hood
    "...........de...............",
    "..........ddd...............",
    ".........addddda............",
    "........adddddda............",
    "........addddddda...........",
    "........abddddba............",
    ".......abbbbbbbba...........",
    ".......abbbbbbbbba..........",
    "......abbcbbbbbbbba.........",
    "......abbcbbbbbbbba.........",
    "......abbcbbbbgbbba.........",
    "......abbcbbbbgbbba.........",
    "......abbcbbbggbba..........",
    ".......abbbbggbba...........",
    ".......abbbbbbba............",
    "........abbbba..............",
    "........hdhdh...............",
    "....hhhhhhhhhhhhhhh.........",
    "...hddddddddddddddh.........",
    "...hhhhhhhhhhhhhhhh.........",
]

FALCON_STOOP = [  # the dive: swept wings, head leading
    "..aa........................",
    "...aba......................",
    "....abba....................",
    ".....abbba..................",
    "..aa..abbbba................",
    "...aba.abbbba...............",
    "....abbabbbbba..............",
    ".....abbbbbbbba.............",
    "......abbbbbbbba............",
    ".......abbbbbbbba...........",
    "........abbbbbbbba..........",
    ".........abbcbbbbba.........",
    "..........abbcbbbbba........",
    "...........abbbbeba.........",
    "............abbbbba.........",
    ".............abbbaa.........",
    "..............aaa...........",
    "................g...........",
    ".................gg.........",
    "............................",
]

SCRIBE_THE_SCOP = [  # hooded scribe, quill, candle, manuscript desk
    "............................",
    ".......abbbba...............",
    "......abbbbbba..............",
    ".....abbbbbbbba.............",
    ".....abffffffba.............",
    ".....abffeeffba..........e..",
    ".....abffffffba..........e..",
    "....abbbbbbbbbba.........c..",
    "....abbbbbbbbbba.......cc...",
    "...abbbbbbbbbbbba.....cc....",
    "...abbbbbbbbbbbba....cc.....",
    "..abbbbbbbbbbbbbba..cc......",
    "..abbbbbbbbbbbbbbaacc.......",
    "..abbbbbbbbbbbbbbadc........",
    "..abbbbbbbbbbbbbbba.........",
    ".hhhhhhhhhhhhhhhhhhhhhhhh...",
    ".hcccccccccccccccccccccch...",
    ".hcgggggcggggggcgggggggch...",
    ".hhhhhhhhhhhhhhhhhhhhhhhh...",
    "............................",
]

SCRIBE_ILLUMINATED = [  # open manuscript, illuminated thorn initial
    "............................",
    "............................",
    "...aaaaaaaaaaaaaaaaaaaaaa...",
    "..acccccccccccaacccccccccca.",
    "..acdccccccccaaccgggggccca..",
    "..acdccccccccaacccccccccca..",
    "..acddddccccaaccggggggcca...",
    "..acdccdccccaacccccccccca...",
    "..acdccdcccaaccgggggccca....",
    "..acddddcccaacccccccccca....",
    "..acdcccccaaccggggggccca....",
    "..acdcccccaacccccccccca.....",
    "..accccccaaccgggggcccca.....",
    "..accccccaacccccccccca......",
    "..aaaaaaaaaaaaaaaaaaaa......",
    "...ccccccca.accccccca.......",
    "....aaaaaa...aaaaaaa........",
    "............................",
    "............................",
    "............................",
]

SMITH_ANVIL = [  # anvil on a block, hammer raised, sparks flying
    ".............e..............",
    "....e.......................",
    "..........e....e............",
    ".......e....ee..............",
    "............ee........e.....",
    "....................dd......",
    "...................dddd.....",
    "....................hh......",
    "...................hh.......",
    "..aaaaaaaaaaaaaaaaaaa.......",
    ".abbbbbbbbbbbbbbbbbbba......",
    "..aabbbbbbbbbbbbbbaa........",
    "....abbbbbbbbbbba...........",
    ".....abbbbbbbbba............",
    "......abbbbbbba.............",
    ".....abbbbbbbbba............",
    "....ahhhhhhhhhhha...........",
    "....ahhhhhhhhhhha...........",
    "...hhhhhhhhhhhhhhh..........",
    "............................",
]

SMITH_AT_FORGE = [  # smith over the forge glow, hammer arm raised
    "............................",
    "..................abba......",
    ".................abbbba.....",
    ".................abbbba.....",
    "..ee..............abbba.....",
    ".eeee.......ahha.abbbba.....",
    "eeeeee.....ahh..aabbbba.....",
    "eeddee....ahh..abbbbbbba....",
    ".eded....ahh..abbbbbbbba....",
    "..dd....ahh..abbbbbbbbba....",
    "..dd.......abbbbbbbbbbba....",
    ".hddh......abbbbbbbbbbba....",
    ".hddh.....abbbbbbbbbbbba....",
    ".hhhh....abbbbbbbbbbbbba....",
    "........abbbbbbbbbbbbbba....",
    "........abbabbbbbbbabba.....",
    "........abba.......abba.....",
    "........abba.......abba.....",
    ".......hhhhhhhhhhhhhhhhh....",
    "............................",
]

RUNESTONE_STANDING = [  # standing stone, carved thorn, mossy base
    "............................",
    ".........abbbbba............",
    "........abbbbbbba...........",
    ".......abbbbbbbbba..........",
    ".......abbcbbbbbba..........",
    "......abbcbbdbbbbba.........",
    "......abbcbbdbbbbba.........",
    "......abbcbbddddbba.........",
    "......abbcbbdbbdbba.........",
    "......abbcbbdbbdbba.........",
    "......abbcbbddddbba.........",
    "......abbcbbdbbbbba.........",
    "......abbcbbdbbbbba.........",
    "......abbbbbbbbbbba.........",
    "......abbbbbbbbbbba.........",
    ".....gabbbbbbbbbbbag........",
    "....ggabbbbbbbbbbbagg.......",
    "...gggggggggggggggggg.......",
    "..hhhhhhhhhhhhhhhhhhhh......",
    "............................",
]

RUNESTONE_SUNBURST = [  # thorn slab with rays behind
    ".....e......e......e........",
    "......e.....e.....e.........",
    ".......e....e....e..........",
    "..ee....e...e...e....ee.....",
    "....eee..aaaaaaa..eee.......",
    ".........abbbbba............",
    "........abbdbbbba...........",
    "........abbdbbbba...........",
    "........abbddddba...........",
    "........abbdbbdba...........",
    "........abbdbbdba...........",
    "........abbddddba...........",
    "........abbdbbbba...........",
    "........abbdbbbba...........",
    "........abbbbbbba...........",
    "........abbbbbbba...........",
    ".......gabbbbbbbag..........",
    "......ggggggggggggg.........",
    ".....hhhhhhhhhhhhhhh........",
    "............................",
]

BEACON_BRAZIER = [  # hilltop fire-basket, night sky
    ".e...........ee..........e..",
    "............eeee............",
    "...e.......eeeeee.......e...",
    "...........edeede...........",
    "..........eeddeee...........",
    "..........eddddee...........",
    "...........dddde............",
    "..........dddddd............",
    ".........hddddddh...........",
    "........ahhhhhhhha..........",
    ".........ahhhhhha...........",
    "...........hh...............",
    "...........hh...............",
    "..........ahha..............",
    ".........ahhhha.............",
    "...ggggggggggggggggggg......",
    "..ggggggggggggggggggggg.....",
    ".ggggggggggggggggggggggg....",
    "............................",
    "............................",
]

BEACON_CHAIN = [  # the near beacon lit, answers on the far hills
    "....ee......................",
    "...eeee.....................",
    "...edde.....................",
    "....dd......................",
    "...hhhh.............e.......",
    "...h..h............ee.......",
    "...h..h.............g.......",
    "...hhhh..........ggggg......",
    "...abba........ggggggggg....",
    "...abba.......ggggggggggg...",
    "...abba....................e",
    "...abba...................ee",
    "..aabbaa................gggg",
    "..abbbba..............gggggg",
    ".gggggggg............ggggggg",
    "gggggggggggg......gggggggggg",
    "gggggggggggggggggggggggggggg",
    "gggggggggggggggggggggggggggg",
    "............................",
    "............................",
]

# (concept, intro note tying it to the thegn name, [(composition, sprite)])
# A thegn was an Anglo-Saxon retainer: a man who held land and rank in
# return for service to his lord — warrior, householder, office-holder.
CONCEPTS = [
    ("Knight",
     "The literal reading of the name: the thegn as the king's armed "
     "retainer. The Sutton Hoo bust is the current mascot, shown here as "
     "the baseline to judge the others against.",
     [("Sutton Hoo Bust", KNIGHT_SUTTON_HOO),
      ("Spangenhelm", KNIGHT_SPANGENHELM)]),
    ("Raven",
     "The scout of the thegn's world: sent out, watches many places at "
     "once, reports back — the way the sidebar watches every worktree's "
     "CI, PR, and agent state.",
     [("Watcher", RAVEN_WATCHER), ("Perched", RAVEN_PERCHED)]),
    ("Owl",
     "The watch a thegn kept over his hall: perfectly still until "
     "something moves. Literally this program's event-loop invariant — "
     "0% idle, wake on signal.",
     [("Sentinel", OWL_SENTINEL), ("Night Watch", OWL_NIGHT_WATCH)]),
    ("Hall",
     "A thegn's standing WAS his hall. One roof housing the whole "
     "retinue, as one session houses every workspace and worktree. No "
     "creature — the place itself.",
     [("Mead-Hall", HALL_MEAD_HALL),
      ("Hall on the Hill", HALL_ON_THE_HILL)]),
    ("Longship",
     "The vessel that carries the retinue's work home to the hall — "
     "already the product's verb: `thegn land`.",
     [("Under Sail", LONGSHIP_UNDER_SAIL),
      ("Dragon Prow", LONGSHIP_DRAGON_PROW)]),
    ("Wolfhound",
     "The 'faithful retainer' sense of thegn, warm instead of armored: "
     "the hound at the hall door — loyal, alert, at your side while you "
     "work.",
     [("Hall-Hound", HOUND_HALL_HOUND), ("Portrait", HOUND_PORTRAIT)]),
    ("Falcon",
     "A lord's trained hunter, kept and flown by thegns: fast, precise, "
     "always returns to the glove. The 'everything is instant' identity "
     "as a creature.",
     [("Hooded", FALCON_HOODED), ("The Stoop", FALCON_STOOP)]),
    ("Scribe",
     "A thegn held office as well as arms — the keeper of record. What "
     "thegn actually does all day: sessions persisted, state "
     "resurrected, every diff remembered.",
     [("The Scop", SCRIBE_THE_SCOP),
      ("Illuminated Thorn", SCRIBE_ILLUMINATED)]),
    ("Smith",
     "Thegnly rank was earned by service and craft; Wayland the Smith "
     "is the Anglo-Saxon maker-legend. The persona as builder rather "
     "than servant.",
     [("Anvil & Sparks", SMITH_ANVIL), ("At the Forge", SMITH_AT_FORGE)]),
    ("Runestone",
     "No persona at all: the thorn rune (þ) that begins the name, cut "
     "in stone the way thegns' deeds were recorded. The mark itself as "
     "the brand — the most durable option.",
     [("Standing Stone", RUNESTONE_STANDING),
      ("Sunburst Thorn", RUNESTONE_SUNBURST)]),
    ("Beacon",
     "Thegns kept the beacon-fires of the realm: dark and cold until "
     "the signal comes, then instantly alight. Wake-on-signal as pure "
     "iconography.",
     [("The Brazier", BEACON_BRAZIER), ("Beacon Chain", BEACON_CHAIN)]),
]


def normalize(name, sprite):
    """Right-pad rows to PX_W and validate the grid; hard-error on overflow."""
    if len(sprite) != PX_H:
        sys.exit(f"{name}: {len(sprite)} rows, want {PX_H}")
    out = []
    for i, row in enumerate(sprite):
        if len(row) > PX_W:
            sys.exit(f"{name} row {i}: {len(row)} px, max {PX_W}: {row!r}")
        out.append(row.ljust(PX_W, '.'))
    return out


def check_palette_chars(name, sprite, roles):
    for i, row in enumerate(sprite):
        for ch in row:
            if ch != '.' and ch not in roles:
                sys.exit(f"{name} row {i}: unknown palette char {ch!r}")


def render(sprite, palette, indent="  "):
    """Half-block mosaic: fg = upper pixel, bg = lower pixel."""
    lines = []
    for top, bot in zip(sprite[0::2], sprite[1::2]):
        cells = []
        for t, b in zip(top, bot):
            tc, bc = palette.get(t), palette.get(b)
            if tc and bc:
                cells.append("\x1b[38;2;%d;%d;%dm\x1b[48;2;%d;%d;%dm▀"
                             % (*tc, *bc))
            elif tc:
                cells.append("\x1b[0m\x1b[38;2;%d;%d;%dm▀" % tc)
            elif bc:
                cells.append("\x1b[0m\x1b[38;2;%d;%d;%dm▄" % bc)
            else:
                cells.append("\x1b[0m ")
        lines.append(indent + "".join(cells) + "\x1b[0m")
    return "\n".join(lines)


def load_drill():
    """Load the drill-down series (scripts/mascot_gallery_drill.py).

    Drill designs are one-offs with their own palettes — deep explorations
    of shortlisted concepts, numbered after the base gallery. Returns
    [(concept, note, [(name, palette, rows), ...]), ...]; empty if the
    data file isn't present.
    """
    import os
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                        "mascot_gallery_drill.py")
    if not os.path.exists(path):
        return []
    ns = {}
    with open(path) as f:
        exec(compile(f.read(), path, "exec"), ns)
    return ns["SECTIONS"]


DRILL = load_drill()


def options():
    """Yield (number, concept, note, title, sprite, palette) per option."""
    n = 0
    for concept, note, comps in CONCEPTS:
        for comp_name, sprite in comps:
            for theme_name, palette in THEMES:
                n += 1
                title = f"{concept} — {comp_name} · {theme_name}"
                yield n, concept, note, title, sprite, palette
    for concept, note, designs in DRILL:
        section = f"{concept} · Drilldown"
        for design in designs:
            n += 1
            title = f"{concept} — {design['name']} · Drill"
            yield n, section, note, title, design["rows"], design["palette"]


def section_header(concept, note):
    import textwrap
    bar = "─" * 64
    body = textwrap.fill(note, width=62)
    lines = [f"\x1b[1m{bar}\x1b[0m", f"\x1b[1m  {concept.upper()}\x1b[0m"]
    lines += [f"\x1b[2m  {ln}\x1b[0m" for ln in body.splitlines()]
    lines.append(f"\x1b[1m{bar}\x1b[0m")
    return "\n".join(lines)


def main(argv):
    for concept, _note, comps in CONCEPTS:
        for comp_name, sprite in comps:
            sprite[:] = normalize(f"{concept}/{comp_name}", sprite)
            check_palette_chars(f"{concept}/{comp_name}", sprite, "abcdefgh")
    for concept, _note, designs in DRILL:
        for design in designs:
            label = f"{concept}/{design['name']}"
            design["rows"] = normalize(label, design["rows"])
            check_palette_chars(label, design["rows"], design["palette"])

    list_only = "--list" in argv
    drill_only = "--drill" in argv
    base_only = "--base" in argv
    args = [a for a in argv if not a.startswith("--")]
    want_num = int(args[0]) if args and args[0].isdigit() else None
    want_sub = args[0].lower() if args and want_num is None else None
    n_base = sum(len(c) for _, _, c in CONCEPTS) * len(THEMES)

    total = shown = 0
    last_concept = None
    for n, concept, note, title, sprite, palette in options():
        total = n
        if drill_only and n <= n_base:
            continue
        if base_only and n > n_base:
            continue
        if want_num is not None and n != want_num:
            continue
        if want_sub is not None and want_sub not in title.lower():
            continue
        shown += 1
        if concept != last_concept:
            print(section_header(concept, note))
            print()
            last_concept = concept
        print(f"\x1b[1m{n:3d} · {title}\x1b[0m")
        if not list_only:
            print(render(sprite, palette))
            print()
    if shown == 0:
        what = repr(args[0]) if args else "the requested series"
        sys.exit(f"no option matches {what} (1..{total})")


if __name__ == "__main__":
    main(sys.argv[1:])
