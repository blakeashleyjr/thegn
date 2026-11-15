//! Exotic-path round-trip matrix (THE-376): names with spaces, tabs, newlines,
//! leading dashes, pathspec magic, globs, quotes, backslashes and non-ASCII
//! must come back from `status()` and `diff_files()` byte-for-byte, once each.
//!
//! Real `git` runs against throwaway repos. Names the filesystem refuses are
//! skipped through `testenv::require_or_skip` with a pinned reason.
//!
//! Out of scope: invalid-UTF-8 names are unsupported here (paths are `String`;
//! byte identity needs THE-368), and SSH/provider transports.

use std::path::Path;

use thegn_core::config::GitBackendKind;
use thegn_core::remote::GitLoc;
use thegn_core::testenv::require_or_skip;
use thegn_core::util::git_cmd;
use thegn_svc::git::backend_for;

/// `(name, reason a filesystem may refuse it)`.
const NAMES: &[(&str, &str)] = &[
    ("a b", "space in name"),
    ("tab\there", "tab in name"),
    ("new\nline", "newline in name"),
    ("-leading-dash", "leading dash"),
    ("--double", "leading double dash"),
    (":(glob)magic", "pathspec magic prefix"),
    ("*star", "glob star (refused on Windows)"),
    ("[br]acket", "glob bracket"),
    ("back\\slash", "backslash (a separator on Windows)"),
    ("quote\"q", "double quote (refused on Windows)"),
    ("\u{fc}n\u{ef}c\u{f6}de", "non-ASCII name"),
    (
        "\u{65e5}\u{672c}\u{8a9e}/\u{6df1}\u{3044}/path.txt",
        "nested non-ASCII path",
    ),
    ("trailing space ", "trailing space (trimmed on Windows)"),
];

fn git_available() -> bool {
    git_cmd(Path::new("."))
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success())
}

fn git(dir: &Path, args: &[&str]) {
    let out = git_cmd(dir)
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=tester",
            "-c",
            "user.email=tester@example.invalid",
            "-c",
            "core.quotepath=true",
        ])
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("seed.txt"), "seed\n").expect("write seed");
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "seed"]);
}

fn write_file(dir: &Path, name: &str, body: &str) -> std::io::Result<()> {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, body)
}

/// The names the filesystem accepts; others are skipped with a reason.
fn usable_names() -> Vec<&'static str> {
    let probe = tempfile::tempdir().expect("probe tempdir");
    let mut usable = Vec::new();
    for (name, reason) in NAMES {
        let accepted = write_file(probe.path(), name, "x\n").is_ok();
        if require_or_skip(&format!("filesystem accepts {name:?} ({reason})"), accepted) {
            usable.push(*name);
        }
    }
    usable
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

const BACKENDS: [(&str, GitBackendKind); 2] =
    [("cli", GitBackendKind::Cli), ("gix", GitBackendKind::Gix)];

#[test]
fn modified_paths_round_trip_exactly() {
    if !require_or_skip("git on PATH", git_available()) {
        return;
    }
    let names = usable_names();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path();
    init_repo(repo);
    for name in &names {
        write_file(repo, name, "one\n").expect("create");
    }
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "base"]);
    for name in &names {
        write_file(repo, name, "one\ntwo\n").expect("modify");
    }
    let expected = sorted(names.iter().map(|n| (*n).to_string()).collect());
    let loc = GitLoc::Local(repo.to_path_buf());
    for (label, kind) in BACKENDS {
        let backend = backend_for(kind);
        let status = backend.status(&loc).expect("status");
        assert!(
            status.iter().all(|s| s.unstaged == 'M'),
            "{label}: modified rows are unstaged M"
        );
        assert_eq!(
            sorted(status.into_iter().map(|s| s.path).collect()),
            expected,
            "{label} status"
        );
        let diff = backend.diff_files(&loc, "HEAD").expect("diff_files");
        assert!(
            diff.iter().all(|d| d.added == 1 && d.deleted == 0),
            "{label}: one added line each"
        );
        assert_eq!(
            sorted(diff.into_iter().map(|d| d.path).collect()),
            expected,
            "{label} diff_files"
        );
    }
}

#[test]
fn untracked_paths_round_trip_exactly() {
    if !require_or_skip("git on PATH", git_available()) {
        return;
    }
    let names = usable_names();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path();
    init_repo(repo);
    for name in &names {
        write_file(repo, name, "new\n").expect("create");
    }
    // Plain `status` collapses a wholly untracked directory to `dir/`.
    let expected = sorted(
        names
            .iter()
            .map(|n| match n.split_once('/') {
                Some((top, _)) => format!("{top}/"),
                None => (*n).to_string(),
            })
            .collect(),
    );
    let loc = GitLoc::Local(repo.to_path_buf());
    for (label, kind) in BACKENDS {
        let backend = backend_for(kind);
        let status = backend.status(&loc).expect("status");
        assert!(
            status.iter().all(|s| s.staged == '?' && s.unstaged == '?'),
            "{label}: untracked rows are ??"
        );
        assert_eq!(
            sorted(status.into_iter().map(|s| s.path).collect()),
            expected,
            "{label} status"
        );
        // Untracked files are not part of a diff against HEAD.
        let diff = backend.diff_files(&loc, "HEAD").expect("diff_files");
        assert!(diff.is_empty(), "{label}: untracked not in diff: {diff:?}");
    }
}

#[test]
fn renamed_paths_round_trip_exactly() {
    if !require_or_skip("git on PATH", git_available()) {
        return;
    }
    let names = usable_names();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path();
    init_repo(repo);
    let origs: Vec<String> = (0..names.len()).map(|i| format!("orig{i}.txt")).collect();
    for orig in &origs {
        write_file(repo, orig, "content\n").expect("create orig");
    }
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "base"]);
    for (orig, name) in origs.iter().zip(&names) {
        if let Some(parent) = Path::new(name).parent() {
            std::fs::create_dir_all(repo.join(parent)).expect("mkdir");
        }
        git(repo, &["mv", "--", orig, name]);
    }
    // Both backends run with `--no-renames`: every rename is a delete plus an
    // add, so both spellings must appear exactly once.
    let expected = sorted(
        origs
            .iter()
            .cloned()
            .chain(names.iter().map(|n| (*n).to_string()))
            .collect(),
    );
    let loc = GitLoc::Local(repo.to_path_buf());
    for (label, kind) in BACKENDS {
        let backend = backend_for(kind);
        let status = backend.status(&loc).expect("status");
        assert_eq!(
            sorted(status.into_iter().map(|s| s.path).collect()),
            expected,
            "{label} status"
        );
        let diff = backend.diff_files(&loc, "HEAD").expect("diff_files");
        assert_eq!(
            sorted(diff.into_iter().map(|d| d.path).collect()),
            expected,
            "{label} diff_files"
        );
    }
}
