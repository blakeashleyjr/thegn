//! Embedded, startup-resolved translation facade for the UI.

use std::borrow::Cow;
use std::collections::HashMap;

use fluent_templates::fluent_bundle::FluentValue;
use fluent_templates::{Loader, static_loader};
use once_cell::sync::OnceCell;
use unic_langid::LanguageIdentifier;

pub use crate::i18n_locale::{LocaleResolution, LocaleSource, resolve_locale};

static_loader! {
    pub static LOCALES = {
        locales: "./locales",
        fallback_language: "en-US",
    };
}

#[derive(Debug)]
struct ActiveLocale {
    language: LanguageIdentifier,
    pseudolocale: bool,
}

/// The locale is a startup value: first initialization wins for the process.
static ACTIVE_LOCALE: OnceCell<ActiveLocale> = OnceCell::new();

/// Initialize the process locale from an explicit host environment snapshot.
///
/// This should be called exactly once during startup (`thegn::hydrate`). Later
/// calls are intentionally inert, so config reloads cannot relocalize live UI.
pub fn init(
    config_language: &str,
    lc_all: Option<&str>,
    lang: Option<&str>,
    freeze: bool,
    pseudolocale_requested: bool,
) {
    let _active = init_cell(
        &ACTIVE_LOCALE,
        config_language,
        lc_all,
        lang,
        freeze,
        pseudolocale_requested,
    );
}

fn init_cell<'a>(
    cell: &'a OnceCell<ActiveLocale>,
    config_language: &str,
    lc_all: Option<&str>,
    lang: Option<&str>,
    freeze: bool,
    pseudolocale_requested: bool,
) -> &'a ActiveLocale {
    cell.get_or_init(|| {
        let resolution = resolve_locale(
            Some(config_language),
            lc_all,
            lang,
            freeze,
            pseudolocale_requested,
        );
        if let Some(diagnostic) = resolution.diagnostic.as_deref() {
            crate::msg::warn(diagnostic);
        }
        ActiveLocale {
            language: resolution.language,
            pseudolocale: resolution.pseudolocale,
        }
    })
}

/// Retrieve the active language identifier, defaulting defensively before init.
pub fn active_lang() -> &'static LanguageIdentifier {
    ACTIVE_LOCALE
        .get()
        .map(|active| &active.language)
        .unwrap_or_else(default_language)
}

fn default_language() -> &'static LanguageIdentifier {
    static FALLBACK: once_cell::sync::Lazy<LanguageIdentifier> =
        once_cell::sync::Lazy::new(|| "en-US".parse().expect("valid default locale"));
    &FALLBACK
}

#[doc(hidden)]
pub fn lookup(key: &str) -> String {
    lookup_complete(key, None)
}

#[doc(hidden)]
pub fn lookup_with_args(key: &str, args: &HashMap<Cow<'static, str>, FluentValue<'_>>) -> String {
    lookup_complete(key, Some(args))
}

fn lookup_complete(
    key: &str,
    args: Option<&HashMap<Cow<'static, str>, FluentValue<'_>>>,
) -> String {
    let pseudo = ACTIVE_LOCALE
        .get()
        .is_some_and(|active| active.pseudolocale);
    let rendered = if pseudo {
        crate::i18n_pseudo::lookup(key, args).unwrap_or_else(|| default_lookup(key, args))
    } else {
        default_lookup(key, args)
    };
    clean_lookup(rendered, key)
}

fn default_lookup(key: &str, args: Option<&HashMap<Cow<'static, str>, FluentValue<'_>>>) -> String {
    match args {
        Some(args) => LOCALES.lookup_with_args(active_lang(), key, args),
        None => LOCALES.lookup(active_lang(), key),
    }
}

fn clean_lookup(rendered: String, key: &str) -> String {
    if rendered.starts_with("Unknown localization key:") {
        key.to_string()
    } else {
        // Fluent isolates interpolated values; the TUI composes directionality
        // itself and must not count those zero-width controls as layout cells.
        rendered.replace(['\u{2068}', '\u{2069}'], "")
    }
}

/// Look up an embedded Fluent message.
///
/// Usage: `t!("hello-world")` or `t!("workspace-title", name = "thegn")`.
#[macro_export]
macro_rules! t {
    ($key:expr) => {
        $crate::i18n::lookup($key)
    };
    ($key:expr, $($arg:ident = $val:expr),* $(,)?) => {{
        let mut args = std::collections::HashMap::new();
        $(
            args.insert(
                std::borrow::Cow::Borrowed(stringify!($arg)),
                fluent_templates::fluent_bundle::FluentValue::from($val)
            );
        )*
        $crate::i18n::lookup_with_args($key, &args)
    }};
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn embedded_lookup_falls_back_before_init() {
        assert_eq!(t!("hello-world"), "Hello World!");
        assert_eq!(t!("workspace-title", name = "thegn"), "Workspace: thegn");
        assert_eq!(t!("missing-key"), "missing-key");
    }

    #[test]
    fn initialization_is_once_only() {
        let cell = OnceCell::new();
        let first = init_cell(&cell, "ja-JP", None, None, false, false);
        assert_eq!(first.language.to_string(), "ja-JP");
        assert!(!first.pseudolocale);

        let second = init_cell(&cell, "en-US", None, None, false, true);
        assert!(std::ptr::eq(first, second));
        assert_eq!(second.language.to_string(), "ja-JP");
        assert!(!second.pseudolocale);
    }

    #[test]
    fn parity_registry_covers_every_embedded_locale() {
        let embedded = LOCALES
            .locales()
            .map(ToString::to_string)
            .collect::<BTreeSet<_>>();
        let registered = crate::i18n_parity::SHIPPED_LOCALES
            .iter()
            .map(|source| source.locale.to_string())
            .collect::<BTreeSet<_>>();
        assert_eq!(registered, embedded);
    }
}
