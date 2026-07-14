//! The sidebar's curated statusbar hints. The old `?` cheatsheet card was
//! subsumed by the built-in help system (`docs/help/sidebar.md`, opened by
//! `?`/F1 at `zone:sidebar`); what remains is the always-on essentials strip.
//! Change a key in `handlers/sidebar_keys.rs` → update this table AND the
//! sidebar help page.

/// The curated always-on statusbar pairs while the sidebar owns focus (spliced
/// ahead of the registry hints): the five keys a newcomer needs first.
pub(crate) fn statusbar_pairs() -> Vec<(String, String)> {
    [
        ("↵", "open"),
        ("n", "new"),
        ("d", "delete"),
        ("m", "menu"),
        ("?", "help"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statusbar_pairs_are_short_and_essential() {
        let pairs = statusbar_pairs();
        assert!(pairs.len() <= 6, "statusbar hints must stay skimmable");
        assert!(pairs.iter().any(|(k, _)| k == "?"));
    }

    #[test]
    fn sidebar_help_page_covers_the_key_surface() {
        // The `?` card's guarantee, carried forward: the sidebar help page
        // documents the essential sidebar keys, so the cheatsheet can't rot.
        let page = include_str!("../../../docs/help/sidebar.md");
        for key in [
            "`n`", "`N`", "`b`", "`f`", "`F2`", "`d`", "`s`", "`p`", "`m`",
        ] {
            assert!(page.contains(key), "docs/help/sidebar.md missing key {key}");
        }
    }
}
