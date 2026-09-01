//! Optional operations exposed by an issue provider.
//!
//! The capability bits are deliberately limited to operations that exist on
//! [`super::IssueBackend`].  A provider must declare a bit before the router
//! may ask it to perform that operation; omitted plugin capabilities therefore
//! degrade locally and never cause an unnecessary RPC round trip.

use serde::{Deserialize, Serialize};

/// Optional operations an issue provider may implement.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(default, deny_unknown_fields)]
pub struct IssueCaps {
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
}
