//! The embedded help content: every authored page under `docs/help/`,
//! compiled into the binary, plus the two runtime-generated pages
//! (keybindings, config reference). `build_registry` is the one place a
//! help registry comes from — the overlay, the panel tab, and the ratchet
//! test all see the same page set.

use thegn_core::help::{HelpRegistry, ValidationError};

/// Every authored page. Adding a file here (and under `docs/help/`) is how
/// a feature gets documentation; the ratchet test enforces coverage.
pub const SOURCES: &[&str] = &[
    include_str!("../../../../docs/help/index.md"),
    include_str!("../../../../docs/help/getting-started.md"),
    include_str!("../../../../docs/help/workspaces-and-worktrees.md"),
    include_str!("../../../../docs/help/sidebar.md"),
    include_str!("../../../../docs/help/terminal-and-panes.md"),
    include_str!("../../../../docs/help/panel.md"),
    include_str!("../../../../docs/help/drawer-and-corner.md"),
    include_str!("../../../../docs/help/bars.md"),
    include_str!("../../../../docs/help/command-palette.md"),
    include_str!("../../../../docs/help/search.md"),
    include_str!("../../../../docs/help/git-and-diffs.md"),
    include_str!("../../../../docs/help/share-and-forward.md"),
    include_str!("../../../../docs/help/media.md"),
    include_str!("../../../../docs/help/workflows.md"),
    include_str!("../../../../docs/help/review-a-pr.md"),
    include_str!("../../../../docs/help/merge-queue.md"),
    include_str!("../../../../docs/help/sandboxing.md"),
    include_str!("../../../../docs/help/configuration.md"),
    include_str!("../../../../docs/help/best-practices.md"),
    include_str!("../../../../docs/help/help.md"),
];

/// The example config the config-reference page is generated from (the same
/// bytes `thegn config example` prints).
const EXAMPLE_CONFIG: &str = include_str!("../../../../config/config.toml.example");

/// Build the full registry for `cfg`: authored pages + the generated
/// keybindings and config-reference pages. Total — validation errors are
/// returned, not thrown; in CI the ratchet test asserts the list is empty.
pub fn build_registry(cfg: &thegn_core::config::Config) -> (HelpRegistry, Vec<ValidationError>) {
    let mut sources: Vec<String> = SOURCES.iter().map(|s| (*s).to_string()).collect();
    sources.push(super::gen_pages::keybindings_page(cfg));
    sources.push(thegn_core::help::config_ref::page(EXAMPLE_CONFIG));
    let refs: Vec<&str> = sources.iter().map(String::as_str).collect();
    let vocab = super::context::vocabulary();
    let vocab_refs: Vec<&str> = vocab.iter().map(String::as_str).collect();
    HelpRegistry::build(&refs, &vocab_refs)
}

/// `build_registry` with errors logged (debug builds of pages are caught in
/// tests; at runtime a broken page should degrade, never crash).
pub fn registry_logged(cfg: &thegn_core::config::Config) -> HelpRegistry {
    let (reg, errors) = build_registry(cfg);
    for e in &errors {
        tracing::warn!(target: "thegn::help", "help page validation: {e}");
    }
    reg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_builds_cleanly_from_the_shipped_pages() {
        // The full validation gate lives in help::ratchet_tests; this early
        // copy keeps `pages.rs` edits honest even when the ratchet module is
        // filtered out of a test run.
        let (reg, errors) = build_registry(&thegn_core::config::Config::default());
        assert!(
            errors.is_empty(),
            "shipped help pages must validate: {errors:?}"
        );
        assert!(reg.page("index").is_some());
        assert!(
            reg.page("keybindings").is_some(),
            "generated page registered"
        );
        assert!(
            reg.page("config-reference").is_some(),
            "generated page registered"
        );
    }

    #[test]
    fn context_pages_resolve() {
        let (reg, _) = build_registry(&thegn_core::config::Config::default());
        assert_eq!(reg.page_for_context("zone:sidebar"), Some("sidebar"));
        assert_eq!(reg.page_for_context("panel:merge"), Some("merge-queue"));
        // Unclaimed contexts land on index, never nowhere.
        assert_eq!(reg.page_for_context("panel:telemetry"), Some("index"));
    }
}
