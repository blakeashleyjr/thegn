//! One approval shape for every effect, bound by a digest (b155, THE-2200).
//!
//! Every approval is a `(issue_id, stage, commit_sha)` triple in
//! `pipeline_approvals`. The triple's shape names the effect kind:
//!
//! - lane approvals: (tracker issue, stage, head sha), unchanged;
//! - proposals: (tracker issue, `start.<stage>`, digest of the proposal);
//! - rule arming: (`rule:<rule id>`, `arm`, digest of {body, requested});
//! - outward actions: (`outward:<target>`, `<cap>`, digest of the call).
//!
//! An effect fires only when the digest recomputed from the TRUSTED stored
//! payload equals the approved sha, so an approval the Lead requested with a
//! forged triple does nothing. Pure: no I/O.

use sha2::{Digest, Sha256};
use std::fmt::Write as _;

use crate::automation_autonomy::Autonomy;
use crate::db_approval_effects::ProposalRow;

/// Issue-id prefix of a rule-arming approval.
pub const RULE_PREFIX: &str = "rule:";
/// Stage of a rule-arming approval.
pub const ARM_STAGE: &str = "arm";
/// Issue-id prefix of an outward-action approval.
pub const OUTWARD_PREFIX: &str = "outward:";
/// Stage prefix of a proposal approval (`start.<stage>`).
pub const PROPOSAL_STAGE_PREFIX: &str = "start.";

/// Hex characters a digest keeps (160 bits).
const DIGEST_HEX: usize = 40;

/// The first 40 lowercase hex characters of sha256 over the value's compact
/// JSON with every object's keys sorted. Stable across key order.
pub fn digest(v: &serde_json::Value) -> String {
    let canonical = canonical(v).to_string();
    let hash = Sha256::digest(canonical.as_bytes());
    let mut out = String::with_capacity(DIGEST_HEX);
    for byte in hash.iter() {
        write!(out, "{byte:02x}").expect("writing to String cannot fail");
    }
    out.truncate(DIGEST_HEX);
    out
}

/// Rebuild `v` with every object's keys inserted in sorted order, so the
/// compact rendering is canonical whether or not `serde_json` preserves
/// insertion order.
fn canonical(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = serde_json::Map::new();
            for key in keys {
                out.insert(key.clone(), canonical(&map[key]));
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(canonical).collect())
        }
        other => other.clone(),
    }
}

/// Which effect an approval triple names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectKind {
    /// A pipeline lane's land approval.
    Lane,
    /// A `propose` start waiting for the owner.
    Proposal,
    /// Arming a Lead rule above its cap.
    RuleArm,
    /// One outward call by a Lead-confined caller.
    Outward,
}

/// Classify a triple by its shape. Anything that is not one of the three
/// structured kinds is a lane approval.
pub fn effect_kind(issue_id: &str, stage: &str) -> EffectKind {
    if issue_id.starts_with(RULE_PREFIX) && stage == ARM_STAGE {
        EffectKind::RuleArm
    } else if issue_id.starts_with(OUTWARD_PREFIX) {
        EffectKind::Outward
    } else if stage.starts_with(PROPOSAL_STAGE_PREFIX) {
        EffectKind::Proposal
    } else {
        EffectKind::Lane
    }
}

/// The approval triple arming `rule_id` at `requested` with body `body_json`.
/// Editing the body changes the digest, which disarms the rule.
pub fn rule_arm_ids(
    rule_id: &str,
    body_json: &str,
    requested: Autonomy,
) -> (String, String, String) {
    let sha = digest(&serde_json::json!({
        "body_json": body_json,
        "requested": requested.as_str(),
    }));
    (
        format!("{RULE_PREFIX}{rule_id}"),
        ARM_STAGE.to_string(),
        sha,
    )
}

/// The approval triple of a stored proposal: the tracker issue, the
/// `start.<stage>` stage and the digest of the trusted proposal fields.
pub fn proposal_ids(p: &ProposalRow) -> (String, String, String) {
    let sha = digest(&serde_json::json!({
        "rule": p.rule_id,
        "repo_root": p.repo_root,
        "issue": p.issue_id,
        "provider": p.provider,
        "account": p.account,
        "stage": p.stage,
        "base_sha": p.base_sha,
    }));
    (
        p.issue_id.clone(),
        format!("{PROPOSAL_STAGE_PREFIX}{}", p.stage),
        sha,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval_request::validate_ids;

    fn proposal() -> ProposalRow {
        ProposalRow {
            id: 0,
            approval_issue: String::new(),
            approval_stage: String::new(),
            approval_sha: String::new(),
            rule_id: "lead:triage".into(),
            repo_root: "/srv/repo".into(),
            issue_id: "linear:THE-2200".into(),
            provider: "linear".into(),
            account: "work".into(),
            stage: "coder".into(),
            base_sha: "0123456789abcdef0123456789abcdef01234567".into(),
            created_at_ms: 1,
            expires_at_ms: 2,
            state: "pending".into(),
        }
    }

    #[test]
    fn digest_is_stable_across_key_order_and_shape() {
        let a: serde_json::Value =
            serde_json::from_str(r#"{"b":1,"a":{"y":2,"x":[{"q":1,"p":2}]}}"#).unwrap();
        let b: serde_json::Value =
            serde_json::from_str(r#"{"a":{"x":[{"p":2,"q":1}],"y":2},"b":1}"#).unwrap();
        let d = digest(&a);
        assert_eq!(d, digest(&b));
        assert_eq!(d.len(), 40);
        assert!(
            d.bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        );
        assert_ne!(d, digest(&serde_json::json!({"b": 2})));
        // Known vector: sha256("{}") starts 44136fa3.
        assert!(digest(&serde_json::json!({})).starts_with("44136fa3"));
    }

    #[test]
    fn effect_kind_table() {
        let table = [
            ("linear:THE-1", "coder", EffectKind::Lane),
            ("linear:THE-1", "start.coder", EffectKind::Proposal),
            ("rule:lead:x", "arm", EffectKind::RuleArm),
            ("rule:lead:x", "coder", EffectKind::Lane),
            (
                "outward:linear:THE-1",
                "issues.comment",
                EffectKind::Outward,
            ),
            ("outward:linear:THE-1", "start.x", EffectKind::Outward),
        ];
        for (issue, stage, kind) in table {
            assert_eq!(effect_kind(issue, stage), kind, "{issue} {stage}");
        }
    }

    #[test]
    fn rule_arm_ids_bind_body_and_level_and_validate() {
        let (issue, stage, sha) = rule_arm_ids("lead:triage", "{\"v\":1}", Autonomy::Merge);
        assert_eq!(
            (issue.as_str(), stage.as_str()),
            ("rule:lead:triage", "arm")
        );
        validate_ids(&issue, &stage, &sha).unwrap();
        assert_eq!(effect_kind(&issue, &stage), EffectKind::RuleArm);
        assert_ne!(
            sha,
            rule_arm_ids("lead:triage", "{\"v\":2}", Autonomy::Merge).2
        );
        assert_ne!(
            sha,
            rule_arm_ids("lead:triage", "{\"v\":1}", Autonomy::Review).2
        );
        // The longest Lead rule id (`lead:` + 48) still fits the issue bound.
        let longest = format!("lead:{}", "a".repeat(48));
        let (issue, stage, sha) = rule_arm_ids(&longest, "{}", Autonomy::Review);
        validate_ids(&issue, &stage, &sha).unwrap();
    }

    #[test]
    fn proposal_ids_bind_every_field_and_validate() {
        let p = proposal();
        let (issue, stage, sha) = proposal_ids(&p);
        assert_eq!(
            (issue.as_str(), stage.as_str()),
            ("linear:THE-2200", "start.coder")
        );
        validate_ids(&issue, &stage, &sha).unwrap();
        assert_eq!(effect_kind(&issue, &stage), EffectKind::Proposal);
        let mut other = p.clone();
        other.base_sha = "f".repeat(40);
        assert_ne!(sha, proposal_ids(&other).2);
        let mut other = p.clone();
        other.account = "home".into();
        assert_ne!(sha, proposal_ids(&other).2);
        // Bookkeeping fields are not bound.
        let mut other = p;
        other.state = "started".into();
        other.id = 9;
        assert_eq!(sha, proposal_ids(&other).2);
    }
}
