//! Write fields and optional operations exposed by an issue provider.
//!
//! A provider must declare a write-field bit before the router may ask it to
//! apply that field. Optional operation bits likewise describe methods that
//! exist on [`super::IssueBackend`]. Omitted plugin capabilities therefore
//! degrade locally and never cause an unnecessary RPC round trip.

use serde::{Deserialize, Serialize};

/// Optional operations an issue provider may implement.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(default, deny_unknown_fields)]
pub struct IssueCaps {
    /// The provider can change an issue's workflow status.
    pub patch_status: bool,
    /// The provider can change an issue's title.
    pub patch_title: bool,
    /// The provider can assign or unassign the current user.
    pub patch_assignee_me: bool,
    /// The provider can change an issue's priority.
    pub patch_priority: bool,
    /// The provider can create comments on issues.
    pub comments: bool,
    /// The provider can attach and detach labels on issues.
    pub labels: bool,
}

impl IssueCaps {
    /// Decode a plugin contribution's existing JSON `caps` field.
    ///
    /// Older manifests omit the field (or serialize it as `null`), which is
    /// intentionally the least-privilege all-false value. Unknown fields are
    /// rejected so a typo cannot silently grant a capability.
    pub fn from_json(value: &serde_json::Value) -> Result<Self, serde_json::Error> {
        if value.is_null() {
            Ok(Self::default())
        } else {
            serde_json::from_value(value.clone())
        }
    }

    /// Decode a contribution defensively for the host registry. A malformed
    /// declaration is treated like an old manifest: no optional operation is
    /// granted, while the plugin's required issue operations remain usable.
    pub fn from_json_or_default(value: &serde_json::Value) -> Self {
        Self::from_json(value).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omitted_and_null_caps_are_least_privilege() {
        assert_eq!(
            IssueCaps::from_json(&serde_json::Value::Null).unwrap(),
            IssueCaps::default()
        );
        assert_eq!(
            serde_json::from_str::<IssueCaps>("{}").unwrap(),
            IssueCaps::default()
        );
    }

    #[test]
    fn unknown_caps_are_rejected() {
        let err = IssueCaps::from_json(&serde_json::json!({ "comment": true })).unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
    }

    #[test]
    fn patch_caps_are_least_privilege_when_omitted() {
        let caps = IssueCaps::from_json(&serde_json::json!({"patch_title": true})).unwrap();
        assert!(caps.patch_title);
        assert!(!caps.patch_status);
        assert!(!caps.patch_assignee_me);
        assert!(!caps.patch_priority);
    }
}
