//! Resolving the `"auto"` calendar display settings.
//!
//! Pure: the caller passes the environment's locale string in rather than this
//! module reading `LC_TIME`, so the resolution is testable and the module stays
//! substrate-agnostic.

use chrono::Weekday;

/// Resolve `week_start`, where `None` means `"auto"`.
///
/// Auto reads the locale's region: most of the world starts the week on Monday
/// (ISO 8601), while the US, Canada, Japan and a handful of others start on
/// Sunday, and much of the Middle East on Saturday. An unrecognised or absent
/// locale falls back to Monday, matching ISO.
pub fn resolve_week_start(configured: Option<Weekday>, locale: Option<&str>) -> Weekday {
    if let Some(w) = configured {
        return w;
    }
    let Some(region) = locale_region(locale) else {
        return Weekday::Mon;
    };
    match region.as_str() {
        // Sunday-first regions.
        "US" | "CA" | "JP" | "IL" | "KR" | "TW" | "HK" | "MO" | "BR" | "MX" | "PH" | "ZA"
        | "CO" | "PE" | "VE" | "AR" | "CL" | "GT" | "DO" | "PR" | "NI" | "HN" | "SV" | "BO"
        | "PY" | "EC" | "PA" | "CR" | "JM" | "TH" | "ID" | "IN" | "PK" | "BD" | "AU" | "NZ" => {
            Weekday::Sun
        }
        // Saturday-first regions.
        "AE" | "AF" | "BH" | "DZ" | "EG" | "IQ" | "IR" | "JO" | "KW" | "LY" | "OM" | "QA"
        | "SA" | "SD" | "SY" | "YE" => Weekday::Sat,
        _ => Weekday::Mon,
    }
}

/// Resolve 12-vs-24-hour display, where `None` means `"auto"`.
///
/// Returns `true` for 12-hour. Auto follows the same region signal; the
/// English-speaking Americas plus a few others read 12-hour clocks, and most of
/// the rest of the world reads 24-hour. Falls back to 24-hour.
pub fn resolve_time_format(configured: Option<bool>, locale: Option<&str>) -> bool {
    if let Some(t) = configured {
        return t;
    }
    let Some(region) = locale_region(locale) else {
        return false;
    };
    matches!(
        region.as_str(),
        "US" | "CA"
            | "AU"
            | "NZ"
            | "PH"
            | "IN"
            | "PK"
            | "BD"
            | "EG"
            | "SA"
            | "CO"
            | "MX"
            | "MY"
            | "NG"
            | "IE"
            | "GB"
    )
}

/// Pull the region out of a POSIX locale string: `en_US.UTF-8` → `US`.
///
/// Handles the `_`/`-` separator and the `.CHARSET` / `@modifier` suffixes.
/// `C` and `POSIX` have no region and yield `None`.
fn locale_region(locale: Option<&str>) -> Option<String> {
    let raw = locale?.trim();
    if raw.is_empty() || raw == "C" || raw == "POSIX" {
        return None;
    }
    let head = raw.split(['.', '@']).next()?;
    let region = head.split(['_', '-']).nth(1)?;
    if region.is_empty() {
        return None;
    }
    Some(region.to_ascii_uppercase())
}
