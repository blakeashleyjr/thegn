//! The builder's palette picker (THE-2023): pure cell generation, grid
//! navigation and line layout. Every value is generated (the theme's own
//! roles, the xterm ANSI 16, an HSL hue x lightness ramp), so the source holds
//! no color literal; the builder paints cells through `Tok`.

use thegn_core::theme::Palette;

use super::elements::ColorRole;

/// The grid width of the widest section; the picker never needs more columns.
pub(super) const MAX_COLS: usize = 12;
const HUES: usize = 12;
const LIGHTNESS_STEPS: usize = 7;
const SATURATION: f32 = 0.7;

/// One titled block of swatches laid out `cols` per grid row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PickerSection {
    pub name: &'static str,
    pub cols: usize,
    pub cells: Vec<String>,
}

impl PickerSection {
    fn rows(&self) -> usize {
        self.cells.len().div_ceil(self.cols.max(1))
    }
}

/// The open picker: which role it edits, the value to restore on Esc, whether
/// the role was already an intentional edit before the picker opened, the
/// `(section, index)` cursor, and a wheel scroll offset (`None` = follow the
/// cursor).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PickerState {
    pub role: ColorRole,
    pub original: String,
    pub was_edited: bool,
    pub cursor: (usize, usize),
    pub scroll: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Move {
    Left,
    Right,
    Up,
    Down,
}

/// One rendered picker line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GridLine {
    Header(usize),
    Cells { section: usize, row: usize },
}

/// The picker's sections for `theme`: its own role values (deduplicated, in
/// token order), the ANSI 16, and a 12-hue x 7-lightness ramp.
pub(super) fn palette_cells(theme: &Palette) -> Vec<PickerSection> {
    let roles = [
        &theme.bg0,
        &theme.bg1,
        &theme.panel,
        &theme.panel2,
        &theme.raise,
        &theme.border,
        &theme.text,
        &theme.dim,
        &theme.faint,
        &theme.ghost,
        &theme.accent,
        &theme.focus,
        &theme.hues.teal,
        &theme.hues.magenta,
        &theme.hues.purple,
        &theme.hues.green,
        &theme.hues.amber,
        &theme.hues.red,
        &theme.hues.blue,
        &theme.hues.orange,
    ];
    let mut own = Vec::with_capacity(roles.len());
    for frag in roles {
        let hex = frag_to_hex(frag);
        if !own.contains(&hex) {
            own.push(hex);
        }
    }
    let ansi = (0..16u8)
        .map(|index| rgb_hex(thegn_core::termcaps::index_256_to_rgb(index)))
        .collect();
    let mut ramp = Vec::with_capacity(HUES * LIGHTNESS_STEPS);
    for step in 0..LIGHTNESS_STEPS {
        let lightness = (step as f32 + 2.0) / 10.0;
        for hue in 0..HUES {
            ramp.push(rgb_hex(hsl_to_rgb(
                hue as f32 * 360.0 / HUES as f32,
                SATURATION,
                lightness,
            )));
        }
    }
    vec![
        PickerSection {
            name: "theme",
            cols: 10,
            cells: own,
        },
        PickerSection {
            name: "ANSI 16",
            cols: 8,
            cells: ansi,
        },
        PickerSection {
            name: "hue x lightness",
            cols: HUES,
            cells: ramp,
        },
    ]
}

/// The cell at `cursor`, if it exists.
pub(super) fn cell(sections: &[PickerSection], cursor: (usize, usize)) -> Option<&str> {
    sections
        .get(cursor.0)
        .and_then(|section| section.cells.get(cursor.1))
        .map(String::as_str)
}

/// The first cell whose value equals `value` (case-insensitive).
pub(super) fn locate(sections: &[PickerSection], value: &str) -> Option<(usize, usize)> {
    sections.iter().enumerate().find_map(|(section, block)| {
        block
            .cells
            .iter()
            .position(|cell| cell.eq_ignore_ascii_case(value))
            .map(|index| (section, index))
    })
}

/// Move the cursor one step. Left/Right walk cells (crossing into the
/// neighbouring section at its ends); Up/Down move a grid row, keeping the
/// column where the neighbouring section allows it. Moves off the grid stay.
pub(super) fn step(
    sections: &[PickerSection],
    cursor: (usize, usize),
    dir: Move,
) -> (usize, usize) {
    let (section, index) = cursor;
    let Some(block) = sections.get(section) else {
        return cursor;
    };
    let cols = block.cols.max(1);
    let len = block.cells.len();
    let next_nonempty = |from: usize| {
        (from + 1..sections.len()).find(|candidate| !sections[*candidate].cells.is_empty())
    };
    let prev_nonempty = |from: usize| {
        (0..from)
            .rev()
            .find(|candidate| !sections[*candidate].cells.is_empty())
    };
    match dir {
        Move::Right if index + 1 < len => (section, index + 1),
        Move::Right => next_nonempty(section).map_or(cursor, |next| (next, 0)),
        Move::Left if index > 0 => (section, index - 1),
        Move::Left => {
            prev_nonempty(section).map_or(cursor, |prev| (prev, sections[prev].cells.len() - 1))
        }
        Move::Down if index + cols < len => (section, index + cols),
        Move::Down => next_nonempty(section).map_or(cursor, |next| {
            let column = index % cols;
            (next, column.min(sections[next].cells.len() - 1))
        }),
        Move::Up if index >= cols => (section, index - cols),
        Move::Up => prev_nonempty(section).map_or(cursor, |prev| {
            let prev_cols = sections[prev].cols.max(1);
            let prev_len = sections[prev].cells.len();
            let last_row = (prev_len - 1) / prev_cols * prev_cols;
            (prev, (last_row + index % cols).min(prev_len - 1))
        }),
    }
}

/// The picker's lines in render order: a header per section, then its rows.
pub(super) fn grid_lines(sections: &[PickerSection]) -> Vec<GridLine> {
    let mut lines = Vec::new();
    for (section, block) in sections.iter().enumerate() {
        lines.push(GridLine::Header(section));
        lines.extend((0..block.rows()).map(|row| GridLine::Cells { section, row }));
    }
    lines
}

/// The line holding `cursor`.
pub(super) fn cursor_line(
    lines: &[GridLine],
    sections: &[PickerSection],
    cursor: (usize, usize),
) -> usize {
    let cols = sections
        .get(cursor.0)
        .map_or(1, |section| section.cols.max(1));
    let row = cursor.1 / cols;
    lines
        .iter()
        .position(|line| {
            *line
                == GridLine::Cells {
                    section: cursor.0,
                    row,
                }
        })
        .unwrap_or(0)
}

/// Parse a `#rrggbb` (or `#rgb`) value.
pub(super) fn parse_hex(value: &str) -> Option<(u8, u8, u8)> {
    let digits = value.strip_prefix('#')?;
    if !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    match digits.len() {
        6 => Some((
            u8::from_str_radix(&digits[0..2], 16).ok()?,
            u8::from_str_radix(&digits[2..4], 16).ok()?,
            u8::from_str_radix(&digits[4..6], 16).ok()?,
        )),
        3 => {
            let nibble = |at: usize| u8::from_str_radix(&digits[at..=at], 16).map(|n| n * 17);
            Some((nibble(0).ok()?, nibble(1).ok()?, nibble(2).ok()?))
        }
        _ => None,
    }
}

fn rgb_hex((r, g, b): (u8, u8, u8)) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// A palette `"R;G;B"` fragment as `#rrggbb`.
fn frag_to_hex(frag: &str) -> String {
    let mut parts = frag
        .split(';')
        .map(|part| part.trim().parse::<u8>().unwrap_or(0));
    rgb_hex((
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    ))
}

/// HSL (`hue` in degrees, `saturation`/`lightness` in 0..=1) to RGB.
fn hsl_to_rgb(hue: f32, saturation: f32, lightness: f32) -> (u8, u8, u8) {
    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let sector = hue.rem_euclid(360.0) / 60.0;
    let second = chroma * (1.0 - (sector % 2.0 - 1.0).abs());
    let (r, g, b) = match sector as u32 {
        0 => (chroma, second, 0.0),
        1 => (second, chroma, 0.0),
        2 => (0.0, chroma, second),
        3 => (0.0, second, chroma),
        4 => (second, 0.0, chroma),
        _ => (chroma, 0.0, second),
    };
    let offset = lightness - chroma / 2.0;
    let channel = |value: f32| ((value + offset).clamp(0.0, 1.0) * 255.0).round() as u8;
    (channel(r), channel(g), channel(b))
}

#[cfg(test)]
mod tests {
    use super::{
        GridLine, Move, PickerSection, cell, cursor_line, frag_to_hex, grid_lines, hsl_to_rgb,
        locate, palette_cells, parse_hex, step,
    };
    use thegn_core::theme::Palette;

    #[test]
    fn palette_cells_sizes_parse_and_stay_unique_per_section() {
        let palette = Palette::default();
        let sections = palette_cells(&palette);
        assert_eq!(sections.len(), 3);
        let own = &sections[0];
        assert!(!own.cells.is_empty() && own.cells.len() <= 20);
        assert_eq!(sections[1].cells.len(), 16);
        assert_eq!(sections[2].cells.len(), 84);
        for section in &sections {
            let mut seen = section.cells.clone();
            seen.sort_unstable();
            seen.dedup();
            assert_eq!(
                seen.len(),
                section.cells.len(),
                "duplicate in {}",
                section.name
            );
            for value in &section.cells {
                assert_eq!(value.len(), 7, "{value}");
                assert!(parse_hex(value).is_some(), "{value} does not parse");
            }
        }
        // The theme section is exactly the distinct role values.
        assert!(own.cells.contains(&frag_to_hex(&palette.bg0)));
        assert!(own.cells.contains(&frag_to_hex(&palette.hues.orange)));
    }

    #[test]
    fn hsl_helper_hits_the_primaries() {
        assert_eq!(hsl_to_rgb(0.0, 1.0, 0.5), (255, 0, 0));
        assert_eq!(hsl_to_rgb(120.0, 1.0, 0.5), (0, 255, 0));
        assert_eq!(hsl_to_rgb(240.0, 1.0, 0.5), (0, 0, 255));
        assert_eq!(hsl_to_rgb(0.0, 0.0, 1.0), (255, 255, 255));
    }

    #[test]
    fn parse_hex_accepts_short_and_long_forms_only() {
        assert_eq!(parse_hex("#0a0B0c"), Some((10, 11, 12)));
        assert_eq!(parse_hex("#fff"), Some((255, 255, 255)));
        assert_eq!(parse_hex("fff"), None);
        assert_eq!(parse_hex("#ffff"), None);
        assert_eq!(parse_hex("#gggggg"), None);
    }

    fn grid() -> Vec<PickerSection> {
        let section = |name: &'static str, cols: usize, len: usize| PickerSection {
            name,
            cols,
            cells: (0..len).map(|i| format!("{name}{i}")).collect(),
        };
        vec![
            section("a", 10, 13),
            section("b", 8, 16),
            section("c", 12, 24),
        ]
    }

    #[test]
    fn step_walks_cells_and_crosses_sections() {
        let g = grid();
        assert_eq!(step(&g, (0, 0), Move::Left), (0, 0));
        assert_eq!(step(&g, (0, 12), Move::Right), (1, 0));
        assert_eq!(step(&g, (1, 0), Move::Left), (0, 12));
        assert_eq!(step(&g, (0, 2), Move::Down), (0, 12));
        // Row 2 of `a` has 3 cells; column 5 clamps into section `b`.
        assert_eq!(step(&g, (0, 5), Move::Down), (1, 5));
        assert_eq!(step(&g, (1, 3), Move::Up), (0, 12));
        assert_eq!(step(&g, (1, 1), Move::Up), (0, 11));
        assert_eq!(step(&g, (2, 23), Move::Down), (2, 23));
        assert_eq!(step(&g, (2, 23), Move::Right), (2, 23));
        assert_eq!(cell(&g, (1, 2)), Some("b2"));
        assert_eq!(locate(&g, "C3"), Some((2, 3)));
    }

    #[test]
    fn grid_lines_put_each_cursor_on_its_row() {
        let g = grid();
        let lines = grid_lines(&g);
        // a: header + 2 rows, b: header + 2 rows, c: header + 2 rows.
        assert_eq!(lines.len(), 9);
        assert_eq!(lines[0], GridLine::Header(0));
        assert_eq!(cursor_line(&lines, &g, (0, 11)), 2);
        assert_eq!(cursor_line(&lines, &g, (2, 0)), 7);
    }
}
