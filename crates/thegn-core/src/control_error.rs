//! Stable, transport-independent error codes for the control plane.
//!
//! The HTTP adapter exposes these identifiers beside its existing human-readable
//! message. Keeping the vocabulary in core lets each transport project the same
//! error taxonomy without depending on a particular protocol.

use serde::{Deserialize, Serialize};

/// The closed set of machine-readable control-plane error identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ControlErrorCode {
    NotFound,
    NoScope,
    Conflict,
    Unimplemented,
    Internal,
    Unauthorized,
    BadRequest,
}

impl ControlErrorCode {
    /// Every code in its stable, serialized order.
    pub const ALL: &'static [Self] = &[
        Self::NotFound,
        Self::NoScope,
        Self::Conflict,
        Self::Unimplemented,
        Self::Internal,
        Self::Unauthorized,
        Self::BadRequest,
    ];

    /// The stable wire identifier.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::NoScope => "no_scope",
            Self::Conflict => "conflict",
            Self::Unimplemented => "unimplemented",
            Self::Internal => "internal",
            Self::Unauthorized => "unauthorized",
            Self::BadRequest => "bad_request",
        }
    }
}

impl std::fmt::Display for ControlErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_error_codes_have_stable_ids() {
        let expected = [
            (ControlErrorCode::NotFound, "not_found"),
            (ControlErrorCode::NoScope, "no_scope"),
            (ControlErrorCode::Conflict, "conflict"),
            (ControlErrorCode::Unimplemented, "unimplemented"),
            (ControlErrorCode::Internal, "internal"),
            (ControlErrorCode::Unauthorized, "unauthorized"),
            (ControlErrorCode::BadRequest, "bad_request"),
        ];
        assert_eq!(ControlErrorCode::ALL, &expected.map(|(code, _)| code));
        for (code, id) in expected {
            assert_eq!(code.as_str(), id);
            assert_eq!(code.to_string(), id);
            assert_eq!(serde_json::to_string(&code).unwrap(), format!("\"{id}\""));
            assert_eq!(
                serde_json::from_str::<ControlErrorCode>(&format!("\"{id}\"")).unwrap(),
                code
            );
        }
    }

    #[test]
    fn unknown_control_error_codes_are_rejected() {
        assert!(serde_json::from_str::<ControlErrorCode>(r#""future_code""#).is_err());
    }
}
