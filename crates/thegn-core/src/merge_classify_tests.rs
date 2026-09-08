//! Every fixture here is a real conflict from the THE-32 / THE-60 reconciles,
//! so the classifier is checked against the merges that motivated it rather
//! than against invented shapes.

use super::*;

/// `crates/thegn-core/src/config.rs`, THE-32 vs main: this lane added
/// `git_submodules`, main added `editor_provider`, at the same point in the
/// override struct. The base section is empty.
const ADDITIVE_CONFIG: &str = "\
struct Overrides {
<<<<<<< HEAD
    pub git_submodules: Option<SubmoduleMode>,
||||||| caef2f0e
=======
    pub editor_provider: Option<EditorProvider>,
>>>>>>> main
}
";

/// `crates/thegn-host/src/pr_view.rs`, same merge: main replaced the flat
/// diff-line model with a row model while this lane added submodule filtering
/// to the same function. The base section is NOT empty.
const RESTRUCTURE_PR_VIEW: &str = "\
            let n = match self.open_file {
<<<<<<< HEAD
                None => self.diff.as_ref().map_or(0, |d| d.files.len()),
                Some(i) => self.file_row_count(i),
||||||| caef2f0e
                None => self.diff.as_ref().map_or(0, |d| d.files.len()),
                Some(i) => self.open_file_lines(i).len(),
=======
                None => {
                    self.diff.as_ref().map_or(0, |d| d.files.len())
                        + self.files_feedback_rows().len()
                }
                Some(i) => self.open_file_rows(i).len(),
>>>>>>> main
            };
";

#[test]
fn an_empty_base_is_a_concurrent_addition_that_still_needs_review() {
    let hunks = classify_file(ADDITIVE_CONFIG);
    assert_eq!(hunks.len(), 1);
    assert_eq!(hunks[0].class, HunkClass::ConcurrentAdd);
    assert_eq!(hunks[0].line, 2);
    assert_eq!(
        hunks[0].ours_hint,
        "pub git_submodules: Option<SubmoduleMode>,"
    );
    assert_eq!(
        hunks[0].theirs_hint,
        "pub editor_provider: Option<EditorProvider>,"
    );
}

#[test]
fn incompatible_concurrent_declarations_are_never_called_safe() {
    let conflict = "<<<<<<< HEAD\nfn parse() {}\n||||||| base\n=======\nfn parse() { panic!() }\n>>>>>>> main\n";
    let hunks = classify_file(conflict);
    assert_eq!(hunks[0].class, HunkClass::ConcurrentAdd);
    let out = render_chunk_skeleton(
        "THE-1",
        &[FileConflicts {
            path: "src/lib.rs".into(),
            hunks,
            inspection_error: None,
        }],
    );
    assert!(!out.contains("Additive — keep both sides"));
    assert!(out.contains("DECISION: _(state it)_"));
}

#[test]
fn a_populated_base_is_a_restructure() {
    let hunks = classify_file(RESTRUCTURE_PR_VIEW);
    assert_eq!(hunks.len(), 1);
    assert_eq!(
        hunks[0].class,
        HunkClass::Restructure,
        "both sides rewrote code that existed — 'keep both' does not compile here"
    );
}

#[test]
fn the_the_32_split_matches_what_was_written_by_hand() {
    // Empty-base additions remain distinct from rewrites, but both need decisions.
    let mixed = format!("{ADDITIVE_CONFIG}\n{RESTRUCTURE_PR_VIEW}");
    let hunks = classify_file(&mixed);
    assert_eq!(hunks.len(), 2);
    let f = FileConflicts {
        path: "crates/thegn-core/src/mixed.rs".into(),
        hunks,
        inspection_error: None,
    };
    assert_eq!(f.concurrent_add(), 1);
    assert_eq!(f.restructure(), 1);
}

#[test]
fn without_a_base_section_everything_needs_a_look() {
    // Default `merge.conflictStyle` records no base. The conservative answer is
    // the only safe one: a false safe classification would tell a worker to keep both
    // sides of a rewrite.
    let no_base = "\
<<<<<<< HEAD
    a();
=======
    b();
>>>>>>> main
";
    let hunks = classify_file(no_base);
    assert_eq!(hunks.len(), 1);
    assert_eq!(hunks[0].class, HunkClass::Restructure);
}

#[test]
fn clean_and_malformed_files_yield_nothing_rather_than_a_guess() {
    assert!(classify_file("fn main() {}\n").is_empty());
    assert!(classify_file("").is_empty());
    // Opened and never closed: skipped, not guessed at.
    assert!(classify_file("<<<<<<< HEAD\nours\n").is_empty());
    // A second hunk opening before the first closes — the first is malformed,
    // and the marker pair must not be matched across hunks.
    let tangled = "\
<<<<<<< HEAD
ours
<<<<<<< HEAD
ours2
||||||| base
b
=======
theirs2
>>>>>>> main
";
    let hunks = classify_file(tangled);
    assert_eq!(
        hunks.len(),
        1,
        "only the well-formed inner hunk is reported"
    );
    assert_eq!(hunks[0].class, HunkClass::Restructure);
}

#[test]
fn multiple_hunks_report_their_own_lines() {
    let two = format!("{ADDITIVE_CONFIG}{ADDITIVE_CONFIG}");
    let hunks = classify_file(&two);
    assert_eq!(hunks.len(), 2);
    assert!(
        hunks[0].line < hunks[1].line,
        "line numbers must be absolute in the file, not per-hunk"
    );
    assert!(hunks.iter().all(|h| h.class == HunkClass::ConcurrentAdd));
}

#[test]
fn a_long_hint_is_clipped() {
    let long = format!(
        "<<<<<<< HEAD\n    {}\n||||||| b\n=======\n    y\n>>>>>>> main\n",
        "x".repeat(200)
    );
    let hunks = classify_file(&long);
    assert!(hunks[0].ours_hint.chars().count() <= HINT_MAX + 1);
    assert!(hunks[0].ours_hint.ends_with('…'));
}

#[test]
fn the_skeleton_names_both_groups_and_refuses_to_decide() {
    let files = vec![
        FileConflicts {
            path: "crates/thegn-core/src/config.rs".into(),
            hunks: classify_file(ADDITIVE_CONFIG),
            inspection_error: None,
        },
        FileConflicts {
            path: "crates/thegn-host/src/pr_view.rs".into(),
            hunks: classify_file(RESTRUCTURE_PR_VIEW),
            inspection_error: None,
        },
    ];
    let out = render_chunk_skeleton("THE-32", &files);
    assert!(out.contains("# \"THE-32\" reconcile"));
    assert!(out.contains("\"crates/thegn-core/src/config.rs\""));
    assert!(out.contains("Concurrent additions — NEEDS A DECISION"));
    assert!(out.contains("NEEDS A DECISION"));
    assert!(
        out.contains("DECISION: _(state it)_"),
        "the skeleton must leave the restructure calls to a human, not invent them"
    );
    assert!(
        out.contains("Do not apply a blanket \"keep both sides\" rule"),
        "the advice that caused the problem must be contradicted up front"
    );
    // The counts a Lead reads first.
    assert!(out.contains("2 hunk(s) total"));
    assert!(out.contains("1 concurrent insertion(s) and 1 rewrite(s) need a decision"));
    assert!(out.contains("DRAFT ONLY — do not dispatch"));
}

#[test]
fn an_unclassified_conflict_is_prominent_and_blocks_dispatch() {
    let files = vec![FileConflicts {
        path: "asset.bin".into(),
        hunks: vec![],
        inspection_error: Some("not UTF-8; likely binary".into()),
    }];
    let out = render_chunk_skeleton("THE-99", &files);
    assert!(out.contains("Unclassified conflicts — INSPECT BEFORE DISPATCHING"));
    assert!(out.contains("\"asset.bin\" — \"not UTF-8; likely binary\""));
    assert!(out.contains("zero parsed hunks means the file is resolved"));
    assert!(out.contains("DRAFT ONLY"));
}

#[test]
fn repository_controlled_text_cannot_inject_markdown_structure() {
    let files = vec![FileConflicts {
        path: "safe.rs`\n\n## INJECTED".into(),
        hunks: vec![Hunk {
            line: 1,
            class: HunkClass::Restructure,
            ours_hint: "`\n## BAD".into(),
            theirs_hint: "ok".into(),
        }],
        inspection_error: None,
    }];
    let out = render_chunk_skeleton("issue\n## BAD", &files);
    assert!(
        !out.lines()
            .any(|line| line == "## INJECTED" || line == "## BAD")
    );
    assert!(out.contains("\\n\\n## INJECTED"));
}

#[test]
fn explicit_decisions_produce_a_dispatchable_chunk() {
    let file = FileConflicts {
        path: "src/lib.rs".into(),
        hunks: classify_file(RESTRUCTURE_PR_VIEW),
        inspection_error: None,
    };
    let key = format!("{}:{}", file.path, file.hunks[0].line);
    let decisions = BTreeMap::from([(key, "keep the row model and port filtering".into())]);
    let out = render_chunk("THE-32", &[file], &decisions);
    assert!(out.contains("**READY — every conflict has an explicit decision.**"));
    assert!(!out.contains("_(state it)_"));
}
