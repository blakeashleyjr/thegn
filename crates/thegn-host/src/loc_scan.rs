//! The `tokei` walk behind the bottom-bar `LOC` chip: turn a worktree path into
//! a per-language [`LocReport`]. Lives off the hydration god-file (called from
//! `hydrate::worktree_loc`, which owns the DB cache around it). Runs on the
//! hydration worker — tokei walks the whole tree, so never call this on the loop.

use std::path::Path;

use thegn_core::loc::{LocLang, LocReport};

/// Count lines under `path` with tokei and fold into a sorted [`LocReport`].
/// Doc strings count as comments (matching the previous behavior).
pub fn scan(path: &Path) -> LocReport {
    let mut languages = tokei::Languages::new();
    let config = tokei::Config {
        treat_doc_strings_as_comments: Some(true),
        ..Default::default()
    };
    languages.get_statistics(&[path.to_path_buf()], &[], &config);
    let langs: Vec<LocLang> = languages
        .iter()
        .filter(|(_, lang)| lang.lines() > 0)
        .map(|(ty, lang)| LocLang {
            name: ty.name().to_string(),
            files: lang.reports.len(),
            lines: lang.lines(),
            code: lang.code,
            comments: lang.comments,
            blanks: lang.blanks,
        })
        .collect();
    LocReport::from_langs(langs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_this_crate_and_detects_rust() {
        // Scan this crate's own `src/` — a real tree that always has Rust.
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let report = scan(&src);
        assert!(report.total_code > 0, "expected some code lines");
        let rust = report.langs.iter().find(|l| l.name == "Rust");
        let rust = rust.expect("Rust should be detected");
        assert!(rust.files > 0 && rust.code > 0);
        // Totals are consistent with the per-language rows.
        assert_eq!(
            report.total_code,
            report.langs.iter().map(|l| l.code).sum::<usize>()
        );
    }
}
