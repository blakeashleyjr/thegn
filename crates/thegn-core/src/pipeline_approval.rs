//! Pipeline approvals — "a person reviewed this exact tree and said yes",
//! written down so a machine can check it.
//!
//! # What an approval is for
//!
//! The supervisor is allowed to do the mechanical half of a pipeline: run a
//! stage's validation, record what came back, and advance a lane whose declared
//! `requires` are all satisfied. One of those requirements is `approval`, and it
//! is the one that keeps human judgment in the loop **without keeping a human in
//! the polling loop**: the reviewer records the approval whenever they get to
//! it, and the supervisor acts on it whenever it next runs, hours later and with
//! nobody watching.
//!
//! For that to be safe rather than merely convenient, an approval has to be a
//! statement about a *specific tree*, not about a lane in general. Hence the
//! central rule here:
//!
//! > **An approval is bound to a commit.** Approve a diff, then push another
//! > commit, and the approval no longer applies — it reports
//! > [`ApprovalState::Superseded`], and the supervisor stops.
//!
//! Without that, "approved" degrades into "approved once, long ago, before the
//! thing you are about to land existed", which is how review gates become
//! ceremony. The cost is one extra approval per revision, which is the correct
//! cost: a revision is new code and nobody has read it.
//!
//! # What this module is NOT
//!
//! It does not grant approvals, store them, or decide who may. Granting is an
//! operator-scoped write on a surface outside the daemon, precisely so that no
//! code path the supervisor runs can manufacture its own authorization. This
//! module only answers, purely: *given this record and this tree, does the
//! approval still hold?*
//!
//! Pure: a record, a sha and a clock reading in; a state out. No I/O.

use serde::{Deserialize, Serialize};

/// The shortest commit prefix an approval may be recorded against.
///
/// git's own default abbreviation is 7, and a shorter prefix stops being an
/// identification: at 4 hex characters a repository of this size has collisions
/// by the birthday bound, and "approved" must never match the wrong tree.
pub const MIN_SHA_PREFIX: usize = 7;

/// One recorded approval: a named person accepted a named stage's output at a
/// named commit.
///
/// `stage` is the stage whose **output** was approved — the parent of whatever
/// transition the approval unblocks. Approving `code` is what lets the
/// supervisor dispatch the stage that consumes `code`'s artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    /// Tracker issue id (`"<provider>:<key>"`), the lane's identity.
    pub issue_id: String,
    /// The stage whose output this approves.
    pub stage: String,
    /// The commit the reviewer actually read. May be an abbreviated sha of at
    /// least [`MIN_SHA_PREFIX`] characters.
    pub commit_sha: String,
    /// Who approved. Recorded for the audit trail, never interpreted.
    pub approver: String,
    /// Optional free text — typically the scope the approval is conditional on.
    pub note: Option<String>,
    /// When it was granted (epoch ms).
    pub granted_at_ms: i64,
    /// An explicit deadline set at grant time, independent of any configured
    /// TTL. `None` means "no deadline of its own"; the configured TTL still
    /// applies (see [`state_for`]).
    pub expires_at_ms: Option<i64>,
    /// When it was revoked, if it was. A revoked approval is kept rather than
    /// deleted: "this was approved and then withdrawn" is a different fact from
    /// "this was never approved", and the audit trail needs both.
    pub revoked_at_ms: Option<i64>,
}

/// Whether an approval still authorizes anything, and if not, why not.
///
/// Every non-[`Live`](ApprovalState::Live) variant carries enough to explain
/// itself to a person without a second lookup — a refusal that says only "no"
/// sends the reviewer hunting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalState {
    /// Valid for this tree, right now.
    Live,
    /// Nothing was ever recorded for this lane and stage.
    Absent,
    /// Recorded, then explicitly withdrawn.
    Revoked {
        /// When it was withdrawn (epoch ms).
        at_ms: i64,
    },
    /// Recorded against a different commit than the lane's current tip — the
    /// lane has moved since it was read.
    Superseded {
        /// The commit that was actually approved.
        approved: String,
        /// The lane's tip now.
        head: String,
    },
    /// Recorded long enough ago that it has lapsed.
    Expired {
        /// The deadline that passed (epoch ms).
        deadline_ms: i64,
    },
    /// The record is unusable: an empty or too-short commit prefix. Treated as
    /// a refusal rather than silently matching everything, which is what a
    /// prefix comparison against `""` would otherwise do.
    Malformed {
        /// Why the record cannot be evaluated.
        why: &'static str,
    },
}

impl ApprovalState {
    /// Does this state authorize the supervisor to proceed? Only
    /// [`Self::Live`] does.
    pub fn is_live(&self) -> bool {
        matches!(self, Self::Live)
    }

    /// A short token for tables and `--json`.
    pub fn token(&self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Absent => "absent",
            Self::Revoked { .. } => "revoked",
            Self::Superseded { .. } => "superseded",
            Self::Expired { .. } => "expired",
            Self::Malformed { .. } => "malformed",
        }
    }

    /// One operator-facing line saying what to do about it.
    pub fn explain(&self) -> String {
        match self {
            Self::Live => "approved for the current tip".into(),
            Self::Absent => "no approval recorded".into(),
            Self::Revoked { .. } => "approval was withdrawn".into(),
            Self::Superseded { approved, head } => format!(
                "approval covers {approved}, but the lane is now at {head} — re-review the new commits"
            ),
            Self::Expired { .. } => "approval has lapsed; re-approve the current tip".into(),
            Self::Malformed { why } => format!("approval record is unusable: {why}"),
        }
    }
}

/// Decide whether `approval` still authorizes work on the tree at `head_sha`.
///
/// `ttl_secs` is the configured maximum age; `0` disables the age bound
/// entirely. When the record also carries its own `expires_at_ms`, the
/// **earlier** of the two deadlines wins — so shortening the configured TTL
/// takes effect on approvals already on record, and an approval deliberately
/// granted as short-lived is not extended by a generous config.
///
/// Precedence when several things are wrong at once is deliberate:
/// revoked (an explicit human act) beats superseded (the tree moved) beats
/// expired (time passed). Each is a refusal; the one reported is the one the
/// reviewer most needs to hear.
pub fn state_for(
    approval: Option<&Approval>,
    head_sha: &str,
    now_ms: i64,
    ttl_secs: u64,
) -> ApprovalState {
    let Some(a) = approval else {
        return ApprovalState::Absent;
    };

    if let Some(at_ms) = a.revoked_at_ms {
        return ApprovalState::Revoked { at_ms };
    }

    let approved = a.commit_sha.trim();
    let head = head_sha.trim();
    if approved.len() < MIN_SHA_PREFIX {
        return ApprovalState::Malformed {
            why: "the approved commit is empty or too short to identify a tree",
        };
    }
    if head.len() < MIN_SHA_PREFIX {
        return ApprovalState::Malformed {
            why: "the lane's head commit is empty or too short to identify a tree",
        };
    }
    if !same_commit(approved, head) {
        return ApprovalState::Superseded {
            approved: approved.to_string(),
            head: head.to_string(),
        };
    }

    if let Some(deadline) = deadline_ms(a, ttl_secs)
        && now_ms > deadline
    {
        return ApprovalState::Expired {
            deadline_ms: deadline,
        };
    }

    ApprovalState::Live
}

/// The effective deadline: the earlier of the record's own and the configured
/// TTL's, or `None` when neither bounds it.
fn deadline_ms(a: &Approval, ttl_secs: u64) -> Option<i64> {
    let from_ttl = (ttl_secs > 0).then(|| {
        // Clamp BEFORE the cast, then saturate. `ttl_secs as i64` alone wraps
        // negative for anything past `i64::MAX`, which would put the deadline
        // in the past and expire every approval instantly — the opposite of
        // what an enormous TTL asks for.
        let secs = i64::try_from(ttl_secs).unwrap_or(i64::MAX);
        a.granted_at_ms.saturating_add(secs.saturating_mul(1_000))
    });
    match (a.expires_at_ms, from_ttl) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (Some(x), None) => Some(x),
        (None, y) => y,
    }
}

/// Do two commit strings name the same commit, allowing either to be an
/// abbreviation of the other?
///
/// git hands out abbreviated shas everywhere (`rev-parse --short`, the reflog,
/// every log line), so an operator typing `supervise approve --commit 4babb09`
/// must match a 40-character HEAD. Comparison is case-insensitive because git
/// accepts either case for hex.
///
/// Both sides are already length-checked by [`state_for`]; this function is
/// deliberately not public, so a prefix match can never be reached with an
/// unchecked empty string.
fn same_commit(a: &str, b: &str) -> bool {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    long.len() >= short.len()
        && long
            .chars()
            .zip(short.chars())
            .all(|(l, s)| l.eq_ignore_ascii_case(&s))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "4babb0901f2c3d4e5f60718293a4b5c6d7e8f901";
    const NOW: i64 = 1_700_000_000_000;

    fn approval(commit: &str) -> Approval {
        Approval {
            issue_id: "linear:THE-407".into(),
            stage: "code".into(),
            commit_sha: commit.into(),
            approver: "blake".into(),
            note: None,
            granted_at_ms: NOW - 1_000,
            expires_at_ms: None,
            revoked_at_ms: None,
        }
    }

    #[test]
    fn an_approval_of_the_current_tip_is_live() {
        let a = approval(HEAD);
        assert_eq!(state_for(Some(&a), HEAD, NOW, 0), ApprovalState::Live);
        assert!(state_for(Some(&a), HEAD, NOW, 0).is_live());
    }

    #[test]
    fn no_record_is_absent_not_live() {
        let state = state_for(None, HEAD, NOW, 0);
        assert_eq!(state, ApprovalState::Absent);
        assert!(!state.is_live());
    }

    // --- the rule the whole module exists for --------------------------------

    #[test]
    fn a_new_commit_supersedes_the_approval() {
        // THE test of this programme: approve a diff, push one more commit, and
        // the supervisor must stop rather than land unreviewed code.
        let a = approval("1111111111111111111111111111111111111111");
        let state = state_for(Some(&a), HEAD, NOW, 0);
        assert!(
            !state.is_live(),
            "an approval must not survive a new commit"
        );
        match state {
            ApprovalState::Superseded { approved, head } => {
                assert_eq!(approved, "1111111111111111111111111111111111111111");
                assert_eq!(head, HEAD);
            }
            other => panic!("expected Superseded, got {other:?}"),
        }
        // And the refusal tells the reviewer what to do about it.
        assert!(
            state_for(Some(&a), HEAD, NOW, 0)
                .explain()
                .contains("re-review"),
            "the refusal must name the remedy"
        );
    }

    #[test]
    fn an_abbreviated_sha_matches_the_full_head_either_way_round() {
        // `rev-parse --short` output is what an operator has to hand.
        assert!(state_for(Some(&approval("4babb09")), HEAD, NOW, 0).is_live());
        assert!(state_for(Some(&approval("4babb0901f2c3d4")), HEAD, NOW, 0).is_live());
        // And a full approval against a short head (a caller that abbreviated).
        assert!(state_for(Some(&approval(HEAD)), "4babb0901", NOW, 0).is_live());
    }

    #[test]
    fn sha_comparison_ignores_hex_case() {
        assert!(state_for(Some(&approval("4BABB09")), HEAD, NOW, 0).is_live());
    }

    #[test]
    fn an_abbreviation_that_diverges_is_not_a_match() {
        // Shares a prefix but differs inside the abbreviation.
        assert!(!state_for(Some(&approval("4babb19")), HEAD, NOW, 0).is_live());
    }

    #[test]
    fn a_too_short_or_empty_commit_is_malformed_not_a_wildcard() {
        // The trap: a naive `head.starts_with(&approved)` matches EVERYTHING
        // when `approved` is empty, so an empty record would approve the world.
        for bad in ["", "   ", "4b", "4babb0"] {
            let state = state_for(Some(&approval(bad)), HEAD, NOW, 0);
            assert!(
                matches!(state, ApprovalState::Malformed { .. }),
                "{bad:?} must be refused, got {state:?}"
            );
            assert!(!state.is_live());
        }
    }

    #[test]
    fn an_empty_head_is_refused_too() {
        let state = state_for(Some(&approval(HEAD)), "", NOW, 0);
        assert!(matches!(state, ApprovalState::Malformed { .. }));
    }

    // --- revocation ----------------------------------------------------------

    #[test]
    fn revocation_beats_everything_else() {
        let mut a = approval(HEAD);
        a.revoked_at_ms = Some(NOW - 10);
        assert_eq!(
            state_for(Some(&a), HEAD, NOW, 0),
            ApprovalState::Revoked { at_ms: NOW - 10 }
        );
        // Even when the commit ALSO moved and the TTL ALSO lapsed.
        let mut stale = approval("2222222222222222222222222222222222222222");
        stale.revoked_at_ms = Some(NOW - 10);
        stale.granted_at_ms = NOW - 10_000_000;
        assert!(matches!(
            state_for(Some(&stale), HEAD, NOW, 1),
            ApprovalState::Revoked { .. }
        ));
    }

    // --- expiry --------------------------------------------------------------

    #[test]
    fn a_zero_ttl_never_expires() {
        let mut a = approval(HEAD);
        a.granted_at_ms = 0; // the epoch
        assert!(state_for(Some(&a), HEAD, NOW, 0).is_live());
    }

    #[test]
    fn a_configured_ttl_lapses_an_old_approval() {
        let mut a = approval(HEAD);
        a.granted_at_ms = NOW - 2_000; // two seconds ago
        assert!(state_for(Some(&a), HEAD, NOW, 60).is_live());
        assert!(matches!(
            state_for(Some(&a), HEAD, NOW, 1),
            ApprovalState::Expired { .. }
        ));
    }

    #[test]
    fn tightening_the_configured_ttl_applies_to_existing_records() {
        // The operator lowers the TTL after granting; records already on the
        // books must obey the new bound rather than riding out the old one.
        let mut a = approval(HEAD);
        a.granted_at_ms = NOW - 10_000;
        a.expires_at_ms = Some(NOW + 1_000_000); // generous record deadline
        assert!(
            matches!(
                state_for(Some(&a), HEAD, NOW, 1),
                ApprovalState::Expired { .. }
            ),
            "the earlier of the two deadlines must win"
        );
    }

    #[test]
    fn a_short_lived_record_is_not_extended_by_a_generous_ttl() {
        let mut a = approval(HEAD);
        a.granted_at_ms = NOW - 10;
        a.expires_at_ms = Some(NOW - 1); // already past
        assert!(matches!(
            state_for(Some(&a), HEAD, NOW, 86_400),
            ApprovalState::Expired { .. }
        ));
    }

    #[test]
    fn expiry_is_inclusive_of_the_deadline_instant() {
        let mut a = approval(HEAD);
        a.granted_at_ms = NOW - 1_000;
        // deadline == now exactly: still live (strictly `now > deadline` expires).
        assert!(state_for(Some(&a), HEAD, NOW, 1).is_live());
    }

    #[test]
    fn an_absurd_ttl_does_not_wrap_into_the_past() {
        // Saturating arithmetic: a huge TTL must not overflow to a deadline
        // before `now` and expire everything.
        let a = approval(HEAD);
        assert!(state_for(Some(&a), HEAD, NOW, u64::MAX).is_live());
    }

    #[test]
    fn superseded_beats_expired_because_it_is_more_informative() {
        let mut a = approval("3333333333333333333333333333333333333333");
        a.granted_at_ms = NOW - 10_000_000;
        assert!(matches!(
            state_for(Some(&a), HEAD, NOW, 1),
            ApprovalState::Superseded { .. }
        ));
    }

    // --- surfaces ------------------------------------------------------------

    #[test]
    fn every_state_has_a_distinct_token_and_a_nonempty_explanation() {
        let states = [
            ApprovalState::Live,
            ApprovalState::Absent,
            ApprovalState::Revoked { at_ms: 0 },
            ApprovalState::Superseded {
                approved: "a".into(),
                head: "b".into(),
            },
            ApprovalState::Expired { deadline_ms: 0 },
            ApprovalState::Malformed { why: "x" },
        ];
        let mut tokens: Vec<&str> = states.iter().map(ApprovalState::token).collect();
        tokens.sort_unstable();
        let before = tokens.len();
        tokens.dedup();
        assert_eq!(before, tokens.len(), "two states share a token");
        for s in &states {
            assert!(!s.explain().is_empty(), "{s:?} explains nothing");
        }
        // Exactly one state authorizes work.
        assert_eq!(states.iter().filter(|s| s.is_live()).count(), 1);
    }

    #[test]
    fn an_approval_round_trips_through_json() {
        let a = approval(HEAD);
        let json = serde_json::to_string(&a).expect("serialize");
        let back: Approval = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(a, back);
    }
}
