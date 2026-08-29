//! The `tokei` walk behind the bottom-bar `LOC` chip: turn a worktree path into
//! a per-language [`LocReport`]. Lives off the hydration god-file; the caller is
//! `measure::loc`, which owns the DB cache and the scheduling around it. tokei
//! walks the whole tree, so this runs on the background measurement lane and
//! must never be called on the loop or on the interactive hydration lane.

use std::path::Path;

use thegn_core::loc::{LocLang, LocReport};

/// Count lines under `path` with tokei and fold into a sorted [`LocReport`].
/// Doc strings count as comments (matching the previous behavior).
///
/// `None` when `path` isn't a readable directory, or when the walk finds nothing
/// countable. Without that guard tokei on a missing or remote path returned a
/// default report and the bottom bar rendered a confident `0 LOC` — the chip
/// must hide instead of asserting an empty tree.
pub fn scan(path: &Path) -> Option<LocReport> {
    if !path.is_dir() {
        return None;
    }
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
    let report = LocReport::from_langs(langs);
    report.is_measurable().then_some(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_this_crate_and_detects_rust() {
        // Scan this crate's own `src/` — a real tree that always has Rust.
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let report = scan(&src).expect("this crate's src/ is countable");
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

    /// The "0 LOC" bug: a path that isn't there must yield nothing to render,
    /// not a zeroed report the chip would print as a real count.
    #[test]
    fn a_missing_or_empty_path_is_not_measurable() {
        let missing = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("no-such-dir");
        assert!(scan(&missing).is_none(), "missing dir");

        // A file, not a directory.
        let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        assert!(scan(&file).is_none(), "not a directory");

        // A real but empty directory has nothing countable in it.
        let empty = std::env::temp_dir().join(format!("tg-loc-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&empty); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        std::fs::create_dir_all(&empty).unwrap();
        assert!(scan(&empty).is_none(), "empty dir");
        let _ = std::fs::remove_dir_all(&empty); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }
}
