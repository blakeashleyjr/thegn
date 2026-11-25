//! The creation marker: a small file thegn writes on a sprite it created, so a
//! different thegn instance on the same Sprites account (stable vs dev keep
//! separate ledgers) can recognise the sprite and adopt it on reattach without
//! a manual `thegn env adopt`.
//!
//! The marker names the sprite, the credential-free account fingerprint and
//! the creating controller, plus a short lease the owner refreshes whenever it
//! reattaches. [`decide`] adopts only when the marker is well formed, names
//! this sprite and account, and either belongs to this controller (the ledger
//! was lost) or its lease has lapsed. A live lease held by another controller
//! always refuses. Everything here is pure; the provider does the I/O.

use serde::{Deserialize, Serialize};

use super::ledger::OwnerScope;

/// Where the marker lives on the sprite.
pub const MARKER_PATH: &str = "/workspace/.thegn/sprite-owner";

/// How long a marker's lease holds other instances off, in seconds.
pub const LEASE_SECS: i64 = 1800;

const VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct Marker {
    v: u32,
    sprite: String,
    account: String,
    owner: String,
    lease_until: i64,
}

/// The marker bytes `scope` writes for `name` at time `now` (unix seconds).
pub fn encode(scope: &OwnerScope, name: &str, now: i64) -> Vec<u8> {
    let marker = Marker {
        v: VERSION,
        sprite: name.to_string(),
        account: scope.account.clone(),
        owner: scope.owner.clone(),
        lease_until: now.saturating_add(LEASE_SECS),
    };
    serde_json::to_vec(&marker).unwrap_or_default()
}

/// May `scope` adopt `name` given the marker `bytes` read from the sprite?
/// `Err` carries the refusal reason.
pub fn decide(bytes: &[u8], scope: &OwnerScope, name: &str, now: i64) -> Result<(), String> {
    let marker: Marker = serde_json::from_slice(bytes)
        .map_err(|_| "its creation marker is unreadable".to_string())?;
    if marker.v != VERSION || marker.owner.is_empty() {
        return Err("its creation marker is not a thegn marker this version knows".into());
    }
    if marker.sprite != name {
        return Err("its creation marker names a different sprite".into());
    }
    if marker.account != scope.account {
        return Err("its creation marker belongs to another Sprites account".into());
    }
    if marker.owner == scope.owner {
        return Ok(());
    }
    if marker.lease_until > now {
        return Err(
            "another thegn instance holds a live lease on it (it refreshes while in use)".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(owner: &str) -> OwnerScope {
        OwnerScope {
            owner: owner.into(),
            account: "acct1-a".into(),
        }
    }

    #[test]
    fn lapsed_foreign_lease_is_adoptable() {
        let bytes = encode(&scope("ctl-dev"), "sx-a", 1000);
        decide(&bytes, &scope("ctl-stable"), "sx-a", 1000 + LEASE_SECS).unwrap();
    }

    #[test]
    fn live_foreign_lease_refuses() {
        let bytes = encode(&scope("ctl-dev"), "sx-a", 1000);
        let reason = decide(&bytes, &scope("ctl-stable"), "sx-a", 1001).unwrap_err();
        assert!(reason.contains("live lease"), "{reason}");
    }

    #[test]
    fn own_marker_is_adoptable_while_leased() {
        let bytes = encode(&scope("ctl-a"), "sx-a", 1000);
        decide(&bytes, &scope("ctl-a"), "sx-a", 1001).unwrap();
    }

    #[test]
    fn wrong_sprite_account_or_garbage_refuses() {
        let bytes = encode(&scope("ctl-dev"), "sx-a", 0);
        decide(&bytes, &scope("ctl-stable"), "sx-b", 99_999).unwrap_err();
        let mut other = scope("ctl-stable");
        other.account = "acct1-b".into();
        decide(&bytes, &other, "sx-a", 99_999).unwrap_err();
        decide(b"not json", &scope("ctl-stable"), "sx-a", 99_999).unwrap_err();
        decide(b"{}", &scope("ctl-stable"), "sx-a", 99_999).unwrap_err();
    }
}
