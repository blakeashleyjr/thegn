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
    let boundaries = path
        .join(".gitmodules")
        .is_file()
        .then(|| {
            std::fs::read_to_string(path.join(".gitmodules"))
                .ok()
                .and_then(|text| thegn_core::submodule::parse_gitmodules(&text).ok())
                .map(|specs| specs.into_iter().map(|s| s.path).collect::<Vec<_>>())
        })
        .flatten()
        .unwrap_or_default();
    scan_excluding(path, &boundaries)
}

/// Count a worktree while excluding each normalized submodule directory and
/// all of its descendants. The boundary list is repository-relative and is
/// compared component-wise by the core helper before it is joined to root.
pub fn scan_excluding(path: &Path, submodule_paths: &[String]) -> Option<LocReport> {
    if !path.is_dir() {
        return None;
    }
    let excludes: Vec<String> = submodule_paths
        .iter()
        .filter(|candidate| thegn_core::submodule::validate_submodule_path(candidate).is_ok())
        .map(|candidate| format!("**/{candidate}"))
        .collect();
    let exclude_refs: Vec<&str> = excludes.iter().map(String::as_str).collect();
    let mut languages = tokei::Languages::new();
    let config = tokei::Config {
        treat_doc_strings_as_comments: Some(true),
        ..Default::default()
    };
    languages.get_statistics(&[path.to_path_buf()], &exclude_refs, &config);
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

    #[test]
    fn scan_excludes_submodule_source_but_keeps_superproject_source() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("vendor/lib/src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(
            dir.path().join("vendor/lib/src/lib.rs"),
            "fn vendored() {}\n",
        )
        .unwrap();

        let report = scan_excluding(dir.path(), &["vendor/lib".into()]).unwrap();
        assert_eq!(report.langs.iter().map(|lang| lang.files).sum::<usize>(), 1);
        assert!(report.total_code > 0);
    }

    #[test]
    fn malformed_gitmodules_does_not_create_an_unsafe_boundary() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("vendor/lib")).unwrap();
        std::fs::write(
            dir.path().join(".gitmodules"),
            "[submodule \"lib\"]\npath = ../escape\nurl = x\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("vendor/lib/lib.rs"), "fn vendored() {}\n").unwrap();
        assert!(scan(dir.path()).is_some());
    }
}
