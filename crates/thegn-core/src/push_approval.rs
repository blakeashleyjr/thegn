//! Approvals from the phone (6a): every new approval request pushes the
//! trusted description plus a 128-bit single-use nonce, stored hashed. The
//! phone answers through `approvals.redeem`.
//!
//! Pure helpers only. The CSPRNG draw lives with the caller (thegn-host's
//! `getrandom`; thegn-core carries no RNG dependency): it fills 16 bytes and
//! hands them to [`nonce_from_bytes`].

use sha2::{Digest, Sha256};
use std::fmt::Write as _;

/// Nonce length in raw bytes (128 bits).
pub const NONCE_BYTES: usize = 16;
/// Nonce length as lowercase hex.
pub const NONCE_HEX_LEN: usize = NONCE_BYTES * 2;
/// Upper bound of a push body, in bytes.
pub const MAX_BODY_BYTES: usize = 1024;

/// A nonce's wire form: 32 lowercase hex characters of fresh OS randomness.
pub fn nonce_from_bytes(bytes: &[u8; NONCE_BYTES]) -> String {
    let mut out = String::with_capacity(NONCE_HEX_LEN);
    for byte in bytes {
        write!(out, "{byte:02x}").expect("writing to String cannot fail");
    }
    out
}

/// Whether `s` has a nonce's exact shape: 32 lowercase hex characters.
pub fn is_nonce_shape(s: &str) -> bool {
    s.len() == NONCE_HEX_LEN && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The stored form of a nonce: lowercase hex sha256. The plaintext nonce is
/// never persisted; a redeem looks its hash up.
pub fn nonce_hash(n: &str) -> String {
    let digest = Sha256::digest(n.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in digest {
        write!(out, "{byte:02x}").expect("writing to String cannot fail");
    }
    out
}

/// The push body: the trusted description, then the reply line carrying the
/// nonce. Bounded at [`MAX_BODY_BYTES`]: the description is truncated (on a
/// char boundary, with an ellipsis) so the reply line always survives intact.
/// No links.
pub fn push_body(description: &str, nonce: &str) -> String {
    let reply = format!(
        "\n\nReply: approvals.redeem {{\"nonce\":\"{nonce}\",\"decision\":\"approve|reject\"}}"
    );
    let room = MAX_BODY_BYTES.saturating_sub(reply.len());
    let description = description.trim();
    let head = if description.len() <= room {
        description.to_string()
    } else {
        const ELLIPSIS: &str = "…";
        let mut cut = room.saturating_sub(ELLIPSIS.len());
        while cut > 0 && !description.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}{ELLIPSIS}", &description[..cut])
    };
    let mut body = head;
    body.push_str(&reply);
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONCE: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn nonce_from_bytes_is_32_lowercase_hex() {
        let n = nonce_from_bytes(&[0xAB; NONCE_BYTES]);
        assert_eq!(n, "ab".repeat(16));
        assert!(is_nonce_shape(&n));
        let z = nonce_from_bytes(&[0; NONCE_BYTES]);
        assert_eq!(z.len(), NONCE_HEX_LEN);
        assert!(is_nonce_shape(&z));
    }

    #[test]
    fn nonce_shape_is_strict() {
        assert!(is_nonce_shape(NONCE));
        assert!(!is_nonce_shape(&NONCE.to_uppercase()));
        assert!(!is_nonce_shape(&NONCE[..31]));
        assert!(!is_nonce_shape(&format!("{NONCE}0")));
        assert!(!is_nonce_shape("g123456789abcdef0123456789abcdef"));
        assert!(!is_nonce_shape(""));
    }

    #[test]
    fn nonce_hash_is_sha256_hex() {
        // sha256("") known vector.
        assert_eq!(
            nonce_hash(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let h = nonce_hash(NONCE);
        assert_eq!(h.len(), 64);
        assert!(
            h.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        assert_ne!(h, NONCE);
        assert!(!h.contains(NONCE));
        assert_eq!(h, nonce_hash(NONCE));
        assert_ne!(h, nonce_hash("0123456789abcdef0123456789abcdee"));
    }

    #[test]
    fn push_body_carries_description_and_reply_line() {
        let body = push_body("Land linear:THE-1 at abc1234", NONCE);
        assert!(body.starts_with("Land linear:THE-1 at abc1234"));
        assert!(body.ends_with(&format!(
            "Reply: approvals.redeem {{\"nonce\":\"{NONCE}\",\"decision\":\"approve|reject\"}}"
        )));
        assert!(!body.contains("http://"));
        assert!(!body.contains("https://"));
    }

    #[test]
    fn push_body_is_bounded_and_keeps_the_reply() {
        for long in ["x".repeat(5000), "é".repeat(3000), "🦀".repeat(800)] {
            let body = push_body(&long, NONCE);
            assert!(body.len() <= MAX_BODY_BYTES, "{} bytes", body.len());
            assert!(body.contains(&format!("\"nonce\":\"{NONCE}\"")));
            assert!(body.contains('…'));
        }
    }

    #[test]
    fn push_body_holds_no_secret_besides_the_nonce() {
        let body = push_body("d", NONCE);
        // Only the description, the fixed reply template and the nonce.
        let expected = format!(
            "d\n\nReply: approvals.redeem {{\"nonce\":\"{NONCE}\",\"decision\":\"approve|reject\"}}"
        );
        assert_eq!(body, expected);
        assert!(!body.contains(&nonce_hash(NONCE)));
    }

    #[test]
    fn inbox_admits_redeem_when_allowlisted_and_still_denies_resolve() {
        use crate::control::{Scope, ScopeSet};
        use crate::push_inbox::{RejectReason, admit};
        let allow: std::collections::BTreeSet<String> = ["approvals.redeem", "approvals.resolve"]
            .into_iter()
            .map(String::from)
            .collect();
        let ceiling = ScopeSet::parse("read,write,admin");
        assert_eq!(admit("approvals.redeem", &allow, ceiling), Ok(Scope::Write));
        // Admin-deny is unconditional, allowlisted or not.
        assert_eq!(
            admit("approvals.resolve", &allow, ceiling),
            Err(RejectReason::AdminDenied)
        );
        // Not allowlisted: refused.
        assert_eq!(
            admit("approvals.redeem", &Default::default(), ceiling),
            Err(RejectReason::NotAllowed)
        );
        // A read-only ceiling cannot redeem.
        assert_eq!(
            admit("approvals.redeem", &allow, ScopeSet::parse("read")),
            Err(RejectReason::ScopeExceeded)
        );
    }
}
