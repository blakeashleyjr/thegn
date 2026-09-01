//! Small deterministic locale-aware formatting primitives.

use chrono::{Datelike, NaiveDate};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluralCategory {
    One,
    Other,
}

impl PluralCategory {
    pub const fn selector(self) -> &'static str {
        match self {
            Self::One => "one",
            Self::Other => "other",
        }
    }
}

/// Return the plural selector supported by the embedded proof locales.
pub fn plural_category(locale: &str, value: i64) -> PluralCategory {
    match locale_family(locale) {
        LocaleFamily::Japanese => PluralCategory::Other,
        LocaleFamily::English => {
            if value.unsigned_abs() == 1 {
                PluralCategory::One
            } else {
                PluralCategory::Other
            }
        }
    }
}

/// Format a base-10 integer deterministically for the embedded proof locales.
pub fn format_integer(locale: &str, value: i64) -> String {
    let _family = locale_family(locale);
    let digits = value.unsigned_abs().to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, byte) in digits.bytes().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(char::from(byte));
    }
    if value.is_negative() {
        grouped.insert(0, '-');
    }
    grouped
}

/// Format a numeric short date without consulting clocks, time zones, or env.
pub fn format_date(locale: &str, date: NaiveDate) -> String {
    match locale_family(locale) {
        LocaleFamily::Japanese => {
            format!("{:04}/{:02}/{:02}", date.year(), date.month(), date.day())
        }
        LocaleFamily::English => {
            format!("{:02}/{:02}/{:04}", date.month(), date.day(), date.year())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocaleFamily {
    English,
    Japanese,
}

fn locale_family(locale: &str) -> LocaleFamily {
    locale
        .parse::<unic_langid::LanguageIdentifier>()
        .ok()
        .filter(|id| id.language.as_str() == "ja")
        .map_or(LocaleFamily::English, |_| LocaleFamily::Japanese)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_and_japanese_plural_categories_are_stable() {
        assert_eq!(plural_category("en-US", 1), PluralCategory::One);
        assert_eq!(plural_category("en-US", -1), PluralCategory::One);
        assert_eq!(plural_category("en-US", 0), PluralCategory::Other);
        assert_eq!(plural_category("en-US", 2), PluralCategory::Other);
        assert_eq!(plural_category("ja-JP", 1), PluralCategory::Other);
        assert_eq!(plural_category("ja-JP", 2), PluralCategory::Other);
        assert_eq!(plural_category("invalid locale", 1), PluralCategory::One);
        assert_eq!(PluralCategory::Other.selector(), "other");
    }

    #[test]
    fn integers_cover_negative_and_large_values() {
        assert_eq!(format_integer("en-US", -1_234_567), "-1,234,567");
        assert_eq!(
            format_integer("ja-JP", 9_223_372_036_854_775_807),
            "9,223,372,036,854,775,807"
        );
        assert_eq!(
            format_integer("invalid locale", i64::MIN),
            "-9,223,372,036,854,775,808"
        );
    }

    #[test]
    fn short_dates_are_locale_specific_and_deterministic() {
        let date = NaiveDate::from_ymd_opt(2026, 8, 29).expect("valid fixture");
        assert_eq!(format_date("en-US", date), "08/29/2026");
        assert_eq!(format_date("ja-JP", date), "2026/08/29");
        assert_eq!(format_date("bad", date), "08/29/2026");
        assert_eq!(format_date("en-US", date), format_date("en-US", date));
    }
}
