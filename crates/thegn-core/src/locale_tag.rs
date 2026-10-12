//! One locale-normalisation contract shared by the calendar and weather
//! `auto` resolvers.
//!
//! Pure: callers pass the environment's locale string in (`LC_TIME`,
//! `LC_ALL`, `LANG`); nothing here reads process state.
//!
//! Accepted forms: POSIX (`en_US.UTF-8`, `ca_ES@valencia`), BCP 47
//! (`en-GB`, `zh-Hant-TW`, `sr-Latn-RS`), and BCP 47 with a `-u-` Unicode
//! extension (`en-US-u-hc-h23`, `de-DE-u-fw-sun`). `C` / `POSIX` / empty carry
//! no locale information and parse to `None`.

use chrono::Weekday;

/// The pieces of a locale tag the `auto` resolvers consume. Subtags are
/// normalised: language lowercase, script title-case, region uppercase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocaleTag {
    pub language: String,
    pub script: Option<String>,
    /// ISO 3166 alpha-2 region. UN M.49 numeric macroregions (`es-419`) are
    /// not territories and yield `None`.
    pub region: Option<String>,
    /// `-u-hc-`: `Some(true)` for `h11`/`h12`, `Some(false)` for `h23`/`h24`.
    pub hour12: Option<bool>,
    /// `-u-fw-`: an explicit first day of the week.
    pub first_day: Option<Weekday>,
}

/// Parse a locale string; `None` for absent, empty, `C`, `POSIX`, or
/// anything whose first subtag is not a language.
pub fn parse(locale: Option<&str>) -> Option<LocaleTag> {
    let raw = locale?.trim();
    // POSIX charset / modifier suffixes are not part of the tag.
    let head = raw.split(['.', '@']).next()?.trim();
    if head.is_empty() {
        return None;
    }
    let mut subtags = head.split(['_', '-']);
    let language = subtags.next()?.to_ascii_lowercase();
    if matches!(language.as_str(), "c" | "posix")
        || !(2..=8).contains(&language.len())
        || !language.bytes().all(|b| b.is_ascii_lowercase())
    {
        return None;
    }
    let mut tag = LocaleTag {
        language,
        script: None,
        region: None,
        hour12: None,
        first_day: None,
    };
    let mut seen_variant_or_ext = false;
    let mut in_unicode_ext = false;
    let mut key: Option<String> = None;
    for sub in subtags {
        let s = sub.to_ascii_lowercase();
        if s.is_empty() {
            continue;
        }
        if s == "x" {
            // Private-use subtags are opaque, even if they resemble a -u-
            // extension or contain values the resolvers understand.
            break;
        }
        if in_unicode_ext {
            if s.len() == 1 {
                // A different singleton ends the `u` extension.
                in_unicode_ext = s == "u";
                key = None;
                continue;
            }
            if s.len() == 2 {
                key = Some(s);
            } else if let Some(k) = key.as_deref() {
                match (k, s.as_str()) {
                    ("hc", "h11" | "h12") => tag.hour12 = Some(true),
                    ("hc", "h23" | "h24") => tag.hour12 = Some(false),
                    ("fw", v) => tag.first_day = parse_fw(v).or(tag.first_day),
                    _ => {}
                }
            }
            continue;
        }
        if s.len() == 1 {
            seen_variant_or_ext = true;
            in_unicode_ext = s == "u";
            continue;
        }
        if seen_variant_or_ext {
            continue;
        }
        let alpha = s.bytes().all(|b| b.is_ascii_lowercase());
        if tag.script.is_none() && tag.region.is_none() && s.len() == 4 && alpha {
            let mut c = s.chars();
            let first = c.next().map(|f| f.to_ascii_uppercase())?;
            tag.script = Some(format!("{first}{}", c.as_str()));
        } else if tag.region.is_none() && s.len() == 2 && alpha {
            tag.region = Some(s.to_ascii_uppercase());
        } else {
            // Numeric macroregion, variant, or POSIX noise: stop reading
            // script/region so a later subtag is never misread as a region.
            seen_variant_or_ext = true;
        }
    }
    Some(tag)
}

fn parse_fw(v: &str) -> Option<Weekday> {
    Some(match v {
        "mon" => Weekday::Mon,
        "tue" => Weekday::Tue,
        "wed" => Weekday::Wed,
        "thu" => Weekday::Thu,
        "fri" => Weekday::Fri,
        "sat" => Weekday::Sat,
        "sun" => Weekday::Sun,
        _ => return None,
    })
}

/// The ISO region of a locale string, or `None` when it has none.
pub fn region(locale: Option<&str>) -> Option<String> {
    parse(locale)?.region
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posix_forms_normalise() {
        let t = parse(Some("en_US.UTF-8")).unwrap();
        assert_eq!(
            (t.language.as_str(), t.region.as_deref()),
            ("en", Some("US"))
        );
        let t = parse(Some("ca_ES.UTF-8@valencia")).unwrap();
        assert_eq!(t.region.as_deref(), Some("ES"));
        let t = parse(Some("sr_RS@latin")).unwrap();
        assert_eq!(t.region.as_deref(), Some("RS"));
        let t = parse(Some("  de_DE  ")).unwrap();
        assert_eq!(t.region.as_deref(), Some("DE"));
    }

    #[test]
    fn script_is_not_mistaken_for_region() {
        let t = parse(Some("zh-Hant-TW")).unwrap();
        assert_eq!(t.script.as_deref(), Some("Hant"));
        assert_eq!(t.region.as_deref(), Some("TW"));
        let t = parse(Some("sr_Latn_RS")).unwrap();
        assert_eq!(t.script.as_deref(), Some("Latn"));
        assert_eq!(t.region.as_deref(), Some("RS"));
        // Script with no region has no region.
        assert_eq!(region(Some("zh-Hant")), None);
    }

    #[test]
    fn no_information_yields_none() {
        for l in [
            None,
            Some(""),
            Some("  "),
            Some("C"),
            Some("POSIX"),
            Some("C.UTF-8"),
        ] {
            assert_eq!(parse(l), None, "{l:?}");
        }
        assert_eq!(region(Some("en")), None);
        // Macroregion digits are not a territory.
        assert_eq!(region(Some("es-419")), None);
    }

    #[test]
    fn unicode_extensions_are_read() {
        let t = parse(Some("en-US-u-hc-h23")).unwrap();
        assert_eq!(t.region.as_deref(), Some("US"));
        assert_eq!(t.hour12, Some(false));
        let t = parse(Some("de-DE-u-hc-h12-fw-sun")).unwrap();
        assert_eq!(t.hour12, Some(true));
        assert_eq!(t.first_day, Some(Weekday::Sun));
        // Other extensions do not leak a region or hour cycle.
        let t = parse(Some("en-t-hc-h12")).unwrap();
        assert_eq!(t.hour12, None);
        assert_eq!(t.region, None);
        let t = parse(Some("en-x-u-hc-h12")).unwrap();
        assert_eq!(t.hour12, None);
        assert_eq!(t.first_day, None);
        // A -u- keyword with a value-less / unknown type is ignored.
        let t = parse(Some("en-GB-u-ca-gregory-fw-xyz")).unwrap();
        assert_eq!((t.hour12, t.first_day), (None, None));
    }
}
