//! Width-aware formatting for system-monitor readings.
//!
//! This is separate from [`crate::viz::UnitFmt`], which formats graph gutters.
//! The latter stays intentionally terse; this module pads every reading to an
//! exact terminal-cell width.

use crate::{termcaps, viz};
use unicode_width::UnicodeWidthStr;

/// A monitor reading's presentation unit. This is distinct from
/// [`viz::Unit`], which describes graph-series storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Percent,
    CpuCoreSum,
    Bytes,
    BitsPerSec,
    Count,
    Ratio,
    Hertz,
    Watts,
    WattHours,
    Volts,
    Amps,
    Rpm,
    Temp,
    Duration,
    Age,
    TimePlus,
}

impl Unit {
    pub const ALL: [Unit; 16] = [
        Unit::Percent,
        Unit::CpuCoreSum,
        Unit::Bytes,
        Unit::BitsPerSec,
        Unit::Count,
        Unit::Ratio,
        Unit::Hertz,
        Unit::Watts,
        Unit::WattHours,
        Unit::Volts,
        Unit::Amps,
        Unit::Rpm,
        Unit::Temp,
        Unit::Duration,
        Unit::Age,
        Unit::TimePlus,
    ];

    pub const fn default_width(self) -> usize {
        match self {
            Unit::Percent | Unit::Ratio | Unit::Age => 4,
            Unit::CpuCoreSum
            | Unit::Bytes
            | Unit::BitsPerSec
            | Unit::Count
            | Unit::Hertz
            | Unit::Watts
            | Unit::Amps
            | Unit::Temp
            | Unit::Duration => 5,
            Unit::WattHours | Unit::Volts => 6,
            Unit::Rpm => 7,
            Unit::TimePlus => 8,
        }
    }
}

/// The semantic state of a reading, keeping absent values distinct from zero.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Reading {
    Value(f64),
    Unknown,
    Warming,
    Denied,
    NotApplicable,
}

/// Terminal marks used for non-numeric readings and the dot zero style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Marks {
    pub unknown: &'static str,
    pub warming: &'static str,
    pub zero: &'static str,
    pub denied: &'static str,
}

impl Marks {
    pub const fn from_glyphs(set: &'static termcaps::GlyphSet, denied: &'static str) -> Marks {
        Marks {
            unknown: set.emdash,
            warming: set.ellipsis,
            zero: set.middot,
            denied,
        }
    }

    /// The denied fallback is `?` until the host's `Glyph::Lock` is available.
    pub const UNICODE: Marks = Marks::from_glyphs(&termcaps::UNICODE, "?");
    /// ASCII marks are `-`, `...`, and `-`; denied remains `?`.
    pub const ASCII: Marks = Marks::from_glyphs(&termcaps::ASCII, "?");
}

/// How numeric zero values are represented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ZeroStyle {
    #[default]
    Digit,
    Dot,
    Blank,
}

/// Formatting preferences and marks for one monitor surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NumFmt {
    pub units: viz::UnitFmt,
    pub zero: ZeroStyle,
    pub marks: Marks,
}

impl Default for NumFmt {
    fn default() -> Self {
        Self {
            units: viz::UnitFmt::RAW,
            zero: ZeroStyle::Digit,
            marks: Marks::UNICODE,
        }
    }
}

impl NumFmt {
    pub fn fmt(&self, value: f64, unit: Unit, width: usize) -> String {
        self.reading(Reading::Value(value), unit, width)
    }

    pub fn reading(&self, reading: Reading, unit: Unit, width: usize) -> String {
        if width == 0 {
            return String::new();
        }
        match reading {
            Reading::Value(value) if !value.is_finite() => fit_mark(self.marks.unknown, width),
            Reading::Value(value) if value.abs() < f64::MIN_POSITIVE => match self.zero {
                ZeroStyle::Digit => fit(self.zero_value(unit), width),
                ZeroStyle::Dot => fit_mark(self.marks.zero, width),
                ZeroStyle::Blank => " ".repeat(width),
            },
            Reading::Value(value)
                if matches!(unit, Unit::Duration | Unit::Age | Unit::TimePlus) && value < 0.0 =>
            {
                fit_mark(self.marks.unknown, width)
            }
            Reading::Value(value) => fit(self.format_value(value, unit, width), width),
            Reading::Unknown => fit_mark(self.marks.unknown, width),
            Reading::Warming => fit_mark(self.marks.warming, width),
            Reading::Denied => fit_mark(self.marks.denied, width),
            Reading::NotApplicable => " ".repeat(width),
        }
    }

    fn zero_value(&self, unit: Unit) -> String {
        match unit {
            Unit::Percent => "0%".into(),
            Unit::CpuCoreSum => "0.0".into(),
            Unit::Bytes => "0B".into(),
            Unit::BitsPerSec => "0.0b".into(),
            Unit::Count => "0".into(),
            Unit::Ratio => "0.00".into(),
            Unit::Hertz => "0.0Hz".into(),
            Unit::Watts => "0.0W".into(),
            Unit::WattHours => "0.0Wh".into(),
            Unit::Volts => "0.000V".into(),
            Unit::Amps => "0.00A".into(),
            Unit::Rpm => "0rpm".into(),
            Unit::Temp => "0°".into(), // ° has no GlyphSet field yet.
            Unit::Duration => "0s".into(),
            Unit::Age => "0s".into(),
            Unit::TimePlus => "0:00.00".into(),
        }
    }

    fn format_value(&self, value: f64, unit: Unit, width: usize) -> String {
        match unit {
            Unit::Duration => duration(value, width),
            Unit::Age => age(value, width),
            Unit::TimePlus => timeplus(value, width),
            Unit::Percent => percent(value, width),
            Unit::CpuCoreSum => cpu_sum(value, width),
            Unit::Bytes => bytes(value, self.units.bytes.step() as f64, width),
            Unit::BitsPerSec => scaled(
                value * 8.0,
                1000.0,
                &["b", "K", "M", "G", "T"],
                &[1, 0],
                width,
            ),
            Unit::Count => count(value, width),
            Unit::Ratio => scaled(value, 1000.0, &["", "k", "M", "G", "T"], &[2, 1, 0], width),
            // Hertz is in raw Hz; FreqMode is intentionally ignored here.
            Unit::Hertz => scaled(
                value,
                1000.0,
                &["Hz", "k", "M", "G", "T"],
                &[2, 1, 0],
                width,
            ),
            Unit::Watts => scaled(value, 1000.0, &["W", "kW", "MW"], &[2, 1, 0], width),
            Unit::WattHours => scaled(value, 1000.0, &["Wh", "kWh", "MWh"], &[2, 1, 0], width),
            Unit::Volts => scaled(value, 1000.0, &["V", "kV"], &[3, 2, 1, 0], width),
            Unit::Amps => scaled(value, 1000.0, &["A", "kA"], &[2, 1, 0], width),
            Unit::Rpm => rpm(value, width),
            Unit::Temp => temperature(value, self.units.temp, width),
        }
    }
}

/// Format with raw preferences, digit zero, and Unicode marks.
pub fn fmt(value: f64, unit: Unit, width: usize) -> String {
    NumFmt::default().fmt(value, unit, width)
}

fn cells(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

fn fit(candidate: String, width: usize) -> String {
    let used = cells(&candidate);
    if used > width {
        return "#".repeat(width);
    }
    format!("{}{}", " ".repeat(width - used), candidate)
}

fn fit_mark(mark: &str, width: usize) -> String {
    let mut clipped = String::new();
    for c in mark.chars() {
        let mut next = clipped.clone();
        next.push(c);
        if cells(&next) > width {
            break;
        }
        clipped.push(c);
    }
    format!("{}{}", " ".repeat(width - cells(&clipped)), clipped)
}

fn scaled(value: f64, divisor: f64, suffixes: &[&str], decimals: &[usize], width: usize) -> String {
    if !value.is_finite() {
        return "#".repeat(width);
    }
    let sign = if value.is_sign_negative() { "-" } else { "" };
    let mut magnitude = value.abs();
    let mut scale = 0;
    while magnitude >= 1000.0 && scale + 1 < suffixes.len() {
        magnitude /= divisor;
        scale += 1;
    }
    for current in scale..suffixes.len() {
        let scaled = value.abs() / divisor.powi(current as i32);
        for places in decimals {
            let mantissa = format!("{scaled:.precision$}", precision = *places);
            let rounded = mantissa.parse::<f64>().unwrap_or(f64::INFINITY);
            if rounded >= 1000.0 && current + 1 < suffixes.len() {
                break;
            }
            let candidate = format!("{sign}{mantissa}{}", suffixes[current]);
            if cells(&candidate) <= width {
                return candidate;
            }
        }
    }
    if scale + 1 == suffixes.len() {
        let scaled = value.abs() / divisor.powi(scale as i32);
        let candidate = format!("{sign}{scaled:.0}{}", suffixes[scale]);
        if cells(&candidate) <= width {
            return candidate;
        }
    }
    "#".repeat(width)
}

fn bytes(value: f64, divisor: f64, width: usize) -> String {
    let suffixes = ["B", "K", "M", "G", "T", "P", "E"];
    let mut magnitude = value.abs();
    let mut scale = 0;
    while magnitude >= 1000.0 && scale + 1 < suffixes.len() {
        magnitude /= divisor;
        scale += 1;
    }
    let sign = if value.is_sign_negative() { "-" } else { "" };
    for current in scale..suffixes.len() {
        let mantissa_value = value.abs() / divisor.powi(current as i32);
        let decimals: &[usize] = if mantissa_value >= 100.0 {
            &[0]
        } else if mantissa_value >= 10.0 || mantissa_value.fract() == 0.0 {
            &[1, 0]
        } else {
            &[2, 1, 0]
        };
        for places in decimals {
            let mantissa = format!("{mantissa_value:.precision$}", precision = *places);
            if mantissa.parse::<f64>().unwrap_or(f64::INFINITY) >= 1000.0
                && current + 1 < suffixes.len()
            {
                break;
            }
            let candidate = format!("{sign}{mantissa}{}", suffixes[current]);
            if cells(&candidate) <= width {
                return candidate;
            }
        }
    }
    "#".repeat(width)
}

fn percent(v: f64, width: usize) -> String {
    let mut candidate = format!("{v:.0}%");
    if cells(&candidate) <= width {
        return candidate;
    }
    candidate = format!("{v:.0}");
    if cells(&candidate) <= width {
        return candidate;
    }
    scaled(v, 1000.0, &["%", "k"], &[0], width)
}

fn cpu_sum(v: f64, width: usize) -> String {
    let decimal = format!("{v:.1}");
    if cells(&decimal) <= width {
        return decimal;
    }
    if v.abs() < 10_000.0 {
        let integer = format!("{v:.0}");
        if cells(&integer) <= width {
            return integer;
        }
    }
    scaled(v, 1000.0, &["", "k", "M", "G"], &[0], width)
}

fn count(v: f64, width: usize) -> String {
    if v.abs() < 10.0 && v.fract() != 0.0 {
        let candidate = format!("{v:.1}");
        if cells(&candidate) <= width {
            return candidate;
        }
    }
    if v.abs() < 10_000.0 {
        let candidate = format!("{v:.0}");
        if cells(&candidate) <= width {
            return candidate;
        }
    }
    scaled(v, 1000.0, &["", "k", "M", "G", "T"], &[0], width)
}

fn temperature(v: f64, scale: viz::TempScale, width: usize) -> String {
    let shown = match scale {
        viz::TempScale::Celsius => v,
        viz::TempScale::Fahrenheit => v * 9.0 / 5.0 + 32.0,
    };
    if !shown.is_finite() {
        return "#".repeat(width);
    }
    for candidate in [format!("{shown:.1}°"), format!("{shown:.0}°")] {
        if cells(&candidate) <= width {
            return candidate;
        }
    }
    "#".repeat(width)
}

fn rpm(v: f64, width: usize) -> String {
    let full = format!("{v:.0}rpm");
    if cells(&full) <= width {
        return full;
    }
    scaled(v, 1000.0, &["", "k", "M", "G"], &[1, 0], width)
}

fn duration(v: f64, width: usize) -> String {
    let n = v.floor() as u64;
    let days = n / 86_400;
    let hours = n / 3_600 % 24;
    let minutes = n / 60 % 60;
    let seconds = n % 60;
    let full = if days >= 100 {
        format!("{}d", days)
    } else if days >= 7 {
        format!("{}w", days / 7)
    } else if days > 0 {
        format!("{}d{:02}h", days, hours)
    } else if n >= 3_600 {
        format!("{}h{:02}m", n / 3_600, minutes)
    } else if n >= 60 {
        format!("{}m{:02}", n / 60, seconds)
    } else {
        format!("{}s", n)
    };
    if cells(&full) <= width {
        return full;
    }
    let compact = if days > 0 {
        format!("{}d{:02}", days, hours)
    } else if n >= 3_600 {
        format!("{}h{:02}", n / 3_600, minutes)
    } else if n >= 60 {
        format!("{}m{:02}", n / 60, seconds)
    } else {
        format!("{}s", n)
    };
    if cells(&compact) <= width {
        return compact;
    }
    if n >= 60 && n < 3_600 {
        let short_minutes = format!("{}m", n / 60);
        if cells(&short_minutes) <= width {
            return short_minutes;
        }
    }
    compact
}

fn age(v: f64, width: usize) -> String {
    let n = v.floor() as u64;
    let (amount, suffix) = if n >= 604_800 {
        (n / 604_800, "w")
    } else if n >= 86_400 {
        (n / 86_400, "d")
    } else if n >= 3_600 {
        (n / 3_600, "h")
    } else if n >= 60 {
        (n / 60, "m")
    } else {
        (n, "s")
    };
    let rendered = format!("{amount}{suffix}");
    if cells(&rendered) <= width {
        rendered
    } else {
        "#".repeat(width)
    }
}

fn timeplus(v: f64, width: usize) -> String {
    let n = v.floor() as u64;
    let candidate = if n < 3_600 {
        let cs = (v * 100.0).floor() as u64 % 100;
        format!("{}:{:02}.{:02}", n / 60, n % 60, cs)
    } else if n < 360_000 {
        format!("{}h{:02}:{:02}", n / 3_600, n / 60 % 60, n % 60)
    } else {
        format!("{}d{:02}h", n / 86_400, n / 3_600 % 24)
    };
    if cells(&candidate) <= width {
        candidate
    } else {
        "#".repeat(width)
    }
}

#[cfg(test)]
mod tests {
    use super::{Marks, NumFmt, Reading, Unit, ZeroStyle, fmt};
    use crate::{termcaps, viz};
    use unicode_width::UnicodeWidthStr;

    fn formatter(marks: Marks, zero: ZeroStyle) -> NumFmt {
        NumFmt {
            units: viz::UnitFmt::RAW,
            zero,
            marks,
        }
    }

    #[test]
    fn every_unit_and_width_is_exactly_that_wide() {
        let mut values = vec![
            0.0,
            -0.0,
            f64::from_bits(1),
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MAX,
        ];
        for exponent in -3..=15 {
            let decade = 10_f64.powi(exponent);
            for factor in [1.0, 0.9995, 0.95, 9.995, 99.95, 999.5, 1023.5] {
                values.push(decade * factor);
            }
        }
        let negatives: Vec<f64> = values
            .iter()
            .copied()
            .filter(|value| value.is_finite())
            .map(|value| -value)
            .collect();
        values.extend(negatives);
        for unit in Unit::ALL {
            for width in 0..=8 {
                for &value in &values {
                    let out = fmt(value, unit, width);
                    assert_eq!(
                        UnicodeWidthStr::width(out.as_str()),
                        width,
                        "{unit:?} {value:?} w{width}: {out:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn readings_and_marks_preserve_exact_width() {
        let readings = [
            Reading::Unknown,
            Reading::Warming,
            Reading::Denied,
            Reading::NotApplicable,
            Reading::Value(1.0),
            Reading::Value(0.0),
            Reading::Value(f64::NAN),
        ];
        for marks in [Marks::UNICODE, Marks::ASCII] {
            for width in 0..=8 {
                for unit in Unit::ALL {
                    for reading in readings {
                        assert_eq!(
                            UnicodeWidthStr::width(
                                formatter(marks, ZeroStyle::Digit)
                                    .reading(reading, unit, width)
                                    .as_str()
                            ),
                            width,
                            "{unit:?} {reading:?} w{width}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn rounding_promotes_on_rendered_mantissa() {
        let decimal = NumFmt {
            units: viz::UnitFmt {
                bytes: viz::ByteBase::Decimal,
                ..viz::UnitFmt::RAW
            },
            ..NumFmt::default()
        };
        assert_eq!(decimal.fmt(999_500.0, Unit::Bytes, 4), "1.0M");
        assert_eq!(decimal.fmt(999_500.0, Unit::Bytes, 5), "1.00M");
        let binary = NumFmt {
            units: viz::UnitFmt {
                bytes: viz::ByteBase::Binary,
                ..viz::UnitFmt::RAW
            },
            ..NumFmt::default()
        };
        assert_eq!(binary.fmt(999.5 * 1024.0, Unit::Bytes, 4), "1.0M");
        assert_eq!(fmt(9.9951, Unit::Watts, 5), "10.0W");
        assert_eq!(fmt(99.96, Unit::Watts, 5), " 100W");
        assert_eq!(fmt(99.96, Unit::Temp, 5), " 100°");
    }

    #[test]
    fn default_widths_and_byte_base_are_stable() {
        let expected = [4, 5, 5, 5, 5, 4, 5, 5, 6, 6, 5, 7, 5, 5, 4, 8];
        for (unit, width) in Unit::ALL.into_iter().zip(expected) {
            assert_eq!(unit.default_width(), width, "{unit:?}");
        }
        let decimal = NumFmt {
            units: viz::UnitFmt {
                bytes: viz::ByteBase::Decimal,
                ..viz::UnitFmt::RAW
            },
            ..NumFmt::default()
        };
        assert_eq!(decimal.fmt(1_000_000.0, Unit::Bytes, 5), "1.0M");
        assert_eq!(fmt(1_000_000.0, Unit::Bytes, 5), " 977K");
    }

    #[test]
    fn golden_units_and_preferences() {
        assert_eq!(fmt(7.0, Unit::Percent, 4), "  7%");
        assert_eq!(fmt(912.0, Unit::CpuCoreSum, 5), "912.0");
        assert_eq!(fmt(912.0, Unit::CpuCoreSum, 4), " 912");
        assert_eq!(fmt(1_610_612_736.0, Unit::Bytes, 5), "1.50G");
        assert_eq!(fmt(48_000.0, Unit::Count, 5), "48k");
        assert_eq!(fmt(1.42, Unit::Ratio, 4), "1.42");
        assert_eq!(fmt(772_000_000.0, Unit::Hertz, 5), "772M");
        assert_eq!(fmt(5_160_000_000.0, Unit::Hertz, 5), "5.16G");
        assert_eq!(fmt(4.82, Unit::Watts, 5), "4.82W");
        assert_eq!(fmt(53.6, Unit::WattHours, 6), "53.6Wh");
        assert_eq!(fmt(17.47, Unit::Volts, 6), "17.47V");
        assert_eq!(fmt(4.90, Unit::Amps, 5), "4.90A");
        assert_eq!(fmt(6182.0, Unit::Rpm, 7), "6182rpm");
        assert_eq!(fmt(6182.0, Unit::Rpm, 4), "6.2k");
        assert_eq!(fmt(90.5, Unit::Temp, 5), "90.5°");
        assert_eq!(fmt(90.5, Unit::Temp, 4), " 91°");
        assert_eq!(fmt(45.0, Unit::Duration, 5), "  45s");
        assert_eq!(fmt(243.0, Unit::Duration, 4), "4m03");
        assert_eq!(fmt(720.0, Unit::Duration, 4), "12m");
        assert_eq!(fmt(86_400.0 * 3.0 + 14_400.0, Unit::Duration, 5), "3d04h");
        assert_eq!(fmt(86_400.0 * 123.0, Unit::Duration, 5), "123d");
        assert_eq!(fmt(86_400.0 * 98.0, Unit::Duration, 5), "14w");
        assert_eq!(fmt(12.0, Unit::Age, 4), "12s");
        let fahrenheit = NumFmt {
            units: viz::UnitFmt {
                temp: viz::TempScale::Fahrenheit,
                ..viz::UnitFmt::RAW
            },
            ..NumFmt::default()
        };
        assert_eq!(fahrenheit.fmt(100.0, Unit::Temp, 5), " 212°");
        assert_eq!(fmt(4096.0, Unit::Bytes, 5), "4.0K");
    }

    #[test]
    fn states_zero_styles_and_marks() {
        assert_eq!(fmt(f64::NAN, Unit::Bytes, 5), "    —");
        assert_eq!(
            formatter(Marks::ASCII, ZeroStyle::Digit).fmt(f64::INFINITY, Unit::Bytes, 5),
            "    -"
        );
        assert_eq!(fmt(100.0, Unit::Percent, 2), "##");
        assert_eq!(
            formatter(Marks::UNICODE, ZeroStyle::Dot).fmt(0.0, Unit::Count, 3),
            "  ·"
        );
        assert_eq!(
            formatter(Marks::UNICODE, ZeroStyle::Blank).fmt(-0.0, Unit::Count, 3),
            "   "
        );
        assert_eq!(fmt(0.0, Unit::Bytes, 3), " 0B");
        assert_eq!(
            formatter(Marks::ASCII, ZeroStyle::Digit).reading(Reading::Unknown, Unit::Bytes, 3),
            "  -"
        );
        assert_eq!(
            formatter(Marks::ASCII, ZeroStyle::Digit).reading(Reading::Warming, Unit::Bytes, 2),
            ".."
        );
        assert_eq!(Marks::from_glyphs(&termcaps::ASCII, "LOCK").denied, "LOCK");
    }

    #[test]
    fn durations_floor_and_timeplus_forms() {
        assert_eq!(fmt(59.9, Unit::Duration, 5), "  59s");
        assert_eq!(fmt(3599.0, Unit::Duration, 5), "59m59");
        assert_eq!(fmt(43_533.0, Unit::TimePlus, 8), "12h05:33");
        assert_eq!(fmt(5.12, Unit::TimePlus, 8), " 0:05.12");
        assert_eq!(fmt(360_000.0, Unit::TimePlus, 8), "   4d04h");
    }
}
