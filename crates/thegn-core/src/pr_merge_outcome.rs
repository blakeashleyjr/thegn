//! Pure classification of a forge merge request and its observed PR state.

use crate::forge::{ForgeError, model::PrStatus};

/// What the forge did with a requested merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// A follow-up read observed the PR as merged.
    Confirmed,
    /// The forge accepted auto-merge and will act when its rules allow.
    AutoMergeArmed,
    /// A direct merge request was accepted, but the PR remains open.
    Requested,
    /// The reviewed head changed before the provider accepted the merge.
    StaleHead { observed: String },
    /// The provider definitively refused the operation.
    Rejected(String),
    /// The request may have been accepted, but its result could not be confirmed.
    Unknown(String),
}

/// Classify the operation result together with a read of the same PR identity.
pub fn classify_merge(
    direct: bool,
    result: &Result<(), ForgeError>,
    after: Option<&PrStatus>,
    expected_head: Option<&str>,
) -> MergeOutcome {
    if after.is_some_and(|status| status.state.eq_ignore_ascii_case("MERGED")) {
        return MergeOutcome::Confirmed;
    }

    if let Err(error) = result {
        if let Some(status) = after
            && expected_head.is_some_and(|head| status.head_ref_oid != head)
        {
            return MergeOutcome::StaleHead {
                observed: status.head_ref_oid.clone(),
            };
        }
        return match error {
            ForgeError::Offline | ForgeError::RateLimited => {
                MergeOutcome::Unknown(error.describe())
            }
            ForgeError::Other(_) if after.is_none() => MergeOutcome::Unknown(error.describe()),
            _ => MergeOutcome::Rejected(error.describe()),
        };
    }

    if direct {
        MergeOutcome::Requested
    } else {
        MergeOutcome::AutoMergeArmed
    }
}

#[cfg(test)]
mod tests {
    use super::{MergeOutcome, classify_merge};
    use crate::forge::{ForgeError, model::PrStatus};

    fn status(state: &str, head: &str) -> PrStatus {
        PrStatus {
            state: state.into(),
            head_ref_oid: head.into(),
            ..PrStatus::default()
        }
    }

    #[test]
    fn classifies_confirmed_armed_requested_stale_rejected_and_unknown() {
        assert_eq!(
            classify_merge(
                true,
                &Ok(()),
                Some(&status("MERGED", "reviewed")),
                Some("reviewed")
            ),
            MergeOutcome::Confirmed
        );
        assert_eq!(
            classify_merge(
                false,
                &Ok(()),
                Some(&status("OPEN", "reviewed")),
                Some("reviewed")
            ),
            MergeOutcome::AutoMergeArmed
        );
        assert_eq!(
            classify_merge(
                true,
                &Ok(()),
                Some(&status("OPEN", "reviewed")),
                Some("reviewed")
            ),
            MergeOutcome::Requested
        );
        assert_eq!(
            classify_merge(
                true,
                &Err(ForgeError::Other("head mismatch".into())),
                Some(&status("OPEN", "new-head")),
                Some("reviewed")
            ),
            MergeOutcome::StaleHead {
                observed: "new-head".into()
            }
        );
        assert_eq!(
            classify_merge(
                true,
                &Err(ForgeError::Unsupported("merge_pr expected head")),
                Some(&status("OPEN", "reviewed")),
                Some("reviewed")
            ),
            MergeOutcome::Rejected("this forge does not support merge_pr expected head".into())
        );
        assert_eq!(
            classify_merge(true, &Err(ForgeError::Offline), None, Some("reviewed")),
            MergeOutcome::Unknown("forge unreachable".into())
        );
        assert_eq!(
            classify_merge(
                true,
                &Err(ForgeError::Other("unclear".into())),
                None,
                Some("reviewed")
            ),
            MergeOutcome::Unknown("unclear".into())
        );
        assert_eq!(
            classify_merge(
                true,
                &Err(ForgeError::Other("rule blocked".into())),
                Some(&status("OPEN", "reviewed")),
                Some("reviewed")
            ),
            MergeOutcome::Rejected("rule blocked".into())
        );
        assert_eq!(
            classify_merge(
                true,
                &Err(ForgeError::Other("no fence".into())),
                Some(&status("OPEN", "changed")),
                None
            ),
            MergeOutcome::Rejected("no fence".into())
        );
    }
}
