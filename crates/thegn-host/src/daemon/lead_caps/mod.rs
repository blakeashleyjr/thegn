//! The daemon side of the Lead tool surface (THE-2172).
//!
//! `DaemonService`'s eleven Lead `ControlApi` methods are one-line delegations
//! into these modules, which take and return `serde_json::Value`; each module
//! decodes into its owning thegn-core DTO with [`decode`] and replies with
//! [`encode`]. The files were created as stubs by the b152 spine (lane 1) and
//! are filled by their owning lanes.

pub(crate) mod approvals;
pub(crate) mod automations;
pub(crate) mod feed;
pub(crate) mod listener;
pub(crate) mod merge_drain;
pub(crate) mod notifications;
pub(crate) mod overview;

use thegn_svc::control::{ControlError, ControlResult};

/// The JSON reply every Lead verb returns.
pub(crate) type JsonResult = ControlResult<serde_json::Value>;

/// Decode a JSON request body into a lane's typed DTO, mapping serde errors to
/// InvalidArgument.
#[allow(dead_code, reason = "filled by b152 lanes 3-6")]
pub(crate) fn decode<T: serde::de::DeserializeOwned>(req: serde_json::Value) -> ControlResult<T> {
    serde_json::from_value(req).map_err(|error| ControlError::InvalidArgument(error.to_string()))
}

/// Serialize a lane's typed reply.
#[allow(dead_code, reason = "filled by b152 lanes 3-6")]
pub(crate) fn encode<T: serde::Serialize>(reply: &T) -> JsonResult {
    serde_json::to_value(reply).map_err(|error| ControlError::Internal(error.into()))
}

#[cfg(test)]
mod tests {
    use super::{decode, encode};
    use serde_json::json;
    use thegn_svc::control::ControlError;

    #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    struct Probe {
        name: String,
    }

    #[test]
    fn decode_maps_serde_errors_to_invalid_argument() {
        let ok: Probe = decode(json!({ "name": "x" })).unwrap();
        assert_eq!(ok, Probe { name: "x".into() });
        assert!(matches!(
            decode::<Probe>(json!({ "name": 3 })),
            Err(ControlError::InvalidArgument(_))
        ));
        assert_eq!(encode(&ok).unwrap(), json!({ "name": "x" }));
    }
}
