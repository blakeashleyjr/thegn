//! Pure selection helpers for the ordered integration CLI flags.

use anyhow::{Result, bail};

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
            if !unknown.iter().any(|known: &&str| *known == name) {
                unknown.push(name);
            }
            continue;
        };
        if indexes.contains(&index) {
            if !duplicate.iter().any(|known: &&str| *known == name) {
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
        .position(|name| name == from)
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
}
