//! Resolving the `"auto"` calendar display settings.
//!
//! Pure: the caller passes the environment's locale string in rather than this
//! module reading `LC_TIME`, so the resolution is testable and the module stays
//! substrate-agnostic.
//!
//! Normalisation is shared with weather via [`crate::locale_tag`]. Territory
//! data is CLDR 48 (ICU 78.3), generated mechanically by querying ICU's
//! first-day and default-hour-cycle APIs for every ISO region. The data keys on
//! territory only (language-specific exceptions are not modelled). Fallback
//! when the locale is absent, `C`/`POSIX`, unparseable, or has no territory:
//! Monday-first and 24-hour (ISO 8601). A Unicode extension in the locale
//! (`-u-fw-`, `-u-hc-`) outranks the table; explicit configuration outranks
//! everything.

use chrono::Weekday;

use crate::locale_tag::{self, LocaleTag};

/// CLDR 48 territories whose first day is Sunday.
const SUNDAY_FIRST: &[&str] = &[
    "AG", "AS", "BD", "BR", "BS", "BT", "BW", "BZ", "CA", "CO", "DM", "DO", "ET", "GT", "GU", "HK",
    "HN", "ID", "IL", "IN", "IS", "JM", "JP", "KE", "KH", "KR", "LA", "MH", "MM", "MO", "MT", "MX",
    "MZ", "NI", "NP", "PA", "PE", "PH", "PK", "PR", "PT", "PY", "SA", "SG", "SV", "TH", "TT", "TW",
    "UM", "US", "VE", "VI", "WS", "YE", "ZA", "ZW",
];

/// CLDR 48 territories whose first day is Saturday.
const SATURDAY_FIRST: &[&str] = &[
    "AF", "BH", "DJ", "DZ", "EG", "IQ", "IR", "JO", "KW", "LY", "OM", "QA", "SD", "SY",
];

/// CLDR 48 territories whose preferred hour cycle is 12-hour (`h11` or `h12`).
const TWELVE_HOUR: &[&str] = &[
    "AE", "AG", "AL", "AR", "AS", "AU", "BB", "BD", "BH", "BM", "BN", "BO", "BS", "BT", "CA", "CL",
    "CO", "CR", "CU", "CY", "DJ", "DM", "DO", "DZ", "EC", "EG", "EH", "ER", "ET", "FJ", "FM", "GD",
    "GH", "GM", "GR", "GT", "GU", "GY", "HK", "HN", "IN", "IQ", "JM", "JO", "KH", "KI", "KN", "KP",
    "KR", "KW", "KY", "LB", "LC", "LR", "LS", "LY", "MH", "MO", "MP", "MR", "MW", "MX", "MY", "NA",
    "NI", "NZ", "OM", "PA", "PE", "PG", "PH", "PK", "PR", "PS", "PW", "PY", "QA", "SA", "SB", "SD",
    "SG", "SL", "SO", "SS", "SV", "SY", "SZ", "TC", "TD", "TN", "TO", "TT", "TW", "UM", "US", "UY",
    "VC", "VE", "VG", "VI", "VU", "WS", "YE", "ZM",
];

/// CLDR 48 territory whose first day is Friday.
const FRIDAY_FIRST: &[&str] = &["MV"];

/// Resolve `week_start`, where `None` means `"auto"`.
///
/// Precedence: explicit `configured`, then a `-u-fw-` extension, then the
/// territory table, then Monday (ISO 8601).
pub fn resolve_week_start(configured: Option<Weekday>, locale: Option<&str>) -> Weekday {
    if let Some(w) = configured {
        return w;
    }
    let Some(tag) = locale_tag::parse(locale) else {
        return Weekday::Mon;
    };
    if let Some(w) = tag.first_day {
        return w;
    }
    match territory(&tag) {
        Some(r) if SUNDAY_FIRST.contains(&r) => Weekday::Sun,
        Some(r) if SATURDAY_FIRST.contains(&r) => Weekday::Sat,
        Some(r) if FRIDAY_FIRST.contains(&r) => Weekday::Fri,
        _ => Weekday::Mon,
    }
}

/// Resolve 12-vs-24-hour display, where `None` means `"auto"`.
///
/// Returns `true` for 12-hour. Precedence: explicit `configured`, then a
/// `-u-hc-` extension, then the territory table, then 24-hour.
pub fn resolve_time_format(configured: Option<bool>, locale: Option<&str>) -> bool {
    if let Some(t) = configured {
        return t;
    }
    let Some(tag) = locale_tag::parse(locale) else {
        return false;
    };
    if let Some(h12) = tag.hour12 {
        return h12;
    }
    territory(&tag).is_some_and(|r| TWELVE_HOUR.contains(&r))
}

fn territory(tag: &LocaleTag) -> Option<&str> {
    tag.region.as_deref()
}
