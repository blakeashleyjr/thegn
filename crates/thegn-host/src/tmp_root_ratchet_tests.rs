//! Prevent tests from treating the shared system temp directory as a repository
//! root. The host and thegn-core source scans must stay clear of that pattern.
//!
//! Known blind spots: variable indirection (`let root = std::env::temp_dir();
//! new_isolated_for_test(root, ..)`), and imported `temp_dir` short forms.

use thegn_core::test_support::ratchet::file_ratchet;

const MANIFEST: &str = env!("CARGO_MANIFEST_DIR");

fn hits(body: &str) -> bool {
    let flat: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    const WIZARD: &str = concat!("new_isolated_for_test(", "std::env::temp_dir()");
    const GIT_LOC: &str = concat!("GitLoc::Local(", "std::env::temp_dir()");
    const GIT_DIR: &str = concat!("std::env::temp_dir()", ".join(\".git\")");
    const GIT_CMD_BORROWED: &str = concat!("git_cmd(&", "std::env::temp_dir()");
    const GIT_CMD: &str = concat!("git_cmd(", "std::env::temp_dir()");

    flat.contains(WIZARD)
        || flat.contains(GIT_LOC)
        || flat.contains(GIT_DIR)
        || flat.contains(GIT_CMD_BORROWED)
        || flat.contains(GIT_CMD)
}

#[test]
fn tests_never_root_a_repository_at_the_temp_root() {
    file_ratchet(
        MANIFEST,
        "tmp-root-ratchet.txt",
        &[],
        |_, body| hits(body),
        "Tests must keep repository, worktree, and wizard roots inside a private tempdir. \
         Never pass the shared temp root itself to git or repository-aware test code.",
    );
}

#[test]
fn core_tests_never_root_a_repository_at_the_temp_root() {
    file_ratchet(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../thegn-core"),
        "tmp-root-ratchet.txt",
        &[],
        |_, body| hits(body),
        "Tests must keep repository, worktree, and wizard roots inside a private tempdir. \
         Never pass the shared temp root itself to git or repository-aware test code.",
    );
}

#[test]
fn multiline_temp_root_wizard_is_a_hit_and_private_tempdir_is_not() {
    let shared_temp_root =
        "NewWorktreeWizard::new_isolated_for_test(\n            std::env::temp_dir(),\n        )";
    assert!(
        hits(shared_temp_root),
        "whitespace must not hide a shared temp root"
    );

    let private_tempdir = "NewWorktreeWizard::new_isolated_for_test(\n            root.path().to_path_buf(),\n        ); let root = tempfile::tempdir()";
    assert!(
        !hits(private_tempdir),
        "an owned tempdir is an allowed fixture root"
    );

    let shared_git_root = concat!(
        "git_cmd(\n            &",
        "std::env::temp_dir(),\n        )"
    );
    assert!(
        hits(shared_git_root),
        "whitespace must not hide a shared temp root"
    );

    let private_git_root = "git_cmd(scratch.path())";
    assert!(!hits(private_git_root), "a private scratch path is allowed");
}
