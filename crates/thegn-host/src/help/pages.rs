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
    include_str!("../../../../docs/help/copy-and-select.md"),
    include_str!("../../../../docs/help/panel.md"),
    include_str!("../../../../docs/help/drawer-and-corner.md"),
    include_str!("../../../../docs/help/bars.md"),
    include_str!("../../../../docs/help/system-monitor.md"),
    include_str!("../../../../docs/help/calendar.md"),
    include_str!("../../../../docs/help/command-palette.md"),
    include_str!("../../../../docs/help/search.md"),
    include_str!("../../../../docs/help/git-and-diffs.md"),
    include_str!("../../../../docs/help/share-and-forward.md"),
    include_str!("../../../../docs/help/media.md"),
    include_str!("../../../../docs/help/daemon-and-sessions.md"),
    include_str!("../../../../docs/help/release-channels.md"),
    include_str!("../../../../docs/help/cli.md"),
    include_str!("../../../../docs/help/workflows.md"),
    include_str!("../../../../docs/help/review-a-pr.md"),
    include_str!("../../../../docs/help/merge-queue.md"),
    include_str!("../../../../docs/help/pr-queue.md"),
    include_str!("../../../../docs/help/sandboxing.md"),
    include_str!("../../../../docs/help/configuration.md"),
    include_str!("../../../../docs/help/terminal-compatibility.md"),
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

    /// treefmt runs prettier over `docs/help/`, which pads pipe tables into
    /// aligned columns (`| ---- | ------ |`). That form must still parse as a
    /// table — otherwise a formatting pass would silently turn every table on
    /// every page into paragraph soup.
    /// `docs/help/*.md` and `SOURCES` are the same set: a page dropped into
    /// the directory but never `include_str!`'d here is silently dead (it can't
    /// satisfy a ratchet either, but the failure would be indirect), and a
    /// stale include is a build error anyway. Generated pages (keybindings,
    /// config-reference) are appended at build time and are not on disk.
    #[test]
    fn every_help_page_is_registered() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/help");
        let mut on_disk: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".md"))
            .collect();
        on_disk.sort();
        // The include list is the source text of this file: parse the paths.
        let me = include_str!("pages.rs");
        let mut included: Vec<String> = me
            .lines()
            .filter_map(|l| {
                let l = l.trim();
                l.strip_prefix("include_str!(\"../../../../docs/help/")
                    .and_then(|r| r.strip_suffix("\"),"))
                    .map(str::to_string)
            })
            .collect();
        included.sort();
        assert_eq!(
            included.len(),
            SOURCES.len(),
            "include parse drifted from SOURCES"
        );
        assert_eq!(
            on_disk, included,
            "docs/help/ and help::pages::SOURCES disagree — add the include_str! (or delete the file)"
        );
    }

    #[test]
    fn authored_tables_survive_the_formatter() {
        use thegn_core::help::markdown::Block;
        let (reg, _) = build_registry(&thegn_core::config::Config::default());
        let mut with_tables = 0;
        for page in reg.pages() {
            let tables = page
                .blocks
                .iter()
                .filter(|b| matches!(b, Block::Table { .. }))
                .count();
            if tables > 0 {
                with_tables += 1;
            }
            // A page whose body has an aligned delimiter row but no Table
            // block means the parser stopped recognising the formatted shape.
            let looks_tabular = page
                .body
                .lines()
                .any(|l| l.trim_start().starts_with("| --") || l.trim_start().starts_with("| ---"));
            assert_eq!(
                looks_tabular,
                tables > 0,
                "page `{}` has a delimiter row but parsed no table",
                page.meta.id
            );
        }
        assert!(with_tables >= 3, "several pages use tables");
    }

    /// The config-reference page is *generated* from `config.toml.example`, so a
    /// malformed comment block above a `[table]` can silently drop that whole
    /// section from the docs while every other gate stays green. Spot-check the
    /// feature tables that carry a comment-block preamble.
    #[test]
    fn the_generated_config_reference_covers_the_feature_tables() {
        let (reg, _) = build_registry(&thegn_core::config::Config::default());
        let page = reg.page("config-reference").expect("generated page");
        let body = format!("{page:?}");
        for table in ["merge_queue", "pr_queue", "sandbox", "theme"] {
            assert!(
                body.contains(table),
                "config reference lost the [{table}] section"
            );
        }
    }

    #[test]
    fn context_pages_resolve() {
        let (reg, _) = build_registry(&thegn_core::config::Config::default());
        assert_eq!(reg.page_for_context("zone:sidebar"), Some("sidebar"));
        assert_eq!(reg.page_for_context("panel:merge"), Some("merge-queue"));
        // Sections with no dedicated page fall back to the panel overview.
        // NOTE: that page currently describes the accordion and a handful of
        // sections, not all of them — so this is a *reachability* guarantee,
        // not a coverage one. Growing `panel.md` is tracked separately.
        assert_eq!(reg.page_for_context("panel:telemetry"), Some("panel"));
        // A context nobody claims lands on index, never nowhere. (`panel:debug`
        // is a dev-only section — see test/help-context-ratchet.txt.)
        assert_eq!(reg.page_for_context("panel:debug"), Some("index"));
    }
}
