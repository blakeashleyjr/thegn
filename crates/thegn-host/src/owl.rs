//! The loading-splash owl mascot: a perched great-horned sentinel, hand-
//! authored as an indexed-palette pixel sprite and rendered with the same
//! half-block mosaic trick as [`crate::mascot`] — each cell is a `▀` whose fg
//! is the upper pixel and bg the lower pixel. Colors go on the wire as
//! `Tok::Rgb` and quantize at the usual seg color layer; ASCII terminals
//! never reach this (the splash falls back to its text variant first).
//!
//! The owl is the event loop, drawn: perfectly still until something
//! happens. Its optional blink ([`MascotMotion::Blink`]) is evaluated only
//! inside wakes that are already repainting the splash — there is **no
//! timer, no tick, no animation thread**; an idle loop never redraws, so an
//! idle owl never moves. This keeps the 0%-idle invariant by construction.
//!
//! `[theme] mascot` picks the sprite (owl / knight / off), `[theme]
//! mascot_motion` pins or enables the blink, and the owl's plumage recolors
//! per `[theme] preset` (see [`palette`]). Settings are installed by
//! [`crate::caps::install_themed`] at startup and on config reload, same
//! pattern as the glyph/color atomics.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Instant;

use termwiz::surface::Surface;
use thegn_core::config::{MascotKind, MascotMotion, ThemeConfig};

use crate::chrome::S;
use crate::seg::{Line, Tok, seg};

/// Palette roles, indexed by the row chars below.
///
/// `o` outline · `p` plumage mid · `q` plumage light · `r` plumage shadow ·
/// `u` cream chest · `t` chest barring · `l` moonlit rim · `e` eye amber ·
/// `E` eye highlight · `f` pupil / claw tips · `h` beak · `y` talons
#[cfg(test)]
const ROLES: &str = "opqrutleEfhy";

/// Pixel grid: [`PX_W`] columns × [`PX_H`] rows, `PX_H` even so two pixel
/// rows always fill one terminal cell (same invariant as the knight).
const PX_W: usize = 20;
const PX_H: usize = 22;

/// The perched sentinel, eyes open. Square-shouldered and narrow: flat
/// crown, horn tufts at the corners, straight flanks, taller than wide.
/// `'.'` is transparent — the splash background shows through.
#[rustfmt::skip]
const SPRITE: [&str; PX_H] = [
    "olo..............olo", // horn tufts
    "oqlo............oqlo",
    ".oqqo..........oqqo.",
    ".oqqqqqqqqqqqqqqqqo.", // flat crown
    "oqqqqqqqqqqqqqqqqqqo",
    "oqooooooqqqqooooooqo", // scowl brow band
    "oqoeEffoqqqqoffEeoqo", // eyes: amber ring, highlight, inward pupils
    "oqoeeeeoqhhqoeeeeoqo", // lower eye ring, beak
    "oqoooqqquhhuqqqoooqo", // facial disc edge
    "oqpqqquuuhhuuuqqqpqo", // beak tip
    "oppuuuutuuuutuuuuppo", // chest, chevron barring
    "oppuututtuuttutuuppo",
    "oppuuttuuttuuttuuppo",
    "oppuutttuuuutttuuppo",
    "orpuuttuttuttuutupro",
    "orpuuuttuuuuttuuupro",
    ".orpuuutuuuutuuupro.", // taper starts only at the tail
    ".orrpuuuuuuuuuuprro.",
    "..orrpppppppppprro..",
    "....yyy......yyy....", // talons
    "....fof......fof....", // claw tips
    "....................", // pad row: keeps PX_H even
];

/// The blink is a single-row swap: the eye row closes to the brow band
/// (the amber lower-ring row beneath it reads as shut lids).
const EYE_ROW: usize = 6;
const EYES_SHUT: &str = "oqooooooqqqqooooooqo";

/// Cell dimensions of the rendered mosaic.
pub const COLS: usize = PX_W;
pub const ROWS: usize = PX_H / 2;

/// Plumage palette for a `[theme] preset`, as `(role char, rgb)` pairs.
/// Every preset colors all of [`ROLES`]; unknown presets get prism.
pub fn palette(preset: PresetId) -> &'static [(char, (u8, u8, u8))] {
    match preset {
        PresetId::Prism => &[
            ('o', (36, 32, 44)),
            ('p', (110, 90, 68)),
            ('q', (148, 122, 90)),
            ('r', (72, 58, 44)),
            ('u', (206, 188, 152)),
            ('t', (134, 112, 82)),
            ('l', (190, 200, 228)),
            ('e', (242, 158, 34)),
            ('E', (255, 214, 92)),
            ('f', (20, 16, 12)),
            ('h', (228, 190, 62)),
            ('y', (96, 74, 46)),
        ],
        PresetId::Storm => &[
            ('o', (30, 32, 44)),
            ('p', (96, 102, 122)),
            ('q', (136, 144, 166)),
            ('r', (64, 68, 86)),
            ('u', (196, 204, 222)),
            ('t', (122, 130, 152)),
            ('l', (210, 220, 244)),
            ('e', (242, 158, 34)),
            ('E', (255, 214, 92)),
            ('f', (14, 14, 20)),
            ('h', (226, 188, 60)),
            ('y', (84, 88, 106)),
        ],
        PresetId::Light => &[
            ('o', (44, 36, 30)),
            ('p', (104, 84, 62)),
            ('q', (140, 116, 86)),
            ('r', (62, 50, 38)),
            ('u', (190, 170, 134)),
            ('t', (120, 98, 72)),
            ('l', (112, 100, 140)),
            ('e', (196, 118, 12)),
            ('E', (232, 168, 44)),
            ('f', (24, 18, 12)),
            ('h', (186, 148, 32)),
            ('y', (84, 64, 40)),
        ],
        PresetId::Abyss => &[
            ('o', (18, 16, 26)),
            ('p', (58, 54, 70)),
            ('q', (88, 84, 104)),
            ('r', (38, 36, 50)),
            ('u', (128, 124, 146)),
            ('t', (74, 70, 90)),
            ('l', (150, 160, 196)),
            ('e', (255, 170, 40)),
            ('E', (255, 224, 110)),
            ('f', (8, 8, 12)),
            ('h', (232, 194, 66)),
            ('y', (56, 52, 68)),
        ],
        PresetId::Ember => &[
            ('o', (40, 30, 28)),
            ('p', (104, 78, 62)),
            ('q', (140, 106, 80)),
            ('r', (66, 50, 42)),
            ('u', (198, 172, 136)),
            ('t', (128, 98, 74)),
            ('l', (226, 180, 140)),
            ('e', (255, 140, 24)),
            ('E', (255, 206, 80)),
            ('f', (18, 12, 10)),
            ('h', (236, 186, 52)),
            ('y', (92, 68, 48)),
        ],
        PresetId::Aurora => &[
            ('o', (28, 36, 40)),
            ('p', (84, 102, 96)),
            ('q', (118, 140, 130)),
            ('r', (56, 70, 64)),
            ('u', (180, 200, 186)),
            ('t', (108, 128, 118)),
            ('l', (170, 230, 210)),
            ('e', (240, 170, 40)),
            ('E', (255, 220, 100)),
            ('f', (10, 16, 14)),
            ('h', (224, 192, 64)),
            ('y', (76, 92, 84)),
        ],
    }
}

/// The theme presets the owl carries a plumage for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetId {
    Prism,
    Storm,
    Light,
    Abyss,
    Ember,
    Aurora,
}

impl PresetId {
    /// Map a `[theme] preset` name; unknown names keep prism plumage.
    pub fn from_name(name: &str) -> Self {
        match name {
            "storm" => PresetId::Storm,
            "light" => PresetId::Light,
            "abyss" => PresetId::Abyss,
            "ember" => PresetId::Ember,
            "aurora" => PresetId::Aurora,
            _ => PresetId::Prism,
        }
    }
}

// ── Installed settings (same atomics pattern as crate::caps) ───────────────

static KIND: AtomicU8 = AtomicU8::new(0); // 0 owl · 1 knight · 2 off
static MOTION: AtomicU8 = AtomicU8::new(0); // 0 blink · 1 still
static PRESET: AtomicU8 = AtomicU8::new(0); // PresetId discriminant

/// Install the `[theme]` mascot settings. Called from
/// [`crate::caps::install_themed`] at startup and on every config reload.
pub fn install(theme: &ThemeConfig) {
    KIND.store(
        match theme.mascot {
            MascotKind::Owl => 0,
            MascotKind::Knight => 1,
            MascotKind::Off => 2,
        },
        Ordering::Relaxed,
    );
    MOTION.store(
        match theme.mascot_motion {
            MascotMotion::Blink => 0,
            MascotMotion::Still => 1,
        },
        Ordering::Relaxed,
    );
    PRESET.store(
        match PresetId::from_name(&theme.preset) {
            PresetId::Prism => 0,
            PresetId::Storm => 1,
            PresetId::Light => 2,
            PresetId::Abyss => 3,
            PresetId::Ember => 4,
            PresetId::Aurora => 5,
        },
        Ordering::Relaxed,
    );
}

/// The configured mascot (render-time read, one atomic load).
pub fn active_kind() -> MascotKind {
    match KIND.load(Ordering::Relaxed) {
        1 => MascotKind::Knight,
        2 => MascotKind::Off,
        _ => MascotKind::Owl,
    }
}

fn active_preset() -> PresetId {
    match PRESET.load(Ordering::Relaxed) {
        1 => PresetId::Storm,
        2 => PresetId::Light,
        3 => PresetId::Abyss,
        4 => PresetId::Ember,
        5 => PresetId::Aurora,
        _ => PresetId::Prism,
    }
}

/// Whether this repaint should draw the owl mid-blink: motion is "blink"
/// and the wall clock sits inside the ~160ms shut window of a ~4.2s cycle.
/// Pure sampling — nothing schedules a wake for it, so blinks surface only
/// while other work is already repainting the splash (startup hydration,
/// resize, load steps). An idle splash holds the stare.
pub fn blink_now() -> bool {
    if MOTION.load(Ordering::Relaxed) != 0 {
        return false;
    }
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    let t = EPOCH.get_or_init(Instant::now).elapsed().as_millis();
    t % 4200 < 160
}

fn color(palette: &[(char, (u8, u8, u8))], ch: char) -> Option<(u8, u8, u8)> {
    palette.iter().find(|(c, _)| *c == ch).map(|(_, rgb)| *rgb)
}

fn row(i: usize, shut: bool) -> &'static str {
    if shut && i == EYE_ROW {
        EYES_SHUT
    } else {
        SPRITE[i]
    }
}

/// Fold two pixel rows into one seg line of `▀`/`▄` cells. Transparent
/// halves take the splash background (`S::Bg0`) so the sprite sits on the
/// filled splash without a box around it.
fn mosaic_line(pal: &[(char, (u8, u8, u8))], top_row: &str, bot_row: &str) -> Line {
    let bg0 = Tok::Slot(S::Bg0);
    let cells =
        top_row
            .chars()
            .zip(bot_row.chars())
            .map(|(t, b)| match (color(pal, t), color(pal, b)) {
                (Some(t), Some(b)) => {
                    seg(Tok::Rgb(t.0, t.1, t.2), "\u{2580}").bg(Tok::Rgb(b.0, b.1, b.2))
                }
                (Some(t), None) => seg(Tok::Rgb(t.0, t.1, t.2), "\u{2580}").bg(bg0),
                (None, Some(b)) => seg(Tok::Rgb(b.0, b.1, b.2), "\u{2584}").bg(bg0),
                (None, None) => seg(bg0, " ").bg(bg0),
            });
    Line::segs(cells.collect::<Vec<_>>())
}

/// Draw the owl with its top-left cell at `(x, y)`, clipped to `max_cols`
/// columns. `shut` selects the mid-blink frame (see [`blink_now`]). Row
/// clipping is the caller's job, as with the knight.
pub fn draw(surface: &mut Surface, x: usize, y: usize, max_cols: usize, shut: bool) {
    let pal = palette(active_preset());
    for r in 0..ROWS {
        let line = mosaic_line(pal, row(r * 2, shut), row(r * 2 + 1, shut));
        crate::seg::draw_line(
            surface,
            x,
            y + r,
            COLS.min(max_cols),
            &line,
            Tok::Slot(S::Bg0),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_PRESETS: [PresetId; 6] = [
        PresetId::Prism,
        PresetId::Storm,
        PresetId::Light,
        PresetId::Abyss,
        PresetId::Ember,
        PresetId::Aurora,
    ];

    #[test]
    fn sprite_dimensions_and_palette_for_every_preset() {
        assert_eq!(SPRITE.len(), PX_H);
        assert_eq!(PX_H % 2, 0, "even pixel height: two px rows per cell");
        for preset in ALL_PRESETS {
            let pal = palette(preset);
            assert_eq!(pal.len(), ROLES.len(), "{preset:?}");
            for (i, r) in SPRITE.iter().enumerate() {
                assert_eq!(r.chars().count(), PX_W, "row {i} width");
                for ch in r.chars() {
                    assert!(
                        ch == '.' || color(pal, ch).is_some(),
                        "{preset:?} row {i}: unknown palette char {ch:?}"
                    );
                }
            }
            for ch in EYES_SHUT.chars() {
                assert!(ch == '.' || color(pal, ch).is_some(), "{preset:?} blink");
            }
        }
    }

    #[test]
    fn palette_chars_unique_per_preset() {
        for preset in ALL_PRESETS {
            let pal = palette(preset);
            for (i, (c, _)) in pal.iter().enumerate() {
                assert!(
                    pal.iter().skip(i + 1).all(|(o, _)| o != c),
                    "{preset:?}: duplicate palette char {c:?}"
                );
            }
        }
    }

    #[test]
    fn blink_swaps_exactly_one_row_same_width() {
        assert_eq!(EYES_SHUT.chars().count(), PX_W);
        assert_ne!(SPRITE[EYE_ROW], EYES_SHUT, "eye row must change");
        for i in 0..PX_H {
            if i != EYE_ROW {
                assert_eq!(row(i, true), row(i, false), "row {i} stable");
            }
        }
        // The shut row is the brow band: no eye pixels survive the blink.
        assert!(!row(EYE_ROW, true).contains(['e', 'E', 'f']));
    }

    #[test]
    fn preset_names_map_and_unknown_falls_back_to_prism() {
        for (name, want) in [
            ("prism", PresetId::Prism),
            ("storm", PresetId::Storm),
            ("light", PresetId::Light),
            ("abyss", PresetId::Abyss),
            ("ember", PresetId::Ember),
            ("aurora", PresetId::Aurora),
            ("no-such-preset", PresetId::Prism),
        ] {
            assert_eq!(PresetId::from_name(name), want, "{name}");
        }
    }

    #[test]
    fn draw_writes_half_blocks_open_and_shut() {
        for shut in [false, true] {
            let mut s = Surface::new(COLS, ROWS);
            draw(&mut s, 0, 0, COLS, shut);
            let text: Vec<String> = s
                .screen_cells()
                .iter()
                .map(|row| row.iter().map(|c| c.str()).collect())
                .collect();
            // Tuft row: opaque half-blocks at both corners.
            assert!(
                text[0].contains('▀') || text[0].contains('▄'),
                "{shut}: {:?}",
                text[0]
            );
            // Mid-face row is opaque edge to edge.
            assert!(text[3].starts_with('▀'), "{shut}: {:?}", text[3]);
            // Center of the crown row stays owl, not background.
            assert!(!text[2].trim().is_empty(), "{shut}");
        }
    }
}
