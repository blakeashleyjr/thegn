//! Approvals from the phone (6a): every new approval request pushes the
//! trusted description plus a 128-bit single-use nonce, stored hashed. The
//! phone answers through `approvals.redeem`.
//!
//! Spine stubs (b155 lane 1); b155 lane 6 fills them.

/// A fresh 128-bit nonce, hex.
#[allow(dead_code, reason = "filled by b155 lane 6")]
pub fn new_nonce() -> String {
    String::new()
}

/// The stored form of a nonce.
#[allow(dead_code, reason = "filled by b155 lane 6")]
pub fn nonce_hash(_n: &str) -> String {
    String::new()
}

/// The push body carrying the trusted description and the nonce.
#[allow(dead_code, reason = "filled by b155 lane 6")]
pub fn push_body(description: &str, _nonce: &str) -> String {
    description.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stubs_are_inert() {
        assert!(new_nonce().is_empty());
        assert!(nonce_hash("n").is_empty());
        assert_eq!(push_body("d", "n"), "d");
    }
}
