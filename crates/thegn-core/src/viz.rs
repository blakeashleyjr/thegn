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

/// What a series' raw values mean, so [`UnitFmt`] can format them.
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
    /// Bytes/second **displayed as bits/second**, rendered `11M` (`bit/s`).
    ///
    /// The stored value is still bytes/sec — storage never changes units, so
    /// the recorder stays comparable across a config edit; the ×8 lives in
    /// [`UnitFmt::fmt`] and [`UnitFmt::factor`], which is what lets an axis
    /// ceiling chosen in display space be a round number of bits.
    BitsPerSec,
}

impl Unit {
    /// Every unit, for exhaustive tests.
    pub const ALL: [Unit; 8] = [
        Unit::Percent,
        Unit::Celsius,
        Unit::BytesPerSec,
        Unit::Bytes,
        Unit::Ratio,
        Unit::Megahertz,
        Unit::Watts,
        Unit::BitsPerSec,
    ];

    /// Format one raw value with the default presentation ([`UnitFmt::RAW`]:
    /// binary bytes, °C, auto frequency).
    ///
    /// Kept so callers with no configured context — the ones that predate
    /// `[monitor]`'s unit keys — read exactly as they always have.
    pub fn fmt(self, v: f32) -> String {
        UnitFmt::RAW.fmt(self, v)
    }

    /// Whether this unit is a rate, i.e. whether a headline reading wants a
    /// trailing `/s`. The distinction [`Unit::Bytes`] and [`Unit::BytesPerSec`]
    /// used to lack — both rendered `1.4M`, so a total and a rate were
    /// indistinguishable.
    pub fn is_rate(self) -> bool {
        matches!(
            self,
            Unit::BytesPerSec | Unit::BitsPerSec | Unit::Watts | Unit::Megahertz
        )
    }
}

/// Byte magnitudes: powers of 1024 or powers of 1000.
///
/// This picks the **divisor**, not the suffix — gutter labels stay single-letter
/// (`1.4M`) in both bases so they keep fitting the 5-cell budget, and
/// [`UnitFmt::note`] discloses which base is in force once, in the graph header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ByteBase {
    /// KiB/MiB/GiB — 1024. What thegn has always used.
    #[default]
    Binary,
    /// kB/MB/GB — 1000. What disk vendors and most network tooling print.
    Decimal,
}

/// Temperature presentation.
///
/// **Display only.** `[stats.alerts]` thresholds are compared against raw °C in
/// [`crate::resource_alert`] and must never be routed through here — an alert
/// that fires at a different temperature because a display preference changed
/// would be a genuine bug.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TempScale {
    #[default]
    Celsius,
    Fahrenheit,
}

/// CPU-frequency presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FreqMode {
    /// Pick the magnitude that fits — `800M`, `3.2G`.
    #[default]
    Auto,
    /// Always megahertz (`3200M`), falling back to [`FreqMode::Auto`] past the
    /// cell budget.
    Mhz,
    /// Always gigahertz (`3.2G`).
    Ghz,
}

/// How raw readings are rendered: the user's unit preferences, resolved once
/// from config and passed by value (it is 3 bytes and `Copy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UnitFmt {
    pub bytes: ByteBase,
    pub temp: TempScale,
    pub freq: FreqMode,
}

impl UnitFmt {
    /// The identity context — binary bytes, °C, auto frequency. Exactly what
    /// thegn rendered before the unit keys existed, so [`Unit::fmt`] delegating
    /// here is a no-op refactor (pinned by a test).
    pub const RAW: UnitFmt = UnitFmt {
        bytes: ByteBase::Binary,
        temp: TempScale::Celsius,
        freq: FreqMode::Auto,
    };

    /// Format one raw value for an axis gutter — deliberately terse (**≤5
    /// cells, guaranteed**), since the gutter steals width from the plot itself
    /// and a label that overflows would shove the plot out of its box.
    ///
    /// Every arm falls back to a compact SI form once the plain rendering would
    /// exceed the budget. That matters for genuinely unbounded readings — a
    /// per-core CPU sum can exceed 100%, a load average is unbounded — and it
    /// means no caller has to pre-clamp to keep the layout intact.
    pub fn fmt(self, unit: Unit, v: f32) -> String {
        if !v.is_finite() {
            return "—".into();
        }
        match unit {
            // 0–100 is the common case ("42%"), but a per-core sum is unbounded.
            Unit::Percent if v.abs() < 10_000.0 => format!("{v:.0}%"),
            Unit::Percent => si(v, 1000.0, &["%", "k%", "M%"]),
            Unit::Celsius => {
                let c = self.display_temp(v);
                if c.abs() < 10_000.0 {
                    format!("{c:.0}°")
                } else {
                    si(c, 1000.0, &["°", "k°"])
                }
            }
            // Single-letter suffixes in BOTH bases: only the divisor moves, so
            // the label keeps fitting and `note()` carries the disclosure.
            Unit::BytesPerSec | Unit::Bytes => {
                si(v, self.bytes.step(), &["B", "K", "M", "G", "T", "P"])
            }
            // Bit rates are decimal by universal convention (100 Mbps is 10^8
            // bits), regardless of how the user likes byte *totals* counted.
            Unit::BitsPerSec => si(v * 8.0, 1000.0, &["b", "K", "M", "G", "T", "P"]),
            Unit::Ratio if v.abs() < 100.0 => format!("{v:.2}"),
            Unit::Ratio if v.abs() < 100_000.0 => format!("{v:.0}"),
            Unit::Ratio => si(v, 1000.0, &["", "k", "M", "G"]),
            Unit::Megahertz => self.fmt_freq(v),
            Unit::Watts if v.abs() < 1000.0 => format!("{v:.0}W"),
            Unit::Watts => si(v, 1000.0, &["W", "kW", "MW"]),
        }
    }

    /// The linear factor from a raw reading to its displayed magnitude.
    ///
    /// Used to choose an axis ceiling **in display space**, so a bits/s axis
    /// gets round numbers of bits. `1.0` for [`Unit::Celsius`] even under °F:
    /// that conversion has an offset and is not expressible as a factor — see
    /// [`Self::display_temp`].
    pub fn factor(self, unit: Unit) -> f32 {
        match unit {
            Unit::BitsPerSec => 8.0,
            _ => 1.0,
        }
    }

    /// °C → the displayed temperature. The **one affine-with-offset**
    /// conversion here, which is why it cannot ride [`Self::factor`] and why an
    /// axis under °F has round ticks in °C (`212 176 140 104 68 32`) rather than
    /// in °F: a round-in-°F axis needs a non-zero bottom, and the plot's scale
    /// has no offset term.
    pub fn display_temp(self, c: f32) -> f32 {
        match self.temp {
            TempScale::Celsius => c,
            TempScale::Fahrenheit => c * 9.0 / 5.0 + 32.0,
        }
    }

    /// The unit family, for the graph header — the disclosure that tells a
    /// `1.4M` apart from a `1.4M`.
    pub fn note(self, unit: Unit) -> &'static str {
        match unit {
            Unit::Percent => "%",
            Unit::Celsius => match self.temp {
                TempScale::Celsius => "°C",
                TempScale::Fahrenheit => "°F",
            },
            Unit::Bytes => match self.bytes {
                ByteBase::Binary => "B (KiB)",
                ByteBase::Decimal => "B (kB)",
            },
            Unit::BytesPerSec => match self.bytes {
                ByteBase::Binary => "B/s (KiB)",
                ByteBase::Decimal => "B/s (kB)",
            },
            Unit::BitsPerSec => "bit/s",
            Unit::Ratio => "",
            Unit::Megahertz => match self.freq {
                FreqMode::Ghz => "GHz",
                _ => "MHz",
            },
            Unit::Watts => "W",
        }
    }

    /// The suffix a **headline** readout carries, where there is room the gutter
    /// does not have. Empty for a level.
    pub fn rate_suffix(self, unit: Unit) -> &'static str {
        match unit {
            Unit::BytesPerSec | Unit::BitsPerSec => "/s",
            _ => "",
        }
    }

    fn fmt_freq(self, mhz: f32) -> String {
        match self.freq {
            // `si` on hertz, exactly as before the mode existed.
            FreqMode::Auto => si(mhz * 1e6, 1000.0, &["Hz", "k", "M", "G", "T"]),
            // Past 9999 MHz the plain form is 6 cells; fall back rather than
            // overflow the gutter.
            FreqMode::Mhz if mhz.abs() < 10_000.0 => format!("{mhz:.0}M"),
            FreqMode::Mhz => si(mhz * 1e6, 1000.0, &["Hz", "k", "M", "G", "T"]),
            FreqMode::Ghz => {
                let g = (mhz / 1000.0).abs();
                if g < 10.0 {
                    format!("{g:.1}G")
                } else if g < 10_000.0 {
                    format!("{g:.0}G")
                } else {
                    si(mhz * 1e6, 1000.0, &["Hz", "k", "M", "G", "T"])
                }
            }
        }
    }
}

impl ByteBase {
    /// The divisor between magnitudes.
    pub fn step(self) -> f32 {
        match self {
            ByteBase::Binary => 1024.0,
            ByteBase::Decimal => 1000.0,
        }
    }
}

/// Divide by `step` until the value fits, tagging with a suffix.
///
/// **Suffixes must be at most two cells.** The widest rendering is three digits
/// plus a suffix, so a three-character suffix (the `kHz` this table used to
/// carry) silently costs six cells — one past the budget.
///
/// The loop threshold stays **1000 even when `step` is 1024**, on purpose: it is
/// what caps the post-loop mantissa at three digits, so a two-character suffix
/// still fits the five-cell budget. Raising it to the divisor would admit
/// `1023Kb`.
fn si(v: f32, step: f32, suffixes: &[&str]) -> String {
    let mut v = v.abs();
    let mut i = 0;
    // Every threshold here is the ROUNDED boundary, not the arithmetic one:
    // `{:.0}` turns 999.6 into "1000" and `{:.1}` turns 9.999 into "10.0", each
    // one cell wider than a naive `>= 1000` / `< 10` test assumes. That is how
    // `Watts.fmt(9999.0)` used to render "10.0kW" — six cells, one past the
    // gutter budget the doc comment promises.
    while v >= 999.5 && i + 1 < suffixes.len() {
        v /= step;
        i += 1;
    }
    if i == 0 {
        format!("{v:.0}{}", suffixes[0])
    } else if v < 9.95 {
        format!("{v:.1}{}", suffixes[i])
    } else if v < 999.5 {
        format!("{v:.0}{}", suffixes[i])
    } else {
        // Past the largest suffix — clamp rather than overflow the gutter.
        format!("999{}", suffixes[suffixes.len() - 1])
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
        for u in Unit::ALL {
            for v in [0.0_f32, 1.0, 999.0, 1e6, 1e12] {
                assert!(u.fmt(v).chars().count() <= 5, "{u:?} {v} -> {}", u.fmt(v));
            }
        }
    }

    /// Every context a user can configure.
    fn every_context() -> Vec<UnitFmt> {
        let mut out = Vec::new();
        for bytes in [ByteBase::Binary, ByteBase::Decimal] {
            for temp in [TempScale::Celsius, TempScale::Fahrenheit] {
                for freq in [FreqMode::Auto, FreqMode::Mhz, FreqMode::Ghz] {
                    out.push(UnitFmt { bytes, temp, freq });
                }
            }
        }
        out
    }

    #[test]
    fn unit_fmt_stays_within_five_cells_in_every_context() {
        // The gutter budget is now a 48-way product (8 units × 2 bases × 2 temp
        // scales × 3 frequency modes). This property is the only thing standing
        // between a new suffix and a plot shoved out of its box — `AXIS_W` in the
        // host is sized against it.
        for uf in every_context() {
            for u in Unit::ALL {
                for v in [
                    0.0_f32,
                    -1.0,
                    1.0,
                    9.99,
                    999.0,
                    1000.0,
                    9999.0,
                    1e5,
                    1e6,
                    1e9,
                    1e12,
                    1e15,
                    f32::MAX,
                ] {
                    let s = uf.fmt(u, v);
                    assert!(
                        s.chars().count() <= 5,
                        "{uf:?} {u:?} {v} -> {s:?} ({} cells)",
                        s.chars().count()
                    );
                }
                // A missing reading is a dash, never a number.
                for v in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
                    assert_eq!(uf.fmt(u, v), "—", "{uf:?} {u:?} {v}");
                }
            }
        }
    }

    #[test]
    fn unit_fmt_default_is_todays_output() {
        // Pins the refactor as a no-op: `Unit::fmt` delegating to `UnitFmt::RAW`
        // must not shift a single character.
        for u in Unit::ALL {
            for v in [0.0_f32, 1.0, 42.4, 61.2, 512.0, 3200.0, 1e6, 1e9, 1e12] {
                assert_eq!(UnitFmt::RAW.fmt(u, v), u.fmt(v), "{u:?} {v}");
            }
        }
        assert_eq!(UnitFmt::default(), UnitFmt::RAW);
    }

    #[test]
    fn unit_fmt_byte_bases_differ_only_in_the_divisor() {
        let bin = UnitFmt {
            bytes: ByteBase::Binary,
            ..UnitFmt::RAW
        };
        let dec = UnitFmt {
            bytes: ByteBase::Decimal,
            ..UnitFmt::RAW
        };
        // 2 MiB is exactly 2.0M binary, and a shade over 2.0M decimal.
        let two_mib = 2.0 * 1024.0 * 1024.0;
        assert_eq!(bin.fmt(Unit::Bytes, two_mib), "2.0M");
        assert_eq!(dec.fmt(Unit::Bytes, two_mib), "2.1M");
        // 2 MB is 2.0M decimal, a shade under binary.
        assert_eq!(dec.fmt(Unit::Bytes, 2e6), "2.0M");
        assert_eq!(bin.fmt(Unit::Bytes, 2e6), "1.9M");
        // The base must not leak into anything that isn't a byte count.
        for u in [Unit::Percent, Unit::Watts, Unit::Ratio, Unit::Megahertz] {
            assert_eq!(bin.fmt(u, 1500.0), dec.fmt(u, 1500.0), "{u:?}");
        }
    }

    #[test]
    fn bits_are_eight_times_bytes_and_always_decimal() {
        // 1 MiB/s of bytes is 8.4 Mbit/s — decimal magnitudes, by convention,
        // regardless of how the user counts byte totals.
        let mib = 1024.0 * 1024.0;
        assert_eq!(UnitFmt::RAW.fmt(Unit::BitsPerSec, mib), "8.4M");
        assert_eq!(UnitFmt::RAW.fmt(Unit::BytesPerSec, mib), "1.0M");
        // The base does not move a bit rate.
        let dec = UnitFmt {
            bytes: ByteBase::Decimal,
            ..UnitFmt::RAW
        };
        assert_eq!(dec.fmt(Unit::BitsPerSec, mib), "8.4M");
        assert_eq!(UnitFmt::RAW.factor(Unit::BitsPerSec), 8.0);
        assert_eq!(UnitFmt::RAW.factor(Unit::BytesPerSec), 1.0);
    }

    #[test]
    fn fahrenheit_converts_at_display_time_only() {
        let f = UnitFmt {
            temp: TempScale::Fahrenheit,
            ..UnitFmt::RAW
        };
        assert_eq!(f.fmt(Unit::Celsius, 100.0), "212°");
        assert_eq!(f.fmt(Unit::Celsius, 0.0), "32°");
        assert_eq!(f.fmt(Unit::Celsius, 61.0), "142°");
        assert_eq!(UnitFmt::RAW.fmt(Unit::Celsius, 61.0), "61°");
        // The offset is why this is NOT a `factor` — a factor would map 0 °C to
        // 0 °F and quietly move every reading.
        assert_eq!(f.factor(Unit::Celsius), 1.0);
        assert_eq!(f.display_temp(100.0), 212.0);
        assert_eq!(f.note(Unit::Celsius), "°F");
        assert_eq!(UnitFmt::RAW.note(Unit::Celsius), "°C");
    }

    #[test]
    fn frequency_modes_render_and_fall_back() {
        let mk = |freq| UnitFmt {
            freq,
            ..UnitFmt::RAW
        };
        assert_eq!(mk(FreqMode::Auto).fmt(Unit::Megahertz, 3200.0), "3.2G");
        assert_eq!(mk(FreqMode::Auto).fmt(Unit::Megahertz, 800.0), "800M");
        assert_eq!(mk(FreqMode::Mhz).fmt(Unit::Megahertz, 3200.0), "3200M");
        assert_eq!(mk(FreqMode::Mhz).fmt(Unit::Megahertz, 800.0), "800M");
        assert_eq!(mk(FreqMode::Ghz).fmt(Unit::Megahertz, 3200.0), "3.2G");
        assert_eq!(mk(FreqMode::Ghz).fmt(Unit::Megahertz, 800.0), "0.8G");
        // Past the cell budget, `mhz` falls back rather than overflowing.
        assert_eq!(mk(FreqMode::Mhz).fmt(Unit::Megahertz, 12_000.0), "12G");
        assert_eq!(mk(FreqMode::Ghz).note(Unit::Megahertz), "GHz");
        assert_eq!(mk(FreqMode::Mhz).note(Unit::Megahertz), "MHz");
    }

    #[test]
    fn notes_tell_a_rate_apart_from_a_total() {
        // The ambiguity this whole context exists to kill: `Bytes` and
        // `BytesPerSec` render identically, so the header has to disclose.
        let v = 1.4 * 1024.0 * 1024.0;
        assert_eq!(
            UnitFmt::RAW.fmt(Unit::Bytes, v),
            UnitFmt::RAW.fmt(Unit::BytesPerSec, v)
        );
        assert_eq!(UnitFmt::RAW.note(Unit::Bytes), "B (KiB)");
        assert_eq!(UnitFmt::RAW.note(Unit::BytesPerSec), "B/s (KiB)");
        let dec = UnitFmt {
            bytes: ByteBase::Decimal,
            ..UnitFmt::RAW
        };
        assert_eq!(dec.note(Unit::Bytes), "B (kB)");
        assert_eq!(dec.note(Unit::BytesPerSec), "B/s (kB)");
        assert_eq!(UnitFmt::RAW.note(Unit::BitsPerSec), "bit/s");
        // And the headline carries `/s` where the gutter has no room for it.
        assert_eq!(UnitFmt::RAW.rate_suffix(Unit::BytesPerSec), "/s");
        assert_eq!(UnitFmt::RAW.rate_suffix(Unit::BitsPerSec), "/s");
        assert_eq!(UnitFmt::RAW.rate_suffix(Unit::Bytes), "");
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
