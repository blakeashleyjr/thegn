//! Trust-on-first-use for a repo `.superzej.*` overlay's *gated* sandbox
//! requests — the pure glue between the resolution clamp
//! ([`crate::config_resolve`]) and the persisted decisions
//! ([`crate::store::RepoTrustStore`]).
//!
//! The security match key is a request's **canonical JSON**
//! ([`crate::config_resolve::GatedRequest::canonical`]) — a stable, whitespace-
//! and key-order-independent string. A decision is matched by *string equality*
//! on that canonical form, never by a hash: `util::short_hash` is a fast
//! non-cryptographic FNV, fine as a display handle but not a trust key. Change
//! the requested set and the canonical key changes, so a stale approval no
//! longer matches and the request re-prompts.

use crate::config_resolve::GatedRequest;
use crate::util;

/// A short, human-friendly handle for a request (for `config`/CLI display and
/// the `repo_trust.request_id` column). **Not** a trust key — see the module
/// docs; matching is always by canonical string.
pub fn request_id(canonical: &str) -> String {
    util::short_hash(canonical, 8)
}

/// The `(request_id, canonical_json)` pair to persist for a gated request.
pub fn storage_key(req: &GatedRequest) -> (String, String) {
    let canonical = req.canonical();
    (request_id(&canonical), canonical)
}

/// Whether a gated request is covered by the approved canonical set.
pub fn is_approved(req: &GatedRequest, approved_canonical: &[String]) -> bool {
    let c = req.canonical();
    approved_canonical.iter().any(|a| a == &c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req(val: &str) -> GatedRequest {
        GatedRequest {
            key: "sandbox.mounts".into(),
            value: json!(val),
            summary: "m".into(),
        }
    }

    #[test]
    fn id_is_stable_and_short() {
        let c = req("/x:/x").canonical();
        assert_eq!(request_id(&c), request_id(&c));
        assert_eq!(request_id(&c).len(), 8);
    }

    #[test]
    fn changed_request_no_longer_matches() {
        let approved = vec![req("/x:/x").canonical()];
        assert!(is_approved(&req("/x:/x"), &approved));
        // A different requested value → different canonical → re-prompt.
        assert!(!is_approved(&req("/etc:/etc"), &approved));
    }

    #[test]
    fn storage_key_roundtrips() {
        let (id, canonical) = storage_key(&req("/x:/x"));
        assert_eq!(id, request_id(&canonical));
        assert!(is_approved(&req("/x:/x"), &[canonical]));
    }
}
