//! Zero-cost translation layer for the UI.

// `Loader` provides the `.lookup`/`.lookup_with_args` methods the `t!` macro
// expands to; it's only "used" at macro call sites (incl. the tests below), so
// the lib build sees it as unused.
#[allow(unused_imports)]
use fluent_templates::Loader;
use fluent_templates::static_loader;
use once_cell::sync::OnceCell;
use unic_langid::LanguageIdentifier;

static_loader! {
    pub static LOCALES = {
        locales: "./locales",
        fallback_language: "en-US",
    };
}

/// The globally resolved language identifier.
static ACTIVE_LANG: OnceCell<LanguageIdentifier> = OnceCell::new();

/// Initializes the global language based on the user's config and OS locale.
/// This should be called exactly once during startup (`szhost::hydrate`).
pub fn init(config_lang: &str) {
    let lang_str = if config_lang == "auto" {
        sys_locale::get_locale().unwrap_or_else(|| "en-US".to_string())
    } else {
        config_lang.to_string()
    };

    let lang_id: LanguageIdentifier = lang_str.parse().unwrap_or_else(|_| {
        crate::msg::warn(&format!(
            "i18n: invalid language '{}', falling back to en-US",
            lang_str
        ));
        "en-US".parse().unwrap()
    });

    let _ = ACTIVE_LANG.set(lang_id);
}

/// Retrieve the active language identifier.
pub fn active_lang() -> &'static LanguageIdentifier {
    ACTIVE_LANG.get().unwrap_or_else(|| {
        // Fallback for tests or if not initialized yet
        static FALLBACK: once_cell::sync::Lazy<LanguageIdentifier> =
            once_cell::sync::Lazy::new(|| "en-US".parse().unwrap());
        &FALLBACK
    })
}

/// Macro to look up a string.
/// Usage: `t!("hello-world")` or `t!("workspace-title", name = "my-workspace")`
#[macro_export]
macro_rules! t {
    ($key:expr) => {
        {
            let res = $crate::i18n::LOCALES.lookup($crate::i18n::active_lang(), $key);
            if res.starts_with("Unknown localization key:") {
                $key.to_string() // Fallback if fluent returns the missing key message
            } else {
                // fluent_templates sometimes inserts Unicode isolation markers around values;
                // strip them out to keep the TUI layout clean.
                res.replace('\u{2068}', "").replace('\u{2069}', "")
            }
        }
    };
    ($key:expr, $($arg:ident = $val:expr),* $(,)?) => {{
        let mut args = std::collections::HashMap::new();
        $(
            args.insert(
                std::borrow::Cow::Borrowed(stringify!($arg)),
                fluent_templates::fluent_bundle::FluentValue::from($val)
            );
        )*
        let res = $crate::i18n::LOCALES.lookup_with_args($crate::i18n::active_lang(), $key, &args);
        if res.starts_with("Unknown localization key:") {
            $key.to_string()
        } else {
            res.replace('\u{2068}', "").replace('\u{2069}', "")
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_i18n_fallback() {
        // Just checking it works even before init()
        assert_eq!(t!("hello-world"), "Hello World!");
        assert_eq!(
            t!("workspace-title", name = "superzej"),
            "Workspace: superzej"
        );
        assert_eq!(t!("missing-key"), "missing-key");
    }
}
