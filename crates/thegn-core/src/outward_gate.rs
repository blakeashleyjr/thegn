//! Outward actions by a Lead-confined caller (6d, THE-2205): `issues.comment`
//! and `issues.update` need an owner approval bound to the call's digest,
//! then the identical call is retried. `[lead] outward_allow` opts a cap out.
//!
//! The approval triple is `("outward:" + target, cap, digest({cap, target,
//! body}))`. The digest binds the FULL target and the canonical body, so a
//! different comment, patch, repo or account on the same issue is a
//! different approval; only the issue-id half is bounded to fit
//! [`MAX_ISSUE_ID`]. The `outward:` prefix keeps these triples apart from
//! rule arming (`rule:`) and lane/proposal approvals, and the digest's key set
//! (`cap`, `target`, `body`) differs from every other kind's. Pure: no I/O.

use crate::approval_digest::{OUTWARD_PREFIX, digest};
use crate::approval_request::MAX_ISSUE_ID;

/// The caps that reach outside thegn on a Lead's behalf.
pub const OUTWARD_CAPS: &[&str] = &["issues.comment", "issues.update"];

/// How much of the comment or update an owner sees in [`describe`].
pub const EXCERPT_CHARS: usize = 200;

/// How much of the target an owner sees in [`describe`].
const TARGET_CHARS: usize = 120;

/// `s` with every control character replaced by a space.
fn printable(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// The longest prefix of `s` at most `max` bytes long, on a char boundary.
fn prefix_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// At most `max` characters of `s`, with `…` when it was cut.
fn excerpt(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().nth(max).is_some() {
        out.push('…');
    }
    out
}

/// The approval triple for one outward call: (`outward:<target>`, `cap`,
/// digest of {cap, target, canonical body}). The issue id holds the target
/// without control characters, cut to fit [`MAX_ISSUE_ID`]; the digest uses
/// the target as given.
pub fn approval_ids(cap: &str, target: &str, body: &serde_json::Value) -> (String, String, String) {
    let shown = printable(target);
    let room = MAX_ISSUE_ID - OUTWARD_PREFIX.len();
    let issue = format!("{OUTWARD_PREFIX}{}", prefix_bytes(&shown, room));
    let sha = digest(&serde_json::json!({
        "cap": cap,
        "target": target,
        "body": body,
    }));
    (issue, cap.to_string(), sha)
}

/// Whether `[lead] outward_allow` lets `cap` through without an approval.
pub fn allowed_without_approval(cap: &str, outward_allow: &[String]) -> bool {
    outward_allow.iter().any(|allowed| allowed == cap)
}

/// The trusted description shown to the owner for an outward call:
/// `<cap> on <target>: <first 200 characters of the comment or update>`,
/// control characters replaced. Built by the daemon from the call itself,
/// never from a caller-supplied reason.
pub fn describe(cap: &str, target: &str, body: &serde_json::Value) -> String {
    let text = match (body.get("body"), body.get("patch")) {
        (Some(serde_json::Value::String(comment)), _) => comment.clone(),
        (_, Some(patch)) => patch.to_string(),
        _ => body.to_string(),
    };
    format!(
        "{cap} on {}: {}",
        excerpt(&printable(target), TARGET_CHARS),
        excerpt(&printable(&text), EXCERPT_CHARS)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval_digest::{EffectKind, effect_kind};
    use crate::approval_request::{MAX_REASON, validate_ids};
    use serde_json::json;

    #[test]
    fn outward_caps_are_catalog_caps() {
        for cap in OUTWARD_CAPS {
            assert!(crate::capability::lookup(cap).is_some(), "{cap}");
        }
    }

    #[test]
    fn ids_validate_classify_and_bind_the_call() {
        let body = json!({"body": "hi", "repo": null, "account": null});
        let (issue, stage, sha) = approval_ids("issues.comment", "linear:THE-1", &body);
        assert_eq!(issue, "outward:linear:THE-1");
        assert_eq!(stage, "issues.comment");
        validate_ids(&issue, &stage, &sha).unwrap();
        assert_eq!(effect_kind(&issue, &stage), EffectKind::Outward);
        // Same call, same digest (key order does not matter).
        let reordered: serde_json::Value =
            serde_json::from_str(r#"{"account":null,"repo":null,"body":"hi"}"#).unwrap();
        assert_eq!(
            sha,
            approval_ids("issues.comment", "linear:THE-1", &reordered).2
        );
        // A different body, target or cap is a different approval.
        for (cap, target, other) in [
            ("issues.comment", "linear:THE-1", json!({"body": "bye"})),
            ("issues.comment", "linear:THE-2", body.clone()),
            ("issues.update", "linear:THE-1", body.clone()),
        ] {
            assert_ne!(sha, approval_ids(cap, target, &other).2, "{cap} {target}");
        }
        // Never the arming digest of anything: different key set and prefix.
        assert_ne!(effect_kind(&issue, "arm"), EffectKind::RuleArm);
    }

    #[test]
    fn a_long_or_hostile_target_still_validates() {
        let body = json!({"body": "x"});
        let long = "é".repeat(400);
        let (issue, stage, sha) = approval_ids("issues.update", &long, &body);
        validate_ids(&issue, &stage, &sha).unwrap();
        assert!(issue.len() <= MAX_ISSUE_ID);
        // Two long targets sharing the bounded prefix still differ by digest.
        let longer = format!("{long}x");
        let (other_issue, _, other_sha) = approval_ids("issues.update", &longer, &body);
        assert_eq!(issue, other_issue);
        assert_ne!(sha, other_sha);
        // Control characters never reach the issue id.
        let (issue, stage, sha) = approval_ids("issues.comment", "a\nb\u{1b}c", &body);
        assert_eq!(issue, "outward:a b c");
        validate_ids(&issue, &stage, &sha).unwrap();
        let (issue, stage, sha) = approval_ids("issues.comment", "", &body);
        validate_ids(&issue, &stage, &sha).unwrap();
    }

    #[test]
    fn outward_allow_is_an_exact_list() {
        let allow = vec!["issues.comment".to_string()];
        assert!(allowed_without_approval("issues.comment", &allow));
        assert!(!allowed_without_approval("issues.update", &allow));
        assert!(!allowed_without_approval("issues.comment", &[]));
        assert!(!allowed_without_approval(
            "issues.comment",
            &[" issues.comment".to_string()]
        ));
    }

    #[test]
    fn describe_strips_control_characters_and_bounds_the_excerpt() {
        let body = json!({"body": "line one\nline\u{1b}[31m two\r\u{7}"});
        let text = describe("issues.comment", "linear:THE-1\n", &body);
        assert_eq!(
            text,
            "issues.comment on linear:THE-1 : line one line [31m two  "
        );
        assert!(!text.chars().any(char::is_control));

        let long = json!({"body": "a".repeat(500)});
        let text = describe("issues.comment", &"t".repeat(500), &long);
        assert!(text.ends_with(&format!("{}…", "a".repeat(EXCERPT_CHARS))));
        assert!(text.chars().count() <= MAX_REASON, "fits a request reason");

        // A patch is shown as its JSON (escapes stay escaped).
        let update = json!({"patch": {"title": "New\ttitle"}, "repo": null});
        assert_eq!(
            describe("issues.update", "gh:1", &update),
            r#"issues.update on gh:1: {"title":"New\ttitle"}"#
        );
        assert_eq!(
            describe("issues.update", "gh:1", &json!({"other": 1})),
            r#"issues.update on gh:1: {"other":1}"#
        );
    }
}
