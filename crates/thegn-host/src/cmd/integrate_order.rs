//! Pure selection helpers for the ordered integration CLI flags.

use anyhow::{Result, bail};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FoldState {
    Deferred(String),
    GateFailed,
    Other,
}

/// Render the optional handoff for fixing an ordered fold across lanes.
pub(crate) fn fixup_hint(
    into: &str,
    into_worktree: Option<&Path>,
    state: FoldState,
) -> Option<String> {
    let worktree = into_worktree?;
    if !matches!(state, FoldState::Deferred(_) | FoldState::GateFailed) {
        return None;
    }
    Some(format!(
        "Fix across lanes: thegn session open --agent <agent> --worktree {} --stage integration-fixup --brief-file <brief> --vars {}",
        thegn_core::util::sh_quote(&worktree.display().to_string()),
        thegn_core::util::sh_quote(&format!("target={into}")),
    ))
}

/// Resolve an explicit branch order to indexes in the discovered candidates.
///
/// The caller uses the indexes to preserve the full candidate records (tips,
/// worktree paths and snapshot identities) while replacing only their order.
pub(crate) fn ordered_indices(
    candidate_names: &[String],
    requested: &[String],
) -> Result<Vec<usize>> {
    if requested.is_empty() {
        bail!("--order must name at least one branch");
    }

    let mut indexes = Vec::with_capacity(requested.len());
    let mut unknown = Vec::new();
    let mut duplicate = Vec::new();
    for raw_name in requested {
        let name = raw_name.trim();
        let Some(index) = candidate_names
            .iter()
            .position(|candidate| candidate == name)
        else {
            if !unknown.contains(&name) {
                unknown.push(name);
            }
            continue;
        };
        if indexes.contains(&index) {
            if !duplicate.contains(&name) {
                duplicate.push(name);
            }
        } else {
            indexes.push(index);
        }
    }
    if !unknown.is_empty() {
        bail!(
            "unknown branch name(s): {}; candidates: {}",
            unknown.join(", "),
            candidate_names.join(", ")
        );
    }
    if !duplicate.is_empty() {
        bail!(
            "duplicate branch name(s) in --order: {}",
            duplicate.join(", ")
        );
    }
    Ok(indexes)
}

/// Validate the target branch probes used by `--into`.
pub(crate) fn validate_into(branch: &str, local_exists: bool, remote_tracking: bool) -> Result<()> {
    if remote_tracking || branch.starts_with("refs/remotes/") {
        bail!("--into must name a local branch, not a remote-tracking name: {branch}");
    }
    if !local_exists {
        bail!("--into target branch does not exist as a local branch: {branch}");
    }
    Ok(())
}

/// Return the index at which an ordered fold should resume.
pub(crate) fn from_index(names: &[String], from: &str) -> Result<usize> {
    let from = from.trim();
    names
        .iter()
        .position(|name| name.trim() == from)
        .ok_or_else(|| anyhow::anyhow!("--from branch is not in --order: {from}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).into()).collect()
    }

    #[test]
    fn ordered_indices_preserves_the_requested_order() {
        assert_eq!(
            ordered_indices(&names(&["a", "b", "c"]), &names(&["c", "a"])).unwrap(),
            [2, 0]
        );
    }

    #[test]
    fn unknown_names_are_reported_together() {
        let error = ordered_indices(&names(&["a", "b"]), &names(&["missing", "also-missing"]))
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "unknown branch name(s): missing, also-missing; candidates: a, b"
        );
    }

    #[test]
    fn validate_into_rejects_remote_tracking_and_missing_targets() {
        assert!(validate_into("origin/main", true, true).is_err());
        assert!(validate_into("main", false, false).is_err());
        assert!(validate_into("main", true, false).is_ok());
    }

    #[test]
    fn from_index_returns_the_resume_slice() {
        let names = names(&["a", "b", "c"]);
        let index = from_index(&names, "b").unwrap();
        assert_eq!(
            names[index..]
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["b", "c"]
        );
        assert!(from_index(&names, "missing").is_err());
    }

    #[test]
    fn from_index_trims_the_requested_order_entry() {
        let names = names(&["gone", " b ", "c"]);
        assert_eq!(from_index(&names, "b").unwrap(), 1);
    }

    #[test]
    fn fixup_hint_names_the_deferred_fold_target() {
        let worktree = tempfile::tempdir().expect("temporary worktree");
        assert_eq!(
            fixup_hint(
                "stack",
                Some(worktree.path()),
                FoldState::Deferred("lane-a".into())
            ),
            Some(format!(
                "Fix across lanes: thegn session open --agent <agent> --worktree {} --stage integration-fixup --brief-file <brief> --vars target=stack",
                worktree.path().display()
            ))
        );
    }

    #[test]
    fn fixup_hint_names_a_failed_gate() {
        let worktree = tempfile::tempdir().expect("temporary worktree");
        assert!(fixup_hint("stack", Some(worktree.path()), FoldState::GateFailed).is_some());
    }

    #[test]
    fn fixup_hint_quotes_worktree_and_target() {
        let hint = fixup_hint("a;b", Some(Path::new("/tmp/my wt")), FoldState::GateFailed)
            .expect("failed gate should produce a hint");
        assert!(hint.contains("--worktree '/tmp/my wt'"));
        assert!(hint.contains("--vars 'target=a;b'"));
    }

    #[test]
    fn fixup_hint_requires_a_worktree() {
        assert!(fixup_hint("stack", None, FoldState::Deferred("lane-a".into())).is_none());
    }

    #[test]
    fn fixup_hint_never_appears_without_an_into_worktree_in_push_mode() {
        assert!(fixup_hint("stack", None, FoldState::Deferred("lane-a".into())).is_none());
    }
}
