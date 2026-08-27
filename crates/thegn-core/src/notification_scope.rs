//! The ONE predicate for "does this notification belong to the active repo's
//! inbox?".
//!
//! It is deliberately FAIL-OPEN on the worktree registry: a row tagged with a
//! path the `worktrees` table does not know — the repo's own main checkout
//! (which never gets a row), an externally-created worktree, a path renamed
//! outside thegn — is KEPT rather than hidden. Only a row tagged with a KNOWN
//! path belonging to a DIFFERENT repo is out of scope.
//!
//! It lives here, alone, because the inbox's display filter and its "clear all"
//! used to carry separate copies and drifted: display was fail-open, the clear
//! fail-closed, so exactly the fail-open rows were shown forever and could never
//! be cleared (THE-68). Both call sites project this function; there is no
//! second copy to drift.

use std::collections::HashSet;

/// Whether a notification tagged `worktree_path` shows in the repo-scoped
/// inbox. `repo_paths` are the active repo's registered worktrees; `all_known`
/// is every path the `worktrees` registry knows, across all repos.
pub fn shows_in_repo_inbox(
    worktree_path: &str,
    repo_paths: &HashSet<String>,
    all_known: &HashSet<String>,
) -> bool {
    worktree_path.is_empty()
        || repo_paths.contains(worktree_path)
        || !all_known.contains(worktree_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(paths: &[&str]) -> HashSet<String> {
        paths.iter().map(|p| (*p).to_string()).collect()
    }

    #[test]
    fn host_global_rows_show_regardless_of_the_registry() {
        // An untagged row belongs to no worktree, so no registry state can
        // attribute it elsewhere.
        assert!(shows_in_repo_inbox("", &set(&[]), &set(&[])));
        assert!(shows_in_repo_inbox(
            "",
            &set(&["/wt/a"]),
            &set(&["/wt/a", "/wt/other"])
        ));
    }

    #[test]
    fn a_registered_path_of_this_repo_shows() {
        assert!(shows_in_repo_inbox(
            "/wt/a",
            &set(&["/wt/a"]),
            &set(&["/wt/a", "/wt/other"])
        ));
    }

    #[test]
    fn a_known_path_of_another_repo_is_out_of_scope() {
        // The only arm that hides anything: the registry positively attributes
        // the path to a different repo.
        assert!(!shows_in_repo_inbox(
            "/wt/other",
            &set(&["/wt/a"]),
            &set(&["/wt/a", "/wt/other"])
        ));
    }

    #[test]
    fn the_repo_main_checkout_has_no_registry_row_so_it_shows() {
        // The main checkout never gets a `worktrees` row, so it is in neither
        // set — fail-open keeps it. Same for an externally-created worktree.
        assert!(shows_in_repo_inbox(
            "/repo/main",
            &set(&["/wt/a"]),
            &set(&["/wt/a", "/wt/other"])
        ));
        assert!(shows_in_repo_inbox("/wt/external", &set(&[]), &set(&[])));
    }

    #[test]
    fn the_arms_are_an_or_not_a_precedence_chain() {
        // A repo path that the global registry read missed (a failed/partial
        // `worktrees()` call) still shows via the first matching arm.
        assert!(shows_in_repo_inbox(
            "/wt/a",
            &set(&["/wt/a"]),
            &set(&["/wt/other"])
        ));
    }
}
