//! Token auth for the control API: minting, hashing, and per-request
//! verification against the `pairings` store.
//!
//! The pure format/scope decisions live in `superzej_core::control` (tested
//! under the coverage gate); this layer adds the two things core deliberately
//! doesn't have — a CSPRNG and a hasher. Stored form is `sha256(secret)` hex:
//! the secrets are 256-bit CSPRNG output, so a fast hash already makes a DB
//! leak useless (argon2 would add per-request latency + a dependency to
//! defend low-entropy inputs we never store). Comparison is constant-time.

use superzej_core::control::{
    ScopeSet, TOKEN_ID_HEX, TOKEN_SECRET_HEX, TokenKind, format_token, parse_token,
};
use superzej_core::store::{ControlStore, PairingRow};

/// The authenticated caller adapters attach to a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthCtx {
    /// The token's public id (`pairings.pairing_id`) — safe to log.
    pub pairing_id: String,
    pub label: String,
    pub scopes: ScopeSet,
}

impl AuthCtx {
    /// A local same-uid unix-socket peer (`[serve] local_admin`): full access,
    /// no token row behind it.
    pub fn local_admin() -> Self {
        AuthCtx {
            pairing_id: "local".into(),
            label: "local unix-socket peer".into(),
            scopes: ScopeSet::parse("admin"),
        }
    }

    /// Guard a verb: `Err(NoScope)` when the caller's grant doesn't satisfy it.
    pub fn require(&self, need: superzej_core::control::Scope) -> Result<(), super::ControlError> {
        if self.scopes.allows(need) {
            Ok(())
        } else {
            Err(super::ControlError::NoScope { need })
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// sha-256 of a token's secret half, in the stored hex form.
pub fn hash_secret(secret: &str) -> String {
    use sha2::{Digest, Sha256};
    hex(&Sha256::digest(secret.as_bytes()))
}

fn ct_eq(a: &str, b: &str) -> bool {
    // Constant-time compare: never short-circuits on the first differing byte
    // (both sides are our own fixed-length hashes, so length leaks nothing).
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// A freshly minted credential: the full token string (shown once, never
/// stored) and the row persisted for it.
pub struct Minted {
    pub token: String,
    pub row: PairingRow,
}

/// Mint a credential of `kind` with `scope`, returning the plaintext token and
/// the (hashed) row for [`ControlStore::put_pairing`]. `expires_at` is unix ms.
/// The row is minted pre-approved; the `require_approval` redeem path parks it
/// by clearing `approved_at` before persisting.
pub fn mint(
    kind: TokenKind,
    scope: ScopeSet,
    label: &str,
    parent_id: Option<&str>,
    expires_at: Option<i64>,
    now_ms: i64,
) -> Minted {
    let mut id_bytes = [0u8; TOKEN_ID_HEX / 2];
    let mut secret_bytes = [0u8; TOKEN_SECRET_HEX / 2];
    getrandom::fill(&mut id_bytes).expect("csprng for control token id");
    getrandom::fill(&mut secret_bytes).expect("csprng for control token secret");
    let id = hex(&id_bytes);
    let secret = hex(&secret_bytes);
    let token = format_token(kind, &id, &secret);
    let row = PairingRow {
        pairing_id: id,
        kind: match kind {
            TokenKind::Control => "token".into(),
            TokenKind::PairingCode => "code".into(),
        },
        token_hash: hash_secret(&secret),
        scope: scope.to_csv(),
        label: label.to_string(),
        parent_id: parent_id.map(str::to_string),
        created_at: now_ms,
        expires_at,
        redeemed_at: None,
        approved_at: Some(now_ms),
        revoked_at: None,
    };
    Minted { token, row }
}

/// Verify a presented bearer token against the store. `None` covers every
/// rejection identically (malformed, unknown id, revoked, expired, wrong kind,
/// hash mismatch) so callers have one 401 path.
pub fn verify(store: &dyn ControlStore, token: &str, now_ms: i64) -> Option<AuthCtx> {
    let (kind, parts) = parse_token(token)?;
    if kind != TokenKind::Control {
        return None; // a pairing code is not a bearer credential
    }
    let row = store.pairing_for_auth(&parts.id, now_ms).ok()??;
    if !ct_eq(&row.token_hash, &hash_secret(&parts.secret)) {
        return None;
    }
    Some(AuthCtx {
        pairing_id: row.pairing_id,
        label: row.label,
        scopes: ScopeSet::parse(&row.scope),
    })
}

/// Redeem a pairing code for a fresh control token (the unauthenticated
/// `/v1/pair` flow). Verifies the code's secret hash, consumes it atomically,
/// and mints a `kind = "token"` row inheriting the code's scope. The returned
/// [`Minted`] row is already persisted. With `require_approval` the token is
/// parked (`approved_at` NULL) — unusable until `approve_pairing`.
pub fn redeem(
    store: &dyn ControlStore,
    code: &str,
    label: &str,
    require_approval: bool,
    now_ms: i64,
) -> anyhow::Result<Option<Minted>> {
    let Some((TokenKind::PairingCode, parts)) = parse_token(code) else {
        return Ok(None);
    };
    // Hash-check BEFORE consuming: a wrong secret must not burn the code.
    let Some(row) = store
        .pairings()?
        .into_iter()
        .find(|p| p.pairing_id == parts.id && p.kind == "code")
    else {
        return Ok(None);
    };
    if !ct_eq(&row.token_hash, &hash_secret(&parts.secret)) {
        return Ok(None);
    }
    // Atomic single-use consume (rejects expired/revoked/already-redeemed).
    let Some(code_row) = store.redeem_pairing_code(&parts.id, now_ms)? else {
        return Ok(None);
    };
    let mut minted = mint(
        TokenKind::Control,
        ScopeSet::parse(&code_row.scope),
        label,
        Some(&code_row.pairing_id),
        None,
        now_ms,
    );
    if require_approval {
        minted.row.approved_at = None;
    }
    store.put_pairing(&minted.row)?;
    Ok(Some(minted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use superzej_core::control::Scope;
    use superzej_core::db::Db;

    #[test]
    fn mint_verify_round_trip_and_rejections() {
        let db = Db::open_memory().unwrap();
        let m = mint(
            TokenKind::Control,
            ScopeSet::parse("read,git"),
            "phone",
            None,
            None,
            1_000,
        );
        db.put_pairing(&m.row).unwrap();

        let ctx = verify(&db, &m.token, 2_000).expect("live token verifies");
        assert_eq!(ctx.pairing_id, m.row.pairing_id);
        assert!(ctx.scopes.allows(Scope::Read) && ctx.scopes.allows(Scope::Git));
        assert!(!ctx.scopes.allows(Scope::Write));

        // Wrong secret with a valid id: rejected.
        let (kind, parts) = parse_token(&m.token).unwrap();
        let forged = format_token(kind, &parts.id, &"0".repeat(TOKEN_SECRET_HEX));
        assert!(verify(&db, &forged, 2_000).is_none());
        // Malformed / unknown / code-kind tokens: rejected.
        assert!(verify(&db, "garbage", 2_000).is_none());
        let code = mint(
            TokenKind::PairingCode,
            ScopeSet::parse("read"),
            "",
            None,
            None,
            1_000,
        );
        db.put_pairing(&code.row).unwrap();
        assert!(verify(&db, &code.token, 2_000).is_none());

        // Revocation kills it.
        db.revoke_pairing(&m.row.pairing_id, 3_000).unwrap();
        assert!(verify(&db, &m.token, 4_000).is_none());
    }

    #[test]
    fn expiry_gates_verification() {
        let db = Db::open_memory().unwrap();
        let m = mint(
            TokenKind::Control,
            ScopeSet::parse("read"),
            "",
            None,
            Some(5_000),
            1_000,
        );
        db.put_pairing(&m.row).unwrap();
        assert!(verify(&db, &m.token, 4_999).is_some());
        assert!(verify(&db, &m.token, 5_000).is_none());
    }

    #[test]
    fn redeem_flow_mints_scoped_token_once() {
        let db = Db::open_memory().unwrap();
        let code = mint(
            TokenKind::PairingCode,
            ScopeSet::parse("read,git"),
            "",
            None,
            Some(60_000),
            1_000,
        );
        db.put_pairing(&code.row).unwrap();

        let minted = redeem(&db, &code.token, "my phone", false, 2_000)
            .unwrap()
            .expect("valid code redeems");
        // The minted token verifies with the code's scope and parentage.
        let ctx = verify(&db, &minted.token, 3_000).unwrap();
        assert_eq!(ctx.label, "my phone");
        assert!(ctx.scopes.allows(Scope::Git) && !ctx.scopes.allows(Scope::Write));
        assert_eq!(
            minted.row.parent_id.as_deref(),
            Some(code.row.pairing_id.as_str())
        );

        // Single-use: a second redeem fails; the first token stays valid.
        assert!(
            redeem(&db, &code.token, "again", false, 2_500)
                .unwrap()
                .is_none()
        );
        assert!(verify(&db, &minted.token, 3_000).is_some());

        // A wrong-secret redeem attempt never consumes the code.
        let fresh = mint(
            TokenKind::PairingCode,
            ScopeSet::parse("read"),
            "",
            None,
            None,
            1_000,
        );
        db.put_pairing(&fresh.row).unwrap();
        let (k, p) = parse_token(&fresh.token).unwrap();
        let forged = format_token(k, &p.id, &"0".repeat(TOKEN_SECRET_HEX));
        assert!(
            redeem(&db, &forged, "attacker", false, 2_000)
                .unwrap()
                .is_none()
        );
        assert!(
            redeem(&db, &fresh.token, "owner", false, 2_100)
                .unwrap()
                .is_some(),
            "code survives a forged attempt"
        );
        // A control token is not redeemable.
        assert!(
            redeem(&db, &minted.token, "x", false, 2_200)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn local_admin_holds_every_scope() {
        let ctx = AuthCtx::local_admin();
        for s in [Scope::Read, Scope::Write, Scope::Git, Scope::Admin] {
            assert!(ctx.require(s).is_ok());
        }
    }
}
