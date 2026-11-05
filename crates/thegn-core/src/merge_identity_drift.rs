//! The text contract of an identity-drift hold on a merge-queue row (THE-230).

/// Prefix of the drain's candidate-identity hold (status `agent_blocked`).
pub const DRIFT_PREFIX: &str = "identity drift: ";
/// Prefix of the fixer-checkout hold written through an infra hold.
pub const WORKTREE_DRIFT_PREFIX: &str = "worktree identity drift: ";
/// Suffix appended to the candidate hold.
pub const REENQUEUE_SUFFIX: &str = "; re-enqueue with `thegn merge add` to adopt the new tip";

/// Detail text for a candidate identity drift hold: `identity drift: <reason>; re-enqueue ...`.
pub fn candidate_drift_detail(reason: &str) -> String {
    format!("{DRIFT_PREFIX}{reason}{REENQUEUE_SUFFIX}")
}

/// Detail text for a fixer-checkout identity drift hold.
pub fn worktree_drift_detail(reason: &str) -> String {
    format!("{WORKTREE_DRIFT_PREFIX}{reason}")
}

/// The reason when `detail` is an identity-drift hold, else `None`.
///
/// A detail that merely contains the words (for example a candidate-unreadable
/// hold) is not drift: the prefix must lead.
pub fn drift_reason(detail: &str) -> Option<&str> {
    let rest = detail
        .strip_prefix(DRIFT_PREFIX)
        .or_else(|| detail.strip_prefix(WORKTREE_DRIFT_PREFIX))?;
    Some(rest.strip_suffix(REENQUEUE_SUFFIX).unwrap_or(rest))
}

#[derive(serde::Serialize)]
pub struct IdentityDrift<'a> {
    pub reason: &'a str,
}

#[cfg(test)]
mod tests {
    use super::{candidate_drift_detail, drift_reason, worktree_drift_detail};
    use crate::db::MergeQueueRow;

    fn row(error_detail: Option<String>) -> MergeQueueRow {
        MergeQueueRow {
            worktree: "/repo/worktree".to_owned(),
            branch: "feature".to_owned(),
            target_branch: "main".to_owned(),
            status: "agent_blocked".to_owned(),
            queued_at: 1,
            updated_at: 2,
            result_oid: None,
            conflict_paths: None,
            error_detail,
            location: "local".to_owned(),
            agent_attempts: 0,
        }
    }

    #[test]
    fn candidate_detail_round_trips_to_its_reason() {
        let detail = candidate_drift_detail(
            "worktree registration changed since enqueue (removed and re-created?)",
        );
        assert_eq!(
            drift_reason(&detail),
            Some("worktree registration changed since enqueue (removed and re-created?)")
        );
    }

    #[test]
    fn worktree_detail_round_trips_to_its_reason() {
        let detail = worktree_drift_detail("checkout branch changed");
        assert_eq!(drift_reason(&detail), Some("checkout branch changed"));
    }

    #[test]
    fn unreadable_and_free_text_are_not_drift() {
        assert_eq!(
            drift_reason("candidate unreadable: Git command timed out"),
            None
        );
        assert_eq!(drift_reason("note: identity drift: x"), None);
        assert_eq!(drift_reason(""), None);
    }

    #[test]
    fn row_serializes_identity_drift_object_and_null() {
        let drifted = row(Some(candidate_drift_detail("r")));
        let value = serde_json::to_value(&drifted).unwrap();
        assert_eq!(value["identity_drift"]["reason"], "r");
        assert_eq!(value["worktree"], "/repo/worktree");
        assert_eq!(value["location"], "local");
        assert_eq!(value["agent_attempts"], 0);
        assert!(value["agent_attempts"].is_number());

        let plain = row(None);
        let value = serde_json::to_value(&plain).unwrap();
        assert!(value["identity_drift"].is_null());
    }

    #[test]
    fn row_key_set_is_stable() {
        let encoded = serde_json::to_string(&row(None)).unwrap();
        let fields = [
            "worktree",
            "branch",
            "target_branch",
            "status",
            "queued_at",
            "updated_at",
            "result_oid",
            "conflict_paths",
            "error_detail",
            "location",
            "agent_attempts",
            "identity_drift",
        ];
        let mut previous_position = 0;
        for field in fields {
            let needle = format!("\"{field}\":");
            let position = encoded.find(&needle).unwrap();
            assert!(
                position >= previous_position,
                "field order changed: {field}"
            );
            previous_position = position;
        }
    }
}
