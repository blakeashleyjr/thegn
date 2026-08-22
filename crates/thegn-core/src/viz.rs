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
    // NaN is the "absent sample" marker (see `crate::series`); clamp maps it to
    // 0.0 rather than propagating, so a gap draws as an empty column instead of
    // poisoning the whole plot.
    if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) }
}

/// Index of the value that belongs in dot column `i` of a `w`-cell plot.
///
/// Series are right-aligned — "now" sits at the RIGHT edge (see
/// `thegn_host::telemetry`) — so when a caller supplies more values than the
/// plot has dot columns, the surplus must come off the FRONT. Reading `vals[i]`
/// directly would drop the *newest* samples, which is the one failure mode that
/// silently lies about the present; every over-supply now degrades to dropping
/// the oldest instead.
fn dot_offset(len: usize, w: usize) -> usize {
    len.saturating_sub(w * 2)
}

/// The last `cols * 2` values, front-padded with `0.0` — exactly what a
/// `cols`-wide braille plot consumes. Callers used to write `gw * 2` by hand at
/// every site; this is that expression, named.
pub fn fit(vals: &[f32], cols: usize) -> Vec<f32> {
    let want = cols * 2;
    let take = vals.len().min(want);
    let mut out = vec![0.0; want - take];
    out.extend_from_slice(&vals[vals.len() - take..]);
    out
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
    let off = dot_offset(vals.len(), w);
    let hgt: Vec<i32> = (0..w * 2)
        .map(|i| {
            let v = clamp01(vals.get(off + i).copied().unwrap_or(0.0));
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
    let off = dot_offset(vals.len(), w);
    let ys: Vec<i32> = (0..w * 2)
        .map(|i| {
            let v = clamp01(vals.get(off + i).copied().unwrap_or(0.0));
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

/// A braille min–max band: each dot column is filled from `lo[i]` to `hi[i]`.
///
/// This is what makes a compressed time window honest. Once a plot spans an
/// hour, one dot column covers ~30 seconds of samples; [`braille_graph`] over a
/// mean would average a 2-second 100% spike into invisibility, and over a max
/// would hide how quiet the rest of the bucket was. Drawing the range shows
/// both. Same shape contract as [`braille_graph`]: `h` strings of `w` chars,
/// top → bottom, right-aligned when over-supplied.
pub fn braille_band(lo: &[f32], hi: &[f32], w: usize, h: usize) -> Vec<String> {
    let total = (h * 4) as i32;
    let (off_lo, off_hi) = (dot_offset(lo.len(), w), dot_offset(hi.len(), w));
    // Per dot column, the inclusive dot range [bottom, top] to fill. A band of
    // zero width still lights one dot so the series never vanishes.
    let span: Vec<(i32, i32)> = (0..w * 2)
        .map(|i| {
            let l = clamp01(lo.get(off_lo + i).copied().unwrap_or(0.0));
            let hgh = clamp01(hi.get(off_hi + i).copied().unwrap_or(0.0));
            let (l, hgh) = (l.min(hgh), l.max(hgh));
            let bot = ((l * total as f32).floor() as i32).clamp(0, total - 1);
            let top = ((hgh * total as f32).ceil() as i32 - 1).clamp(bot, total - 1);
            (bot, top)
        })
        .collect();
    (0..h)
        .map(|r| {
            (0..w)
                .map(|c| {
                    let mut code = 0u32;
                    for (col, bits) in BRDOTS.iter().enumerate() {
                        let (bot, top) = span[c * 2 + col];
                        for (dr, bit) in bits.iter().enumerate() {
                            let from_bottom = ((h - 1 - r) * 4 + (3 - dr)) as i32;
                            if from_bottom >= bot && from_bottom <= top {
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

/// What a series' raw values mean, so [`axis_labels`] can format them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    /// 0–100, rendered `42%`.
    Percent,
    /// Degrees Celsius, rendered `61°`.
    Celsius,
    /// Bytes/second, rendered `1.4M`.
    BytesPerSec,
    /// Bytes, rendered `512M`.
    Bytes,
    /// A bare number (load average), rendered `1.42`.
    Ratio,
    /// Megahertz, rendered `3.2G`.
    Megahertz,
    /// Watts, rendered `18W`.
    Watts,
}

impl Unit {
    /// Format one raw value for an axis gutter — deliberately terse (**≤5
    /// cells, guaranteed**), since the gutter steals width from the plot itself
    /// and a label that overflows would shove the plot out of its box.
    ///
    /// Every arm falls back to a compact SI form once the plain rendering would
    /// exceed the budget. That matters for genuinely unbounded readings — a
    /// per-core CPU sum can exceed 100%, a load average is unbounded — and it
    /// means no caller has to pre-clamp to keep the layout intact.
    pub fn fmt(self, v: f32) -> String {
        /// Divide by `step` until the value fits, tagging with a suffix.
        fn si(v: f32, step: f32, suffixes: &[&str]) -> String {
            let mut v = v.abs();
            let mut i = 0;
            while v >= 1000.0 && i + 1 < suffixes.len() {
                v /= step;
                i += 1;
            }
            if i == 0 {
                format!("{v:.0}{}", suffixes[0])
            } else if v < 10.0 {
                format!("{v:.1}{}", suffixes[i])
            } else if v < 1000.0 {
                format!("{v:.0}{}", suffixes[i])
            } else {
                // Past the largest suffix — clamp rather than overflow the gutter.
                format!("999{}", suffixes[suffixes.len() - 1])
            }
        }
        if !v.is_finite() {
            return "—".into();
        }
        match self {
            // 0–100 is the common case ("42%"), but a per-core sum is unbounded.
            Unit::Percent if v.abs() < 10_000.0 => format!("{v:.0}%"),
            Unit::Percent => si(v, 1000.0, &["%", "k%", "M%"]),
            Unit::Celsius if v.abs() < 10_000.0 => format!("{v:.0}°"),
            Unit::Celsius => si(v, 1000.0, &["°", "k°"]),
            Unit::BytesPerSec | Unit::Bytes => si(v, 1024.0, &["B", "K", "M", "G", "T", "P"]),
            Unit::Ratio if v.abs() < 100.0 => format!("{v:.2}"),
            Unit::Ratio if v.abs() < 100_000.0 => format!("{v:.0}"),
            Unit::Ratio => si(v, 1000.0, &["", "k", "M", "G"]),
            Unit::Megahertz => si(v * 1e6, 1000.0, &["Hz", "kHz", "M", "G", "T"]),
            Unit::Watts if v.abs() < 1000.0 => format!("{v:.0}W"),
            Unit::Watts => si(v, 1000.0, &["W", "kW", "MW"]),
        }
    }
}

/// Axis tick labels for a `rows`-tall plot spanning `min..=max`, top → bottom,
/// right-aligned to a common width so the gutter is a clean column. `rows == 1`
/// labels the max alone; interior rows of a tall plot are blank rather than
/// crowded, so only the top, middle, and bottom carry text.
pub fn axis_labels(min: f32, max: f32, rows: usize, unit: Unit) -> Vec<String> {
    if rows == 0 {
        return Vec::new();
    }
    let mut out: Vec<String> = (0..rows)
        .map(|r| {
            let show = rows <= 2 || r == 0 || r == rows - 1 || r == rows / 2;
            if !show {
                return String::new();
            }
            // Row 0 is the TOP of the plot, hence `max` at r == 0.
            let frac = 1.0 - (r as f32 / (rows.max(2) - 1) as f32);
            unit.fmt(min + (max - min) * frac)
        })
        .collect();
    let w = out.iter().map(|s| s.chars().count()).max().unwrap_or(0);
    for s in &mut out {
        let pad = w - s.chars().count();
        *s = " ".repeat(pad) + s;
    }
    out
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
    fn oversupplied_series_keeps_the_newest_values() {
        // 4 dot columns available, 8 values supplied. The plot must show the
        // LAST four (the present), not the first four (ancient history).
        let vals = [1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0];
        assert_eq!(braille_graph(&vals, 2, 1), vec!["\u{2800}\u{2800}"]);
        // Mirror image: the newest four are full.
        let vals = [0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0];
        assert_eq!(braille_graph(&vals, 2, 1), vec!["⣿⣿"]);
        // braille_line right-aligns identically.
        let vals = [1.0, 1.0, 0.0, 0.0];
        let flat_low = braille_line(&[0.0, 0.0], 1, 1);
        assert_eq!(braille_line(&vals, 1, 1), flat_low);
    }

    #[test]
    fn existing_callers_are_unaffected_by_right_alignment() {
        // Every in-tree caller sizes `w = len.div_ceil(2)`, so `w*2` is `len` or
        // `len+1` and the offset is always 0 — this change is a no-op for them.
        for len in 1..40usize {
            let w = len.div_ceil(2);
            assert_eq!(dot_offset(len, w), 0, "len={len} w={w}");
        }
    }

    #[test]
    fn nan_reads_as_an_empty_column_not_a_poisoned_plot() {
        // `f32::NAN` is the absent-sample marker; it must not blank neighbours.
        let rows = braille_graph(&[f32::NAN, 1.0], 1, 1);
        assert_eq!(rows, vec!["⢸"]);
        assert_eq!(sparkline(&[f32::NAN]), "▁");
        assert_eq!(hbar(f32::NAN, 4), "");
        assert_eq!(heat_index(f32::NAN), 0);
    }

    #[test]
    fn fit_right_aligns_and_front_pads() {
        assert_eq!(fit(&[1.0, 2.0, 3.0], 1), vec![2.0, 3.0]);
        assert_eq!(fit(&[1.0], 2), vec![0.0, 0.0, 0.0, 1.0]);
        assert_eq!(fit(&[], 2), vec![0.0; 4]);
        assert_eq!(fit(&[1.0, 2.0], 0), Vec::<f32>::new());
        // Always exactly cols*2 long.
        for cols in 0..8usize {
            for len in 0..12usize {
                let v: Vec<f32> = (0..len).map(|i| i as f32).collect();
                assert_eq!(fit(&v, cols).len(), cols * 2, "cols={cols} len={len}");
            }
        }
    }

    #[test]
    fn braille_band_fills_between_lo_and_hi() {
        // Full-range band == a full area graph.
        assert_eq!(braille_band(&[0.0; 2], &[1.0; 2], 1, 1), vec!["⣿"]);
        // A zero-width band at the floor still lights the bottom dot row, so a
        // quiet series reads as flat rather than absent. (0x40 | 0x80 = ⣀)
        assert_eq!(braille_band(&[0.0; 2], &[0.0; 2], 1, 1), vec!["⣀"]);
        // Reversed inputs are normalized, not empty.
        let a = braille_band(&[0.75, 0.75], &[0.25, 0.25], 1, 1);
        let b = braille_band(&[0.25, 0.25], &[0.75, 0.75], 1, 1);
        assert_eq!(a, b);
        // Shape contract holds.
        let rows = braille_band(&[0.2; 6], &[0.8; 6], 4, 3);
        assert_eq!(rows.len(), 3);
        for r in &rows {
            assert_eq!(r.chars().count(), 4);
        }
    }

    #[test]
    fn axis_labels_are_right_aligned_top_down() {
        // Top is the max, bottom is the min, all padded to one column width.
        let l = axis_labels(0.0, 100.0, 3, Unit::Percent);
        assert_eq!(l, vec!["100%", " 50%", "  0%"]);
        assert_eq!(
            axis_labels(0.0, 1.0, 0, Unit::Percent),
            Vec::<String>::new()
        );
        // A tall plot labels only top / middle / bottom.
        let l = axis_labels(0.0, 100.0, 5, Unit::Percent);
        assert!(l[1].trim().is_empty() && l[3].trim().is_empty());
    }

    #[test]
    fn unit_formats_terse() {
        assert_eq!(Unit::Percent.fmt(42.4), "42%");
        assert_eq!(Unit::Celsius.fmt(61.2), "61°");
        assert_eq!(Unit::Ratio.fmt(1.4159), "1.42");
        assert_eq!(Unit::Megahertz.fmt(3200.0), "3.2G");
        assert_eq!(Unit::Megahertz.fmt(800.0), "800M");
        assert_eq!(Unit::BytesPerSec.fmt(512.0), "512B");
        assert_eq!(Unit::Bytes.fmt(2.0 * 1024.0 * 1024.0), "2.0M");
        assert_eq!(Unit::Watts.fmt(18.4), "18W");
        // Every unit stays inside the 5-cell gutter budget.
        for u in [
            Unit::Percent,
            Unit::Celsius,
            Unit::BytesPerSec,
            Unit::Bytes,
            Unit::Ratio,
            Unit::Megahertz,
            Unit::Watts,
        ] {
            for v in [0.0_f32, 1.0, 999.0, 1e6, 1e12] {
                assert!(u.fmt(v).chars().count() <= 5, "{u:?} {v} -> {}", u.fmt(v));
            }
        }
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
