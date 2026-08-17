//! The sidebar's statusbar hint strip. The old `?` cheatsheet card was
//! subsumed by the built-in help system (`docs/help/sidebar.md`, opened by
//! `?`/F1 at `zone:sidebar`); what remains is the always-on essentials strip.
//!
//! The rows are **derived**, not restated: they come from
//! [`crate::sidebar_keytable::SIDEBAR_KEYS`], the same table that drives
//! dispatch. Adding a sidebar key surfaces it here automatically; there is no
//! second list to keep in sync.

use crate::sidebar_keytable::{HintTier, hints};

/// The always-on statusbar pairs while the sidebar owns focus, spliced ahead of
/// the registry hints by [`crate::keyhint::context_hints`].
pub(crate) fn statusbar_pairs() -> Vec<(String, String)> {
    hints(HintTier::Essential)
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

    /// The strip is the table's Essential tier, not a parallel list.
    #[test]
    fn statusbar_pairs_track_the_key_table() {
        assert_eq!(statusbar_pairs(), hints(HintTier::Essential));
    }
}
