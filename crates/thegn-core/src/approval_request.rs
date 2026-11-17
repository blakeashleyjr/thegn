//! Approval requests (`approvals.request` / `approvals.list` /
//! `approvals.resolve`, THE-2172, THE-2182).
//!
//! A request only asks: it is an `agent_attention` notification row naming the
//! lane, stage and commit. Granting stays an Admin act outside every agent door
//! (see [`crate::pipeline_approval`]: no supervisor path may grant).
//!
//! The notification row's source ref is `approval:{issue}:{stage}@{sha}`
//! ([`source_ref`]). Issue ids are `"<provider>:<key>"` and may themselves hold
//! `:`, so [`parse_source_ref`] reads from the RIGHT: the sha after the last
//! `@`, then the stage after the last `:`. [`validate_ids`] keeps `:` and `@`
//! out of the stage and anything but hex out of the sha, which is what makes
//! that parse unambiguous for every ref this module writes.
//!
//! Pure: no I/O. The daemon side is `daemon/lead_caps/approvals.rs`.

use crate::pipeline_approval::{Approval, MIN_SHA_PREFIX, state_for};
use serde::{Deserialize, Serialize};

/// Every approval-request source ref starts with this.
pub const SOURCE_PREFIX: &str = "approval:";

/// The longest issue id a request may name (bytes).
pub const MAX_ISSUE_ID: usize = 128;
/// The longest stage name a request may name (bytes).
pub const MAX_STAGE: usize = 64;
/// The longest commit sha a request may name (a full SHA-256 object id).
pub const MAX_SHA: usize = 64;
/// The longest worktree path a request may carry (bytes).
pub const MAX_WORKTREE: usize = 4096;
/// The longest free-text reason a request may carry (characters).
pub const MAX_REASON: usize = 500;
/// The longest approver label a resolve may record (bytes).
pub const MAX_APPROVER: usize = 128;

/// One outstanding approval request: a human has been asked to approve
/// `stage` of `issue_id` at `commit_sha` and has not answered yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingApproval {
    /// Tracker issue id (`"<provider>:<key>"`), the lane's identity.
    pub issue_id: String,
    /// The stage whose output awaits approval.
    pub stage: String,
    /// The commit the approval would cover.
    pub commit_sha: String,
    /// The worktree the stage runs in (empty when unknown).
    pub worktree: String,
    /// When the request was made (epoch ms).
    pub requested_at: i64,
    /// The `agent_attention` notification row that carries the request.
    pub notification_id: i64,
    /// The human-readable request text.
    pub message: String,
}

/// One recorded approval as `approvals.list` reports it: the record plus its
/// evaluated state token ([`crate::pipeline_approval::ApprovalState::token`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalView {
    /// The recorded approval.
    #[serde(flatten)]
    pub approval: crate::pipeline_approval::Approval,
    /// `live`, `revoked`, `superseded`, `expired`, … as evaluated now.
    pub state: String,
}

impl ApprovalView {
    /// Evaluate a record against its own commit: the list has no lane tip to
    /// compare with, so the state reports revocation, expiry and malformation
    /// (never `superseded`).
    pub fn evaluate(approval: Approval, now_ms: i64, ttl_secs: u64) -> Self {
        let state = state_for(Some(&approval), &approval.commit_sha, now_ms, ttl_secs)
            .token()
            .to_string();
        Self { approval, state }
    }
}

/// `approvals.request`: ask a human to approve `stage` of `issue_id` at
/// `commit_sha`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRequest {
    pub issue_id: String,
    pub stage: String,
    pub commit_sha: String,
    #[serde(default)]
    pub worktree: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// What a human decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Approve,
    Reject,
}

/// `approvals.resolve` (Admin; HTTP + CLI only): answer a request.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalResolve {
    pub issue_id: String,
    pub stage: String,
    pub commit_sha: String,
    pub decision: Decision,
    pub approver: String,
}

/// The notification source ref for a request: `approval:{issue}:{stage}@{sha}`.
pub fn source_ref(issue_id: &str, stage: &str, commit_sha: &str) -> String {
    format!("{SOURCE_PREFIX}{issue_id}:{stage}@{commit_sha}")
}

/// Invert [`source_ref`], reading from the right so an issue id holding `:`
/// (or `@`) parses intact. `None` for anything that is not a well-formed ref:
/// a missing prefix, an empty part, or parts [`validate_ids`] would refuse.
pub fn parse_source_ref(s: &str) -> Option<(String, String, String)> {
    let rest = s.strip_prefix(SOURCE_PREFIX)?;
    let (left, sha) = rest.rsplit_once('@')?;
    let (issue, stage) = left.rsplit_once(':')?;
    validate_ids(issue, stage, sha).ok()?;
    Some((issue.to_string(), stage.to_string(), sha.to_string()))
}

/// The inbox line for a request: `Approval requested: {issue} {stage} @
/// {sha7} — {reason}` (the reason clause only when one was given).
pub fn request_message(r: &ApprovalRequest) -> String {
    let sha7: String = r.commit_sha.chars().take(MIN_SHA_PREFIX).collect();
    let base = format!("Approval requested: {} {} @ {sha7}", r.issue_id, r.stage);
    match r.reason.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(reason) => format!("{base} — {reason}"),
        None => base,
    }
}

fn has_control(s: &str) -> bool {
    s.chars().any(char::is_control)
}

/// Validate the three identity parts every request, resolve and source ref
/// carries. The rules make [`parse_source_ref`] unambiguous: the stage holds
/// neither `:` nor `@` (charset `[A-Za-z0-9._-]`) and the sha is hex only.
pub fn validate_ids(issue_id: &str, stage: &str, commit_sha: &str) -> Result<(), String> {
    if issue_id.trim().is_empty() {
        return Err("issue_id is empty".into());
    }
    if issue_id.len() > MAX_ISSUE_ID {
        return Err(format!("issue_id is longer than {MAX_ISSUE_ID} bytes"));
    }
    if has_control(issue_id) {
        return Err("issue_id contains control characters".into());
    }
    if stage.is_empty() {
        return Err("stage is empty".into());
    }
    if stage.len() > MAX_STAGE {
        return Err(format!("stage is longer than {MAX_STAGE} bytes"));
    }
    if !stage
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err("stage may only contain [A-Za-z0-9._-]".into());
    }
    if commit_sha.len() < MIN_SHA_PREFIX || commit_sha.len() > MAX_SHA {
        return Err(format!(
            "commit_sha must be {MIN_SHA_PREFIX}..={MAX_SHA} hex characters"
        ));
    }
    if !commit_sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("commit_sha must be hexadecimal".into());
    }
    Ok(())
}

/// Validate an `approvals.request` body: bounded fields, no control
/// characters, a reason of at most [`MAX_REASON`] characters.
pub fn validate_request(r: &ApprovalRequest) -> Result<(), String> {
    validate_ids(&r.issue_id, &r.stage, &r.commit_sha)?;
    if let Some(worktree) = &r.worktree {
        if worktree.len() > MAX_WORKTREE {
            return Err(format!("worktree is longer than {MAX_WORKTREE} bytes"));
        }
        if has_control(worktree) {
            return Err("worktree contains control characters".into());
        }
    }
    if let Some(reason) = &r.reason {
        if reason.chars().count() > MAX_REASON {
            return Err(format!("reason is longer than {MAX_REASON} characters"));
        }
        if has_control(reason) {
            return Err("reason contains control characters".into());
        }
    }
    Ok(())
}

/// Validate an `approvals.resolve` body.
pub fn validate_resolve(r: &ApprovalResolve) -> Result<(), String> {
    validate_ids(&r.issue_id, &r.stage, &r.commit_sha)?;
    if r.approver.trim().is_empty() {
        return Err("approver is empty".into());
    }
    if r.approver.len() > MAX_APPROVER {
        return Err(format!("approver is longer than {MAX_APPROVER} bytes"));
    }
    if has_control(&r.approver) {
        return Err("approver contains control characters".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "4babb0901f2c3d4e5f60718293a4b5c6d7e8f901";

    fn request() -> ApprovalRequest {
        ApprovalRequest {
            issue_id: "linear:THE-1".into(),
            stage: "code".into(),
            commit_sha: SHA.into(),
            worktree: None,
            reason: None,
        }
    }

    #[test]
    fn source_ref_round_trips() {
        let s = source_ref("linear:THE-1", "code", SHA);
        assert_eq!(s, format!("approval:linear:THE-1:code@{SHA}"));
        assert_eq!(
            parse_source_ref(&s),
            Some(("linear:THE-1".into(), "code".into(), SHA.into()))
        );
    }

    #[test]
    fn source_ref_round_trips_issue_ids_holding_colons_and_ats() {
        for issue in ["a:b:c:THE-9", "gh:org/repo:12", "jira:x@y:Z-1", ":lead"] {
            let s = source_ref(issue, "review.v2", "abcdef1");
            assert_eq!(
                parse_source_ref(&s),
                Some((issue.to_string(), "review.v2".into(), "abcdef1".into())),
                "{issue}"
            );
        }
    }

    #[test]
    fn parse_source_ref_refuses_malformed_refs() {
        for bad in [
            "",
            "approval:",
            "linear:THE-1:code@abcdef1",
            "approval:linear:THE-1:code",
            "approval:THE1@abcdef1",
            "approval:linear:THE-1:code@abc",
            "approval:linear:THE-1:code@zzzzzzz",
            "approval::code@abcdef1",
            "approval:linear:THE-1:@abcdef1",
        ] {
            assert_eq!(parse_source_ref(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_crafted_issue_id_cannot_shift_the_parsed_stage_or_sha() {
        // An issue id that embeds a fake `:stage@sha` tail still parses back to
        // itself: the real stage and sha are always the rightmost parts.
        let issue = "linear:THE-1:merge@ffffffff";
        let s = source_ref(issue, "code", SHA);
        assert_eq!(
            parse_source_ref(&s),
            Some((issue.into(), "code".into(), SHA.into()))
        );
    }

    #[test]
    fn validate_request_accepts_a_plain_request() {
        let mut r = request();
        r.worktree = Some("/w/t".into());
        r.reason = Some("x".repeat(MAX_REASON));
        assert_eq!(validate_request(&r), Ok(()));
    }

    #[test]
    fn validate_request_bounds_every_field() {
        type Mutate = Box<dyn Fn(&mut ApprovalRequest)>;
        let cases: Vec<Mutate> = vec![
            Box::new(|r| r.issue_id = String::new()),
            Box::new(|r| r.issue_id = "x".repeat(MAX_ISSUE_ID + 1)),
            Box::new(|r| r.issue_id = "linear:THE\n1".into()),
            Box::new(|r| r.stage = String::new()),
            Box::new(|r| r.stage = "co:de".into()),
            Box::new(|r| r.stage = "co@de".into()),
            Box::new(|r| r.stage = "x".repeat(MAX_STAGE + 1)),
            Box::new(|r| r.commit_sha = "abc12".into()),
            Box::new(|r| r.commit_sha = "g".repeat(10)),
            Box::new(|r| r.commit_sha = "a".repeat(MAX_SHA + 1)),
            Box::new(|r| r.worktree = Some("x".repeat(MAX_WORKTREE + 1))),
            Box::new(|r| r.worktree = Some("/w\u{7}".into())),
            Box::new(|r| r.reason = Some("x".repeat(MAX_REASON + 1))),
            Box::new(|r| r.reason = Some("line\nbreak".into())),
        ];
        for (index, mutate) in cases.iter().enumerate() {
            let mut r = request();
            mutate(&mut r);
            assert!(validate_request(&r).is_err(), "case {index} accepted");
        }
    }

    #[test]
    fn validate_resolve_requires_an_approver() {
        let mut r = ApprovalResolve {
            issue_id: "linear:THE-1".into(),
            stage: "code".into(),
            commit_sha: SHA.into(),
            decision: Decision::Approve,
            approver: "blake".into(),
        };
        assert_eq!(validate_resolve(&r), Ok(()));
        r.approver = " ".into();
        assert!(validate_resolve(&r).is_err());
        r.approver = "x".repeat(MAX_APPROVER + 1);
        assert!(validate_resolve(&r).is_err());
    }

    #[test]
    fn decision_decodes_lowercase() {
        let r: ApprovalResolve = serde_json::from_value(serde_json::json!({
            "issue_id": "linear:THE-1", "stage": "code", "commit_sha": SHA,
            "decision": "reject", "approver": "blake",
        }))
        .unwrap();
        assert_eq!(r.decision, Decision::Reject);
    }

    #[test]
    fn request_message_shortens_the_sha_and_appends_a_reason() {
        let mut r = request();
        assert_eq!(
            request_message(&r),
            "Approval requested: linear:THE-1 code @ 4babb09"
        );
        r.reason = Some("needs a human eye".into());
        assert_eq!(
            request_message(&r),
            "Approval requested: linear:THE-1 code @ 4babb09 — needs a human eye"
        );
    }

    #[test]
    fn approval_view_reports_the_record_state() {
        let mut a = Approval {
            issue_id: "linear:THE-1".into(),
            stage: "code".into(),
            commit_sha: SHA.into(),
            approver: "blake".into(),
            note: None,
            granted_at_ms: 1_000,
            expires_at_ms: None,
            revoked_at_ms: None,
        };
        assert_eq!(ApprovalView::evaluate(a.clone(), 2_000, 0).state, "live");
        assert_eq!(
            ApprovalView::evaluate(a.clone(), 10_000, 1).state,
            "expired"
        );
        a.revoked_at_ms = Some(1_500);
        assert_eq!(ApprovalView::evaluate(a, 2_000, 0).state, "revoked");
    }
}
