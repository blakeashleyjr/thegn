//! Pure startup locale precedence.

use std::borrow::Cow;

use unic_langid::LanguageIdentifier;

pub(crate) const DEFAULT_LOCALE: &str = "en-US";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocaleSource {
    Freeze,
    Config,
    LcAll,
    Lang,
    Default,
    Pseudolocale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocaleResolution {
    pub language: LanguageIdentifier,
    pub source: LocaleSource,
    pub diagnostic: Option<String>,
    pub pseudolocale: bool,
}

/// Resolve the startup locale without reading process state.
pub fn resolve_locale(
    config_language: Option<&str>,
    lc_all: Option<&str>,
    lang: Option<&str>,
    freeze: bool,
    pseudolocale_requested: bool,
) -> LocaleResolution {
    if freeze {
        return resolved_default(LocaleSource::Freeze, None);
    }
    if pseudolocale_requested {
        return LocaleResolution {
            language: default_language(),
            source: LocaleSource::Pseudolocale,
            diagnostic: None,
            pseudolocale: true,
        };
    }

    let config = config_language.unwrap_or_default().trim();
    if config != "auto" {
        return parse_or_default(config, LocaleSource::Config);
    }
    if let Some(value) = non_empty(lc_all) {
        return parse_or_default(value, LocaleSource::LcAll);
    }
    if let Some(value) = non_empty(lang) {
        return parse_or_default(value, LocaleSource::Lang);
    }
    resolved_default(LocaleSource::Default, None)
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn parse_or_default(value: &str, source: LocaleSource) -> LocaleResolution {
    // Config is documented as BCP-47, while LC_ALL/LANG conventionally carry
    // POSIX spellings such as `ja_JP.UTF-8`. The removed `sys-locale` adapter
    // performed this conversion; keep it at the pure resolver boundary.
    let parse_value = match source {
        LocaleSource::LcAll | LocaleSource::Lang => normalize_posix_locale(value),
        _ => Cow::Borrowed(value),
    };
    match parse_value.parse::<LanguageIdentifier>() {
        Ok(language) => LocaleResolution {
            language,
            source,
            diagnostic: None,
            pseudolocale: false,
        },
        Err(_) => resolved_default(
            LocaleSource::Default,
            Some(format!(
                "i18n: invalid language '{value}', falling back to {DEFAULT_LOCALE}"
            )),
        ),
    }
}

fn normalize_posix_locale(value: &str) -> Cow<'_, str> {
    let base = value
        .split_once(['.', '@'])
        .map_or(value, |(base, _suffix)| base);
    if base.contains('_') || base.len() != value.len() {
        Cow::Owned(base.replace('_', "-"))
    } else {
        Cow::Borrowed(value)
    }
}

fn resolved_default(source: LocaleSource, diagnostic: Option<String>) -> LocaleResolution {
    LocaleResolution {
        language: default_language(),
        source,
        diagnostic,
        pseudolocale: false,
    }
}

fn default_language() -> LanguageIdentifier {
    DEFAULT_LOCALE
        .parse()
        .expect("the built-in default locale is valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(config: Option<&str>, lc_all: Option<&str>, lang: Option<&str>) -> LocaleResolution {
        resolve_locale(config, lc_all, lang, false, false)
    }

    #[test]
    fn explicit_config_wins_over_environment() {
        let got = resolve(Some("fr-FR"), Some("de-DE"), Some("ja-JP"));
        assert_eq!(got.language.to_string(), "fr-FR");
        assert_eq!(got.source, LocaleSource::Config);
        assert!(got.diagnostic.is_none());
    }

    #[test]
    fn auto_uses_lc_all_then_lang() {
        let lc_all = resolve(Some("auto"), Some("de-DE"), Some("ja-JP"));
        assert_eq!(lc_all.language.to_string(), "de-DE");
        assert_eq!(lc_all.source, LocaleSource::LcAll);

        let lang = resolve(Some("auto"), Some("  "), Some("ja-JP"));
        assert_eq!(lang.language.to_string(), "ja-JP");
        assert_eq!(lang.source, LocaleSource::Lang);
    }

    #[test]
    fn auto_normalizes_posix_environment_locales() {
        let lc_all = resolve(Some("auto"), Some("ja_JP.UTF-8"), Some("en_US.UTF-8"));
        assert_eq!(lc_all.language.to_string(), "ja-JP");
        assert_eq!(lc_all.source, LocaleSource::LcAll);
        assert!(lc_all.diagnostic.is_none());

        let lang = resolve(Some("auto"), None, Some("ja_JP@calendar"));
        assert_eq!(lang.language.to_string(), "ja-JP");
        assert_eq!(lang.source, LocaleSource::Lang);
        assert!(lang.diagnostic.is_none());
    }

    #[test]
    fn absent_and_empty_inputs_fall_back() {
        for got in [
            resolve(None, None, None),
            resolve(Some(""), Some("de-DE"), Some("ja-JP")),
            resolve(Some("auto"), Some(""), Some("")),
        ] {
            assert_eq!(got.language.to_string(), DEFAULT_LOCALE);
            assert_eq!(got.source, LocaleSource::Default);
        }
    }

    #[test]
    fn invalid_selected_input_degrades_with_a_diagnostic() {
        let got = resolve(Some("auto"), Some("not a locale"), Some("ja-JP"));
        assert_eq!(got.language.to_string(), DEFAULT_LOCALE);
        assert_eq!(got.source, LocaleSource::Default);
        assert!(got.diagnostic.as_deref().is_some_and(|message| {
            message.contains("not a locale") && message.contains(DEFAULT_LOCALE)
        }));
    }

    #[test]
    fn freeze_beats_every_input_and_the_pseudolocale() {
        let got = resolve_locale(Some("ja-JP"), Some("de-DE"), Some("fr-FR"), true, true);
        assert_eq!(got.language.to_string(), DEFAULT_LOCALE);
        assert_eq!(got.source, LocaleSource::Freeze);
        assert!(!got.pseudolocale);
    }

    #[test]
    fn pseudolocale_is_a_hook_not_a_selectable_language() {
        let requested = resolve_locale(Some("ja-JP"), None, None, false, true);
        assert!(requested.pseudolocale);
        assert_eq!(requested.source, LocaleSource::Pseudolocale);

        let configured = resolve(Some("en-XA"), None, None);
        assert!(!configured.pseudolocale);
        assert_eq!(configured.language.to_string(), "en-XA");
    }
}
