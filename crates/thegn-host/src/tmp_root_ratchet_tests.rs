//! Prevent tests from treating the shared system temp directory as a repository
//! root. Repository and wizard fixtures must live in their own temp directories.

use thegn_core::test_support::ratchet::file_ratchet;

const MANIFEST: &str = env!("CARGO_MANIFEST_DIR");

#[test]
fn tests_never_root_a_repository_at_the_temp_root() {
    const WIZARD: &str = concat!("new_isolated_for_test(", "std::env::temp_dir()");
    const GIT_LOC: &str = concat!("GitLoc::Local(", "std::env::temp_dir()");
    const GIT_DIR: &str = concat!("std::env::temp_dir()", ".join(\".git\")");

    file_ratchet(
        MANIFEST,
        "tmp-root-ratchet.txt",
        &[],
        |_, body| body.contains(WIZARD) || body.contains(GIT_LOC) || body.contains(GIT_DIR),
        "Tests must keep repository, worktree, and wizard roots inside a private tempdir. \
         Never pass the shared temp root itself to git or repository-aware test code.",
    );
}
