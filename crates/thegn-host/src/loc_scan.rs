//! The `tokei` walk behind the bottom-bar `LOC` chip: turn a worktree path into
//! a per-language [`LocReport`]. Lives off the hydration god-file; the caller is
//! `measure::loc`, which owns the DB cache and the scheduling around it. tokei
//! walks the whole tree, so this runs on the background measurement lane and
//! must never be called on the loop or on the interactive hydration lane.

use std::path::Path;

use thegn_core::loc::{LocLang, LocReport};

/// Result of one filesystem count. A partial report is useful for diagnostics,
/// but must never be published as a fresh complete cache row.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ScanOutcome {
    /// The root is absent or is not a directory.
    Unavailable,
    /// No language parse failure was surfaced by Tokei. `None` means there was
    /// nothing measurable. Tokei's ignore walker logs directory errors and does
    /// not expose them in `Languages`, so this is not a certification that every
    /// directory was traversed; that observability gap remains explicit here.
    Complete(Option<LocReport>),
    /// Tokei parsed some data but marked at least one language inaccurate.
    /// The report is retained only for tests/diagnostics; callers must not cache it.
    Incomplete(LocReport),
}

/// Count lines under `path` with tokei and fold into a sorted [`LocReport`].
/// Doc strings count as comments (matching the previous behavior).
///
/// `Unavailable` when `path` isn't a readable directory, and `Complete(None)`
/// when the walk finds nothing countable. Without that distinction tokei on a
/// missing or remote path returned a default report and the bottom bar rendered
/// a confident `0 LOC` — the chip must hide instead of asserting an empty tree.
pub(crate) fn scan(path: &Path) -> ScanOutcome {
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
pub(crate) fn scan_excluding(path: &Path, submodule_paths: &[String]) -> ScanOutcome {
    if !path.is_dir() {
        return ScanOutcome::Unavailable;
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
    outcome_from_languages(&languages)
}

/// Convert Tokei's aggregate while retaining its completeness marker. A
/// language can be marked inaccurate after every attempted file read failed,
/// leaving no positive line count to survive the row filter.
fn outcome_from_languages(languages: &tokei::Languages) -> ScanOutcome {
    // Check this before filtering zero-line languages.
    let incomplete = languages.iter().any(|(_, lang)| lang.inaccurate);
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
    if incomplete {
        ScanOutcome::Incomplete(report)
    } else {
        ScanOutcome::Complete(report.is_measurable().then_some(report))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_this_crate_and_detects_rust() {
        // Scan this crate's own `src/` — a real tree that always has Rust.
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let ScanOutcome::Complete(Some(report)) = scan(&src) else {
            panic!("this crate's src/ is countable");
        };
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
        assert_eq!(scan(&missing), ScanOutcome::Unavailable);

        // A file, not a directory.
        let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        assert_eq!(scan(&file), ScanOutcome::Unavailable);

        // A real but empty directory has nothing countable in it.
        let empty = std::env::temp_dir().join(format!("tg-loc-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&empty); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(scan(&empty), ScanOutcome::Complete(None));
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

        let ScanOutcome::Complete(Some(report)) =
            scan_excluding(dir.path(), &["vendor/lib".into()])
        else {
            panic!("superproject source is countable");
        };
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
        assert!(matches!(scan(dir.path()), ScanOutcome::Complete(Some(_))));
    }

    fn synthetic_language(code: usize, inaccurate: bool) -> tokei::Language {
        tokei::Language {
            code,
            reports: vec![tokei::Report::new("fixture.rs".into())],
            inaccurate,
            ..Default::default()
        }
    }

    #[test]
    fn inaccurate_positive_language_is_not_a_complete_report() {
        let mut languages = tokei::Languages::new();
        languages.insert(tokei::LanguageType::Rust, synthetic_language(4, false));
        languages.insert(tokei::LanguageType::Python, synthetic_language(2, true));

        // A valid count cannot make a report look fresh when another language
        // failed, even though the failed language has no usable row.
        let ScanOutcome::Incomplete(report) = outcome_from_languages(&languages) else {
            panic!("an inaccurate language must invalidate the report");
        };
        assert!(report.total_code > 0);
    }

    #[test]
    fn inaccurate_zero_line_language_is_seen_before_filtering() {
        let mut languages = tokei::Languages::new();
        languages.insert(tokei::LanguageType::Rust, synthetic_language(4, false));
        languages.insert(
            tokei::LanguageType::Python,
            tokei::Language {
                inaccurate: true,
                ..Default::default()
            },
        );
        let ScanOutcome::Incomplete(report) = outcome_from_languages(&languages) else {
            panic!("a zero-line inaccurate language must invalidate the report");
        };
        assert_eq!(
            report.langs.len(),
            1,
            "failed zero-line language has no row"
        );
    }

    /// On a non-root Unix test runner, an unreadable source file exercises the
    /// real Tokei parse-error path. Root can still read mode-000 files, so that
    /// environment is an explicit skip rather than a false failure.
    #[test]
    fn unreadable_source_is_not_published_as_complete() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("secret.rs");
        std::fs::write(&source, "fn secret() {}\n").unwrap();
        // The platform helper changes permissions and checks the boundary
        // independently of the scan result. A complete empty result is not
        // evidence that this test exercised a read failure: root can bypass
        // mode bits.
        if !crate::platform::test_make_unreadable(&source) {
            return;
        }
        let outcome = scan(dir.path());
        assert!(matches!(outcome, ScanOutcome::Incomplete(_)));
    }
}
