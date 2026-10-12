//! Write fields and optional operations exposed by an issue provider.
//!
//! Built-in providers declare write-field bits before the router may ask them
//! to apply those fields. Optional operation bits likewise describe methods
//! that exist on [`super::IssueBackend`]. Plugin manifests default omitted
//! patch-field bits to allowed for backward compatibility; optional operation
//! bits remain opt-in.

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
    /// Older manifests omit the field (or serialize it as `null`). Their
    /// required `update_issue` operation remains usable, so omitted patch
    /// fields default to allowed while optional operations remain disabled.
    /// Unknown fields are rejected so a typo cannot silently grant a
    /// capability.
    pub fn from_json(value: &serde_json::Value) -> Result<Self, serde_json::Error> {
        if value.is_null() {
            Ok(Self::plugin_defaults())
        } else {
            let mut caps: Self = serde_json::from_value(value.clone())?;
            if let Some(fields) = value.as_object() {
                if !fields.contains_key("patch_status") {
                    caps.patch_status = true;
                }
                if !fields.contains_key("patch_title") {
                    caps.patch_title = true;
                }
                if !fields.contains_key("patch_assignee_me") {
                    caps.patch_assignee_me = true;
                }
                if !fields.contains_key("patch_priority") {
                    caps.patch_priority = true;
                }
            }
            Ok(caps)
        }
    }

    fn plugin_defaults() -> Self {
        Self {
            patch_status: true,
            patch_title: true,
            patch_assignee_me: true,
            patch_priority: true,
            ..Self::default()
        }
    }

    /// Decode a contribution defensively for the host registry. A malformed
    /// declaration grants no capability; the plugin host uses this only for
    /// optional gates after manifest decoding has failed.
    pub fn from_json_or_default(value: &serde_json::Value) -> Self {
        Self::from_json(value).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_omitted_and_null_patch_caps_default_to_allowed() {
        assert_eq!(
            IssueCaps::from_json(&serde_json::Value::Null).unwrap(),
            IssueCaps {
                patch_status: true,
                patch_title: true,
                patch_assignee_me: true,
                patch_priority: true,
                ..Default::default()
            }
        );
        assert_eq!(
            IssueCaps::from_json(&serde_json::json!({})).unwrap(),
            IssueCaps {
                patch_status: true,
                patch_title: true,
                patch_assignee_me: true,
                patch_priority: true,
                ..Default::default()
            }
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
    fn plugin_explicit_false_patch_caps_remain_disabled() {
        let caps = IssueCaps::from_json(&serde_json::json!({"patch_title": true})).unwrap();
        assert!(caps.patch_title);
        assert!(caps.patch_status);
        assert!(caps.patch_assignee_me);
        assert!(caps.patch_priority);
        let caps = IssueCaps::from_json(&serde_json::json!({"patch_status": false})).unwrap();
        assert!(!caps.patch_status);
        assert!(caps.patch_title);
    }
}
