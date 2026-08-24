//! Plot axes: rounded tick values for the y gutter, and the time tick row that
//! labels the x extent.
//!
//! Pure and allocation-light. [`crate::series`] is the arithmetic that maps
//! readings onto the plot, [`crate::viz`] is the glyphs and the unit vocabulary;
//! this is the layer that turns "the window peaked at 37.42 MB/s" into an axis a
//! person can read off.
//!
//! # Why the ceiling is rounded rather than the data's own maximum
//!
//! [`crate::series::Scale::Window`] divides by the window's own peak, so the top
//! of the plot is whatever that peak happened to be. Labelling it directly gives
//! a gutter of `36M / 14M / 0B` — every number arbitrary, and none of them a
//! value you can compare against tomorrow's. Rounding the ceiling **up** to a
//! step from a familiar family costs a little headroom and buys an axis whose
//! every row is a number worth reading.
//!
//! # Why byte axes step in powers of two
//!
//! A decimal step over a binary magnitude renders as `1.0M 2.1M 3.1M` — the
//! labels drift because the divisor and the step disagree. Stepping in powers of
//! two makes every tick an exact multiple of the unit it is printed in.

use crate::series::{self, Scale};
use crate::viz::{Unit, UnitFmt};

/// The step family a rounded axis may draw from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickBase {
    /// 1 / 2 / 2.5 / 5 × 10^k.
    ///
    /// `2.5` earns its place: without it a 5-row percent plot would step 2 → 5
    /// and overshoot 100 to 200, instead of landing exactly on `100 75 50 25 0`.
    Decimal,
    /// Powers of two. Every tick is then an exact binary multiple of the
    /// magnitude it is printed in, so a byte axis never renders `1.0M 2.1M`.
    Binary,
}

/// The step family a unit's axis should use under `uf`.
///
/// Byte *counts* follow the configured base — a decimal-base user is reading
/// powers of 1000, so their axis should step in them. Bit rates are decimal by
/// convention regardless.
pub fn tick_base(unit: Unit, uf: UnitFmt) -> TickBase {
    match unit {
        Unit::Bytes | Unit::BytesPerSec => match uf.bytes {
            crate::viz::ByteBase::Binary => TickBase::Binary,
            crate::viz::ByteBase::Decimal => TickBase::Decimal,
        },
        _ => TickBase::Decimal,
    }
}

/// The smallest axis top `>= max` whose `top / divisions` is a member of
/// `base`'s step family — so every one of the `divisions + 1` tick rows is a
/// round number.
///
/// `divisions` is `rows - 1`. A non-finite, zero or negative `max` yields
/// `divisions as f32`, i.e. a step of 1 in the metric's own unit: an all-idle
/// window then reads `4 3 2 1 0` rather than dividing by nothing or labelling
/// every row `0`.
pub fn nice_ceiling(max: f32, divisions: usize, base: TickBase) -> f32 {
    let divisions = divisions.max(1);
    let d = divisions as f32;
    if !max.is_finite() || max <= 0.0 {
        return d;
    }
    let mut step = nice_step(max / d, base);
    // Float division can leave `step * d` a hair under `max`. Walking the family
    // rather than nudging the product keeps every tick round.
    let mut top = step * d;
    let mut guard = 0;
    while top < max && guard < 8 {
        step = nice_step_above(step, base);
        top = step * d;
        guard += 1;
    }
    if top.is_finite() { top } else { max }
}

/// The smallest member of `base`'s family that is `>= target`.
fn nice_step(target: f32, base: TickBase) -> f32 {
    if !target.is_finite() || target <= 0.0 {
        return 1.0;
    }
    match base {
        TickBase::Binary => {
            let s = 2f32.powf(target.log2().ceil());
            if s.is_finite() && s >= target {
                s
            } else {
                target
            }
        }
        TickBase::Decimal => {
            let pow = 10f32.powf(target.log10().floor());
            if !pow.is_finite() || pow <= 0.0 {
                return target;
            }
            let frac = target / pow;
            let m = if frac <= 1.0 {
                1.0
            } else if frac <= 2.0 {
                2.0
            } else if frac <= 2.5 {
                2.5
            } else if frac <= 5.0 {
                5.0
            } else {
                10.0
            };
            m * pow
        }
    }
}

/// The next member of the family strictly above `step`.
fn nice_step_above(step: f32, base: TickBase) -> f32 {
    match base {
        TickBase::Binary => step * 2.0,
        TickBase::Decimal => {
            let pow = 10f32.powf(step.log10().floor());
            let frac = step / pow;
            let m = if frac < 1.995 {
                2.0
            } else if frac < 2.495 {
                2.5
            } else if frac < 4.995 {
                5.0
            } else {
                10.0
            };
            m * pow
        }
    }
}

/// Gutter labels for a `rows`-tall plot spanning `0..=top`, top → bottom,
/// right-aligned to a common width so the gutter is a clean column.
///
/// **Every** row is labelled, not just three: once the ceiling is rounded, every
/// intermediate tick is a round number too, so blanking them throws away
/// information that now costs nothing to print.
pub fn y_gutter(rows: usize, top: f32, unit: Unit, uf: UnitFmt) -> Vec<String> {
    if rows == 0 {
        return Vec::new();
    }
    if rows == 1 {
        return pad_column(vec![uf.fmt(unit, top)]);
    }
    let last = (rows - 1) as f32;
    let vals: Vec<f32> = (0..rows)
        .map(|r| {
            // Row 0 is the TOP. The bottom is computed as an exact 0 rather than
            // `top * 0.0`, so a float crumb can never print as `-0` or `1`.
            if r == rows - 1 {
                0.0
            } else {
                top * (rows - 1 - r) as f32 / last
            }
        })
        .collect();
    y_gutter_values(&vals, unit, uf)
}

/// Gutter labels for a plot whose rows are **not** evenly spaced in value — the
/// [`Scale::Log`] case, where the caller supplies each row's raw value.
pub fn y_gutter_values(values: &[f32], unit: Unit, uf: UnitFmt) -> Vec<String> {
    pad_column(values.iter().map(|v| uf.fmt(unit, *v)).collect())
}

/// Gutter labels for a log plot of `rows` rows whose top is `axis_max`.
///
/// A log axis cannot be rounded: its rows are not evenly spaced in value, so a
/// "nice" label would be a label that lies about which row it sits on. Each row
/// is instead the exact inverse of the map that drew it.
pub fn y_gutter_log(
    rows: usize,
    axis_max: f32,
    floor: f32,
    unit: Unit,
    uf: UnitFmt,
) -> Vec<String> {
    if rows == 0 {
        return Vec::new();
    }
    let last = (rows.max(2) - 1) as f32;
    let vals: Vec<f32> = (0..rows)
        .map(|r| {
            let t = 1.0 - r as f32 / last;
            series::denormalize(t, Scale::Log { floor }, axis_max)
        })
        .collect();
    y_gutter_values(&vals, unit, uf)
}

fn pad_column(mut labels: Vec<String>) -> Vec<String> {
    let w = labels.iter().map(|s| s.chars().count()).max().unwrap_or(0);
    for s in &mut labels {
        let pad = w - s.chars().count();
        *s = " ".repeat(pad) + s;
    }
    labels
}

/// Tick spacings a time axis may use, ascending. Each is a division a person
/// reads without arithmetic — no 45s, no 7m.
const STEPS_SECS: [u32; 14] = [
    10, 15, 30, 60, 120, 300, 600, 900, 1_800, 3_600, 10_800, 21_600, 43_200, 86_400,
];

/// Cells a tick needs to itself before the row reads as a crowd rather than a
/// scale. Sized so a 90-cell 12h axis picks 3h steps, not 1h.
const MIN_TICK_CELLS: usize = 9;

/// One row of x-axis tick labels for a plot `w` cells wide covering the last
/// `span_secs`, with "now" at the **right** edge — matching the right-alignment
/// [`crate::viz::braille_graph`] and friends already use, so a plot and its axis
/// agree about where the present is.
///
/// Returns exactly `w` display cells (pure ASCII, so cells and chars coincide).
/// Ticks fall on the coarsest [`STEPS_SECS`] division that still yields at least
/// two labels with room to breathe. When two labels would collide the **older**
/// one is dropped — the same bias as `viz::dot_offset`, so the present is never
/// the thing that disappears.
pub fn time_axis(span_secs: f32, w: usize) -> String {
    if w == 0 {
        return String::new();
    }
    let blank = " ".repeat(w);
    if !span_secs.is_finite() || span_secs <= 0.0 {
        return blank;
    }
    let mut row: Vec<char> = vec![' '; w];
    // "now" always earns its place; without room for it plus one older tick
    // there is no scale to read, so draw nothing rather than a lone word.
    if w < MIN_TICK_CELLS {
        return blank;
    }
    let max_ticks = (w / MIN_TICK_CELLS).max(2);
    let span = span_secs as u64;
    let step = STEPS_SECS
        .iter()
        .copied()
        .find(|s| {
            let n = span / u64::from(*s) + 1;
            n >= 2 && n <= max_ticks as u64
        })
        // Nothing in the family divides this span sparsely enough (a span past a
        // day, or a very narrow plot): fall back to labelling the two ends.
        .unwrap_or(span.max(1) as u32);

    // One unit for the whole row, chosen from the step. Formatting each label
    // independently would mix them — a 2-minute axis stepping by 30s would read
    // `2m 90s 1m 30s`, three units in four labels.
    let (div, suffix) = age_unit(step);

    // Place newest first and refuse anything that would touch it, so crowding
    // sheds history rather than the present. `w + 1` (not `w`) so the first
    // label, which ends flush against the right edge, is not rejected for
    // touching a boundary that nothing occupies.
    let mut leftmost = w + 1;
    let mut age = 0u64;
    loop {
        let label = if age == 0 {
            "now".to_string()
        } else {
            format!("{}{suffix}", age / div)
        };
        let len = label.len();
        // The tick's own column, measured from the right edge.
        let frac = age as f32 / span_secs;
        let from_right = (frac * (w - 1) as f32).round() as usize;
        let col_right = (w - 1).saturating_sub(from_right);
        // Right-align the label on its tick, clamped into the row. The oldest
        // tick sits at column 0 and can only extend rightwards.
        let start = (col_right + 1)
            .saturating_sub(len)
            .min(w.saturating_sub(len));
        if start + len < leftmost {
            for (i, c) in label.chars().enumerate() {
                row[start + i] = c;
            }
            leftmost = start;
        }
        age += u64::from(step);
        if age > span || age > span_secs as u64 {
            break;
        }
    }
    row.into_iter().collect()
}

/// The `(divisor, suffix)` every label on an axis stepping by `step` uses.
///
/// Chosen from the step rather than per label, so one axis speaks one unit: a
/// 30-second step reads `120s 90s 60s 30s`, never `2m 90s 1m 30s`.
fn age_unit(step: u32) -> (u64, char) {
    let step = u64::from(step.max(1));
    for (div, suffix) in [(86_400u64, 'd'), (3_600, 'h'), (60, 'm')] {
        if step % div == 0 {
            return (div, suffix);
        }
    }
    (1, 's')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::viz::ByteBase;

    #[test]
    fn nice_ceiling_decimal_uses_the_1_2_2p5_5_family() {
        // 37.42 over 5 divisions wants a step of 7.484; the decimal family's
        // next member is 10, so the top is 50. (A step of 8 would be smaller,
        // but 8 is a BINARY step — that is exactly the difference the two
        // families exist to express.)
        assert_eq!(nice_ceiling(37.42, 5, TickBase::Decimal), 50.0);
        assert_eq!(nice_ceiling(37.42, 5, TickBase::Binary), 40.0);
        // A full-scale percentage must land exactly on 100, not overshoot.
        assert_eq!(nice_ceiling(100.0, 5, TickBase::Decimal), 100.0);
        assert_eq!(nice_ceiling(95.0, 5, TickBase::Decimal), 100.0);
        // 4 divisions is where 2.5 earns its place: 100/4 = 25.
        assert_eq!(nice_ceiling(100.0, 4, TickBase::Decimal), 100.0);
        assert_eq!(nice_ceiling(1.0, 2, TickBase::Decimal), 1.0);
    }

    #[test]
    fn nice_ceiling_binary_steps_are_powers_of_two() {
        // 37.42 MB over 5 divisions → an 8 MiB step → a 40 MiB top.
        let top = nice_ceiling(37.42e6, 5, TickBase::Binary);
        assert_eq!(top, 5.0 * 8.0 * 1024.0 * 1024.0);
        // Every step is an exact power of two across a wide sweep.
        for mag in 0..30u32 {
            for divisions in 1..8usize {
                let max = 1.7f32 * 2f32.powi(mag as i32);
                let top = nice_ceiling(max, divisions, TickBase::Binary);
                let step = top / divisions as f32;
                let l = step.log2();
                assert!(
                    (l - l.round()).abs() < 1e-4,
                    "step {step} (mag {mag}, div {divisions}) is not a power of two"
                );
            }
        }
    }

    #[test]
    fn nice_ceiling_never_sits_below_the_data() {
        // The property the whole axis rests on: a rounded ceiling that clipped
        // the peak would draw a plot pinned at its top, silently.
        let mut seed = 0x9E3779B9u32;
        for _ in 0..10_000 {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            // Spread across ~20 orders of magnitude.
            let mant = (seed >> 8) as f32 / (1u32 << 24) as f32 * 9.0 + 1.0;
            let exp = ((seed % 41) as i32) - 20;
            let max = mant * 10f32.powi(exp);
            for divisions in 1..9usize {
                for base in [TickBase::Decimal, TickBase::Binary] {
                    let top = nice_ceiling(max, divisions, base);
                    assert!(
                        top >= max * (1.0 - 1e-5),
                        "{base:?} div={divisions} max={max} -> top={top}"
                    );
                    assert!(top.is_finite() && top > 0.0);
                }
            }
        }
    }

    #[test]
    fn nice_ceiling_degenerate_inputs_never_divide_by_nothing() {
        for base in [TickBase::Decimal, TickBase::Binary] {
            for max in [0.0_f32, -5.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
                // An all-idle window reads `4 3 2 1 0`, not a row of zeroes.
                assert_eq!(nice_ceiling(max, 4, base), 4.0, "{base:?} {max}");
                assert_eq!(nice_ceiling(max, 0, base), 1.0, "{base:?} {max}");
            }
            // A finite but enormous peak still yields a finite ceiling.
            assert!(nice_ceiling(f32::MAX, 5, base).is_finite());
        }
    }

    #[test]
    fn tick_base_follows_the_configured_byte_base() {
        let bin = UnitFmt::RAW;
        let dec = UnitFmt {
            bytes: ByteBase::Decimal,
            ..UnitFmt::RAW
        };
        assert_eq!(tick_base(Unit::Bytes, bin), TickBase::Binary);
        assert_eq!(tick_base(Unit::BytesPerSec, bin), TickBase::Binary);
        assert_eq!(tick_base(Unit::Bytes, dec), TickBase::Decimal);
        // A bit rate is decimal whatever the user does with byte totals.
        assert_eq!(tick_base(Unit::BitsPerSec, bin), TickBase::Decimal);
        assert_eq!(tick_base(Unit::Percent, bin), TickBase::Decimal);
    }

    #[test]
    fn y_gutter_labels_every_row_top_down_right_aligned() {
        let l = y_gutter(6, 100.0, Unit::Percent, UnitFmt::RAW);
        assert_eq!(l, vec!["100%", " 80%", " 60%", " 40%", " 20%", "  0%"]);
        // The old renderer blanked every row but three.
        assert!(l.iter().all(|s| !s.trim().is_empty()));
        assert_eq!(
            y_gutter(0, 100.0, Unit::Percent, UnitFmt::RAW),
            Vec::<String>::new()
        );
        // One row labels the top alone.
        assert_eq!(
            y_gutter(1, 100.0, Unit::Percent, UnitFmt::RAW),
            vec!["100%"]
        );
    }

    #[test]
    fn y_gutter_bottom_is_exactly_zero() {
        // Interpolating the bottom could leave a float crumb that prints as a
        // stray `1` or `-0` under the plot's baseline.
        for rows in 2..10usize {
            for top in [1.0_f32, 37.42, 1e9, 0.001] {
                let l = y_gutter(rows, top, Unit::Ratio, UnitFmt::RAW);
                assert_eq!(l.len(), rows);
                assert_eq!(l[rows - 1].trim(), "0.00", "rows={rows} top={top}");
            }
        }
    }

    #[test]
    fn y_gutter_stays_within_the_gutter_budget() {
        // Six cells is the host's `AXIS_W + 1`. A label past it would shove the
        // plot out of its box.
        for uf in [
            UnitFmt::RAW,
            UnitFmt {
                bytes: ByteBase::Decimal,
                temp: crate::viz::TempScale::Fahrenheit,
                freq: crate::viz::FreqMode::Ghz,
            },
        ] {
            for unit in Unit::ALL {
                for top in [0.0_f32, 1.0, 100.0, 37.42e6, 1e12] {
                    for rows in 1..10usize {
                        for s in y_gutter(rows, top, unit, uf) {
                            assert!(
                                s.chars().count() <= 5,
                                "{uf:?} {unit:?} top={top} rows={rows} -> {s:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn y_gutter_renders_a_byte_axis_in_round_multiples() {
        // The mockup axis: an 8 MiB step, every label an exact binary multiple.
        let top = nice_ceiling(37.42e6, 5, TickBase::Binary);
        let l = y_gutter(6, top, Unit::BytesPerSec, UnitFmt::RAW);
        assert_eq!(l, vec![" 40M", " 32M", " 24M", " 16M", "8.0M", "  0B"]);
    }

    #[test]
    fn y_gutter_log_inverts_the_log_map() {
        // A log gutter's rows are not evenly spaced in value; each must be the
        // exact inverse of the map that drew it.
        let l = y_gutter_log(3, 1e6, 1.0, Unit::Ratio, UnitFmt::RAW);
        assert_eq!(l.len(), 3);
        assert_eq!(l[2].trim(), "0.00");
        // Top row is the axis max.
        assert_eq!(l[0].trim(), Unit::Ratio.fmt(1e6).trim());
        // The middle row sits far below the linear midpoint — the whole reason
        // a log scale exists.
        let mid: f32 = l[1].trim().replace(',', "").parse().unwrap_or(0.0);
        assert!(mid < 1e5, "log midpoint should be well under 500k: {mid}");
    }

    #[test]
    fn time_axis_is_exactly_w_cells() {
        for w in 0..120usize {
            for span in [10.0_f32, 30.0, 120.0, 600.0, 3_600.0, 43_200.0, 86_400.0] {
                let row = time_axis(span, w);
                assert_eq!(row.chars().count(), w, "w={w} span={span} -> {row:?}");
                assert!(row.is_ascii(), "must stay ASCII: {row:?}");
            }
        }
    }

    #[test]
    fn time_axis_anchors_now_at_the_right_edge() {
        let row = time_axis(120.0, 60);
        assert!(row.ends_with("now"), "{row:?}");
        // And the oldest label is the span itself, at the left.
        assert!(row.trim_start().starts_with("120s"), "{row:?}");
    }

    #[test]
    fn time_axis_picks_readable_divisions() {
        // 12h over a wide plot steps in 3h, not 1h — MIN_TICK_CELLS is what
        // stops the row reading as a crowd.
        let row = time_axis(43_200.0, 90);
        for want in ["12h", "9h", "6h", "3h", "now"] {
            assert!(row.contains(want), "{want} missing from {row:?}");
        }
        assert!(!row.contains("11h"), "too dense: {row:?}");
        // Two minutes steps in 30s — and every label speaks seconds, so the row
        // never mixes units.
        let row = time_axis(120.0, 60);
        for want in ["120s", "90s", "60s", "30s", "now"] {
            assert!(row.contains(want), "{want} missing from {row:?}");
        }
        assert!(!row.contains('m'), "one axis, one unit: {row:?}");
    }

    #[test]
    fn time_axis_speaks_one_unit_per_row() {
        // A step that divides into hours labels in hours, and nothing else.
        let row = time_axis(43_200.0, 90);
        assert!(!row.contains('m') && !row.contains('s'), "{row:?}");
        // A sub-minute step labels in seconds, and nothing else.
        let row = time_axis(120.0, 60);
        assert!(!row.contains('h') && !row.contains('m'), "{row:?}");
    }

    #[test]
    fn time_axis_drops_the_oldest_label_when_crowded() {
        // Crowding must shed history, never the present — the same bias as the
        // plot's own right-alignment.
        let wide = time_axis(3_600.0, 100);
        let narrow = time_axis(3_600.0, 20);
        assert!(narrow.contains("now"), "{narrow:?}");
        let count = |s: &str| s.split_whitespace().count();
        assert!(
            count(&narrow) < count(&wide),
            "narrow={narrow:?} wide={wide:?}"
        );
        // Labels never overlap: every run is separated by at least one space.
        assert!(!narrow.contains("nownow"));
    }

    #[test]
    fn time_axis_degenerate_inputs_are_blank_not_wrong() {
        assert_eq!(time_axis(0.0, 20), " ".repeat(20));
        assert_eq!(time_axis(-5.0, 20), " ".repeat(20));
        assert_eq!(time_axis(f32::NAN, 20), " ".repeat(20));
        assert_eq!(time_axis(100.0, 0), "");
        // Too narrow for a scale: blank beats a lone misleading word.
        assert_eq!(time_axis(100.0, 5), " ".repeat(5));
    }

    #[test]
    fn age_unit_follows_the_step_not_the_label() {
        assert_eq!(age_unit(10), (1, 's'));
        assert_eq!(age_unit(30), (1, 's'));
        // 60 divides into minutes, so a 1-minute step labels in minutes.
        assert_eq!(age_unit(60), (60, 'm'));
        assert_eq!(age_unit(900), (60, 'm'));
        assert_eq!(age_unit(3_600), (3_600, 'h'));
        assert_eq!(age_unit(10_800), (3_600, 'h'));
        assert_eq!(age_unit(86_400), (86_400, 'd'));
        // A zero step must not divide by nothing.
        assert_eq!(age_unit(0), (1, 's'));
    }
}
