//! Character-cell data-viz primitives: eighth-block bars, sparklines,
//! braille graphs, the commit-heat ramp, spinner frames.
//!
//! Pure string builders — no colors, no I/O. The host's seg layer applies
//! palette tokens on top. Semantics are a 1:1 port of the design mockup's
//! renderer so rendered output matches the reference artboards cell-for-cell.

/// Eighth-block sparkline glyphs, empty → full.
pub const SPARK: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Partial-cell fills for [`hbar`], by eighths (index 1..=7).
const HPART: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];

/// Braille spinner frames (80–120ms per frame reads well).
pub const SPIN: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// The spinner frame for a monotonic tick.
pub fn spin(tick: u64) -> char {
    SPIN[(tick % SPIN.len() as u64) as usize]
}

/// Braille dot bits, `[column][row]`, rows top → bottom. A braille cell is
/// 2×4 dots; U+2800 + the OR of set bits.
const BRDOTS: [[u8; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];

fn clamp01(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

/// A precision horizontal bar: `frac` (0..=1) of `w` cells, full blocks plus
/// an eighth-block remainder. Returns only the filled part (may be shorter
/// than `w`); pair with [`bar_track`] for the dotted track.
pub fn hbar(frac: f32, w: usize) -> String {
    let cells = clamp01(frac) * w as f32;
    let full = cells.floor() as usize;
    let rem = ((cells - full as f32) * 8.0).round() as usize;
    let mut s = "█".repeat(full.min(w));
    if rem > 0 && full < w {
        s.push(if rem >= 8 { '█' } else { HPART[rem] });
    }
    s
}

/// A bar plus its `░` track filling the remaining cells: `(bar, track)`.
/// `bar.chars().count() + track.chars().count() == w`.
pub fn bar_track(frac: f32, w: usize) -> (String, String) {
    let bar = hbar(frac, w);
    let used = bar.chars().count();
    (bar, "░".repeat(w.saturating_sub(used)))
}

/// An eighth-block sparkline, one cell per value (0..=1, clamped).
pub fn sparkline(vals: &[f32]) -> String {
    vals.iter()
        .map(|&v| SPARK[(clamp01(v) * 7.0).round() as usize])
        .collect()
}

/// A filled braille area graph: `vals` are 0..=1 heights, two per cell
/// (dot columns), drawn into `w` cells × `h` rows. Missing values render
/// empty; any nonzero value shows at least one dot. Returns `h` strings,
/// top → bottom, each exactly `w` chars.
pub fn braille_graph(vals: &[f32], w: usize, h: usize) -> Vec<String> {
    let total = (h * 4) as i32;
    let hgt: Vec<i32> = (0..w * 2)
        .map(|i| {
            let v = clamp01(vals.get(i).copied().unwrap_or(0.0));
            let floor = if v > 0.001 { 1 } else { 0 };
            ((v * total as f32).round() as i32).max(floor)
        })
        .collect();
    (0..h)
        .map(|r| {
            (0..w)
                .map(|c| {
                    let mut code = 0u32;
                    for (col, bits) in BRDOTS.iter().enumerate() {
                        for (dr, bit) in bits.iter().enumerate() {
                            let from_bottom = ((h - 1 - r) * 4 + (3 - dr)) as i32;
                            if from_bottom < hgt[c * 2 + col] {
                                code |= *bit as u32;
                            }
                        }
                    }
                    char::from_u32(0x2800 + code).unwrap_or(' ')
                })
                .collect()
        })
        .collect()
}

/// A braille line graph (curve only, not filled): consecutive dot columns are
/// connected vertically so the curve reads continuously. Same shape contract
/// as [`braille_graph`].
pub fn braille_line(vals: &[f32], w: usize, h: usize) -> Vec<String> {
    let total = (h * 4) as i32;
    let ys: Vec<i32> = (0..w * 2)
        .map(|i| {
            let v = clamp01(vals.get(i).copied().unwrap_or(0.0));
            ((v * total as f32).floor() as i32).min(total - 1)
        })
        .collect();
    (0..h)
        .map(|r| {
            (0..w)
                .map(|c| {
                    let mut code = 0u32;
                    for (col, bits) in BRDOTS.iter().enumerate() {
                        let i = c * 2 + col;
                        let y = ys[i];
                        let prev = ys[i.saturating_sub(1)];
                        let (lo, hi) = (y.min(prev), y.max(prev));
                        for yy in lo..=hi {
                            let dr = 3 - (yy - ((h - 1 - r) * 4) as i32);
                            if (0..4).contains(&dr) {
                                code |= bits[dr as usize] as u32;
                            }
                        }
                    }
                    char::from_u32(0x2800 + code).unwrap_or(' ')
                })
                .collect()
        })
        .collect()
}

/// Heat-ramp level for a 0..=1 value: 0 (cold) ..= 4 (hot), for
/// [`crate::theme::Palette::heat`].
pub fn heat_index(v: f32) -> usize {
    (clamp01(v) * 4.0).round() as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hbar_endpoints_and_eighths() {
        assert_eq!(hbar(0.0, 10), "");
        assert_eq!(hbar(1.0, 10), "█".repeat(10));
        assert_eq!(hbar(2.0, 10), "█".repeat(10)); // clamped
        assert_eq!(hbar(-1.0, 10), "");
        assert_eq!(hbar(0.5, 10), "█".repeat(5));
        // 0.5 of 1 cell = 4/8 → half block.
        assert_eq!(hbar(0.5, 1), "▌");
        // 1/8 of a cell.
        assert_eq!(hbar(0.125, 1), "▏");
        // 0.96875 of 1 cell = 7.75/8 → rounds to full.
        assert_eq!(hbar(0.96875, 1), "█");
        // zero width never panics
        assert_eq!(hbar(0.7, 0), "");
    }

    #[test]
    fn bar_track_always_fills_width() {
        for frac in [0.0_f32, 0.13, 0.5, 0.31, 0.625, 0.97, 1.0] {
            for w in [1usize, 5, 14, 30] {
                let (bar, track) = bar_track(frac, w);
                assert_eq!(
                    bar.chars().count() + track.chars().count(),
                    w,
                    "frac={frac} w={w}"
                );
            }
        }
        let (bar, track) = bar_track(0.0, 4);
        assert_eq!((bar.as_str(), track.as_str()), ("", "░░░░"));
        let (bar, track) = bar_track(1.0, 4);
        assert_eq!((bar.as_str(), track.as_str()), ("████", ""));
    }

    #[test]
    fn sparkline_maps_each_value_to_an_eighth() {
        assert_eq!(sparkline(&[0.0, 1.0]), "▁█");
        assert_eq!(sparkline(&[0.5]), "▅"); // round(0.5*7)=4
        assert_eq!(sparkline(&[-3.0, 9.0]), "▁█"); // clamped
        assert_eq!(sparkline(&[]), "");
        let s = sparkline(&[0.0, 0.14, 0.29, 0.43, 0.57, 0.71, 0.86, 1.0]);
        assert_eq!(s, "▁▂▃▄▅▆▇█");
    }

    #[test]
    fn braille_graph_full_and_empty() {
        // All-1.0 → every dot set (⣿); all-0 → blank braille (U+2800).
        assert_eq!(braille_graph(&[1.0; 8], 4, 2), vec!["⣿⣿⣿⣿", "⣿⣿⣿⣿"]);
        assert_eq!(
            braille_graph(&[0.0; 8], 4, 2),
            vec!["\u{2800}".repeat(4), "\u{2800}".repeat(4)]
        );
    }

    #[test]
    fn braille_graph_known_codes() {
        // One column at height 1 (of 4): bottom-left dot only = 0x40 → ⡀
        let rows = braille_graph(&[0.25, 0.0], 1, 1);
        assert_eq!(rows, vec!["⡀"]);
        // Right column full, left empty: 0x08|0x10|0x20|0x80 = 0xB8 → ⢸
        let rows = braille_graph(&[0.0, 1.0], 1, 1);
        assert_eq!(rows, vec!["⢸"]);
        // Tiny non-zero value still shows one dot (the v>0.001 floor).
        let rows = braille_graph(&[0.01, 0.0], 1, 1);
        assert_eq!(rows, vec!["⡀"]);
        // Half height in a 2-row graph fills the bottom row only.
        let rows = braille_graph(&[0.5, 0.5], 1, 2);
        assert_eq!(rows, vec!["\u{2800}", "⣿"]);
    }

    #[test]
    fn braille_graph_shape_contract() {
        let rows = braille_graph(&[0.3, 0.7, 0.5], 5, 3); // fewer vals than 2*w
        assert_eq!(rows.len(), 3);
        for r in &rows {
            assert_eq!(r.chars().count(), 5);
        }
    }

    #[test]
    fn braille_line_connects_jumps() {
        // A jump from 0 to full in adjacent columns draws the connecting run.
        let rows = braille_line(&[0.0, 0.99], 1, 1);
        assert_eq!(rows.len(), 1);
        let code = rows[0].chars().next().unwrap() as u32 - 0x2800;
        // Left column: floor(0*4)=0 → bottom dot (0x40). Right column spans
        // 0..=3 → the whole right column (0x08|0x10|0x20|0x80).
        assert_eq!(code, 0x40 | 0x08 | 0x10 | 0x20 | 0x80);
    }

    #[test]
    fn braille_line_flat_midline() {
        let rows = braille_line(&[0.5; 8], 4, 2);
        assert_eq!(rows.len(), 2);
        // y = floor(0.5*8)=4, in the top row's bottom dot line.
        // fromBottom 4 → row 0 (top), dr = 3 - (4 - 4) = 3 → bits 0x40/0x80.
        assert_eq!(rows[0], "⣀⣀⣀⣀");
        assert_eq!(rows[1], "\u{2800}".repeat(4));
    }

    #[test]
    fn heat_index_quantizes_and_clamps() {
        assert_eq!(heat_index(0.0), 0);
        assert_eq!(heat_index(0.1), 0);
        assert_eq!(heat_index(0.13), 1);
        assert_eq!(heat_index(0.5), 2);
        assert_eq!(heat_index(0.9), 4);
        assert_eq!(heat_index(1.0), 4);
        assert_eq!(heat_index(7.0), 4);
        assert_eq!(heat_index(-1.0), 0);
    }

    #[test]
    fn spinner_wraps() {
        assert_eq!(spin(0), '⠋');
        assert_eq!(spin(9), '⠏');
        assert_eq!(spin(10), '⠋');
        assert_eq!(spin(u64::MAX), SPIN[(u64::MAX % 10) as usize]);
    }
}
