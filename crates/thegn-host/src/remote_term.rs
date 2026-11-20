//! The terminal identity a REMOTE pane advertises. A pane's terminal is
//! thegn's own emulator, so a remote shell gets what a local pane gets
//! (`pane_pty.rs:488-491`), never the host terminal's `TERM`.

pub(crate) const PANE_TERM: &str = "xterm-256color";
pub(crate) const PANE_COLORTERM: &str = "truecolor";

/// First non-empty of `LC_ALL`, `LC_CTYPE`, `LANG` from `get`.
pub(crate) fn host_locale(get: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .into_iter()
        .find_map(|key| get(key).filter(|value| !value.is_empty()))
}

/// `Some("C.UTF-8")` when `locale` names a UTF-8 codeset, else `None`.
pub(crate) fn remote_locale(locale: Option<&str>) -> Option<&'static str> {
    let codeset = locale?.split_once('.')?.1;
    let codeset = codeset
        .split_once('@')
        .map_or(codeset, |(codeset, _)| codeset);
    (codeset.eq_ignore_ascii_case("utf-8") || codeset.eq_ignore_ascii_case("utf8"))
        .then_some("C.UTF-8")
}

/// Replace host terminal and locale variables with the identity supported by
/// the remote pane's emulator.
pub(crate) fn apply(
    env: &mut Vec<(String, String)>,
    term: &str,
    colorterm: Option<&str>,
    locale: Option<&str>,
) {
    env.retain(|(key, _)| {
        !matches!(
            key.as_str(),
            "TERM" | "COLORTERM" | "LANG" | "LC_ALL" | "LC_CTYPE"
        )
    });
    env.push(("TERM".to_string(), term.to_string()));
    if let Some(colorterm) = colorterm {
        env.push(("COLORTERM".to_string(), colorterm.to_string()));
    }
    if let Some(lang) = remote_locale(locale) {
        env.push(("LANG".to_string(), lang.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_replaces_host_terminal_and_preserves_other_keys_in_order() {
        let mut env = vec![
            ("BEFORE".into(), "one".into()),
            ("TERM".into(), "tmux-256color".into()),
            ("MIDDLE".into(), "two".into()),
            ("COLORTERM".into(), "24bit".into()),
            ("LANG".into(), "en_US.UTF-8".into()),
            ("LC_ALL".into(), "C".into()),
            ("LC_CTYPE".into(), "C".into()),
            ("AFTER".into(), "three".into()),
        ];

        apply(
            &mut env,
            PANE_TERM,
            Some(PANE_COLORTERM),
            Some("en_US.UTF-8"),
        );

        assert_eq!(
            env,
            vec![
                ("BEFORE".into(), "one".into()),
                ("MIDDLE".into(), "two".into()),
                ("AFTER".into(), "three".into()),
                ("TERM".into(), "xterm-256color".into()),
                ("COLORTERM".into(), "truecolor".into()),
                ("LANG".into(), "C.UTF-8".into()),
            ]
        );
        for key in ["TERM", "COLORTERM", "LANG"] {
            assert_eq!(
                env.iter().filter(|(name, _)| name == key).count(),
                1,
                "{key}"
            );
        }
        for key in ["LC_ALL", "LC_CTYPE"] {
            assert_eq!(
                env.iter().filter(|(name, _)| name == key).count(),
                0,
                "{key}"
            );
        }
    }

    #[test]
    fn remote_locale_accepts_utf8_codesets_only() {
        for (locale, expected) in [
            (Some("en_US.UTF-8"), Some("C.UTF-8")),
            (Some("C.utf8"), Some("C.UTF-8")),
            (Some("de_DE.ISO-8859-1"), None),
            (Some(""), None),
            (None, None),
        ] {
            assert_eq!(remote_locale(locale), expected, "{locale:?}");
        }
    }

    #[test]
    fn host_locale_uses_lc_all_then_lc_ctype_then_lang() {
        let values = [("LC_ALL", "all"), ("LC_CTYPE", "ctype"), ("LANG", "lang")];
        let get = |key: &str| {
            values
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_string())
        };
        assert_eq!(host_locale(&get).as_deref(), Some("all"));

        assert_eq!(
            host_locale(&|key| match key {
                "LC_ALL" => Some(String::new()),
                "LC_CTYPE" => Some("ctype".into()),
                "LANG" => Some("lang".into()),
                _ => None,
            })
            .as_deref(),
            Some("ctype")
        );
    }
}
