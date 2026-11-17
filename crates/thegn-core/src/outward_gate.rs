//! Outward actions by a Lead-confined caller (6d): `issues.comment` and
//! `issues.update` need an owner approval bound to the call's digest, then
//! the identical call is retried. `[lead] outward_allow` opts a cap out.
//!
//! [`OUTWARD_CAPS`] is final (the spine, b155 lane 1); the functions are
//! stubs b155 lane 4 fills, and they fail closed.

/// The caps that reach outside thegn on a Lead's behalf.
pub const OUTWARD_CAPS: &[&str] = &["issues.comment", "issues.update"];

/// The approval triple for one outward call: (`outward:<target>`, `cap`,
/// digest of {cap, target, canonical body}).
#[allow(dead_code, reason = "filled by b155 lane 4")]
pub fn approval_ids(
    cap: &str,
    _target: &str,
    _body: &serde_json::Value,
) -> (String, String, String) {
    ("outward:x".into(), cap.to_string(), "0".repeat(40))
}

/// Whether `[lead] outward_allow` lets `cap` through without an approval.
#[allow(dead_code, reason = "filled by b155 lane 4")]
pub fn allowed_without_approval(_cap: &str, _outward_allow: &[String]) -> bool {
    false
}

/// The trusted description shown to the owner for an outward call.
#[allow(dead_code, reason = "filled by b155 lane 4")]
pub fn describe(cap: &str, _target: &str, _body: &serde_json::Value) -> String {
    cap.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outward_caps_are_catalog_caps_and_stubs_fail_closed() {
        for cap in OUTWARD_CAPS {
            assert!(crate::capability::lookup(cap).is_some(), "{cap}");
        }
        let body = serde_json::json!({});
        let (issue, stage, sha) = approval_ids("issues.comment", "linear:THE-1", &body);
        crate::approval_request::validate_ids(&issue, &stage, &sha).unwrap();
        assert!(!allowed_without_approval(
            "issues.comment",
            &["issues.comment".into()]
        ));
        assert_eq!(describe("issues.update", "t", &body), "issues.update");
    }
}
