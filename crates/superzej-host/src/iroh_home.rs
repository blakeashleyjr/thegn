//! Host-side lifecycle for the iroh call-home reach.
//!
//! Owns the process-global home [`IrohHome`] endpoint (a stable EndpointId whose
//! secret key is persisted in the OS keyring) and the per-sandbox token mint.
//! Provisioning injects `SUPERZEJ_HOME_NODE` (from [`home_node_id`]) +
//! `SUPERZEJ_SANDBOX_AUTH` (from [`mint_token`]) into a machine; the machine's
//! baked `sz-agent` dials home; and the pane path routes a connected sandbox's
//! exec through [`current`] (see `pane::relay_exec`).
//!
//! Opt-in via `SUPERZEJ_IROH=1` until a config field lands (config.rs is at its
//! god-file ratchet ceiling).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use std::sync::Arc;

use superzej_svc::iroh_reach::{FnVerifier, IrohHome, TokenVerifier};

/// Keyring account holding the stable home secret key (hex-encoded 32 bytes).
const SECRET_ACCOUNT: &str = "iroh-home-node";

static HOME: OnceLock<Arc<IrohHome>> = OnceLock::new();
static STARTING: AtomicBool = AtomicBool::new(false);

/// Whether the iroh call-home reach is enabled. Opt-in (`SUPERZEJ_IROH=1`) until
/// a config field replaces the env toggle.
pub(crate) fn enabled() -> bool {
    matches!(
        std::env::var("SUPERZEJ_IROH").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn decode_hex32(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

/// Load the stable home secret key from the keyring, generating + persisting one
/// on first use so the EndpointId is stable across restarts.
fn secret_key() -> iroh::SecretKey {
    if let Some(hex) = crate::secret::resolve(&format!("keyring:{SECRET_ACCOUNT}"))
        && let Some(bytes) = decode_hex32(&hex)
    {
        return iroh::SecretKey::from_bytes(&bytes);
    }
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("csprng for iroh home key");
    // best-effort: a keyring failure just means an ephemeral id this session.
    let _ = crate::secret::store(SECRET_ACCOUNT, &encode_hex(&bytes));
    iroh::SecretKey::from_bytes(&bytes)
}

/// The stable home EndpointId string — the value injected as `SUPERZEJ_HOME_NODE`.
/// Derived synchronously from the persisted secret so provisioning can inject it
/// before the endpoint has finished binding.
pub(crate) fn home_node_id() -> String {
    secret_key().public().to_string()
}

/// Ensure the home endpoint is bound + accepting (idempotent, fire-and-forget).
/// Spawns the bind on the ambient tokio runtime; the DB-backed verifier resolves
/// each presented token to its authorized sandbox (rejecting unminted ones).
pub(crate) fn ensure_started() {
    if HOME.get().is_some() || STARTING.swap(true, Ordering::SeqCst) {
        return;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        STARTING.store(false, Ordering::SeqCst);
        return;
    };
    handle.spawn(async {
        let verifier: Arc<dyn TokenVerifier> =
            Arc::new(FnVerifier(|h: &superzej_core::iroh_wire::Hello| {
                superzej_core::db::Db::open()
                    .ok()
                    .and_then(|db| db.verify_iroh_token(&h.token).ok().flatten())
            }));
        match IrohHome::bind(Some(secret_key()), verifier).await {
            Ok((home, _registered)) => {
                let _ = HOME.set(Arc::new(home));
            }
            Err(e) => {
                superzej_core::msg::warn(&format!("iroh home endpoint failed to bind: {e}"));
                STARTING.store(false, Ordering::SeqCst);
            }
        }
    });
}

/// The live home endpoint, if it has finished binding.
pub(crate) fn current() -> Option<Arc<IrohHome>> {
    HOME.get().cloned()
}

/// Mint a fresh random per-sandbox auth token, persist it (v38 `iroh_tokens`), and
/// return it for injection as `SUPERZEJ_SANDBOX_AUTH`. `None` on a DB/CSPRNG error.
pub(crate) fn mint_token(sandbox: &str) -> Option<String> {
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).ok()?;
    let token = format!("szi_{}", encode_hex(&bytes));
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    superzej_core::db::Db::open()
        .ok()?
        .mint_iroh_token(sandbox, &token, now_ms)
        .ok()?;
    Some(token)
}

/// Build the iroh injection for a sandbox being provisioned, when the reach is
/// enabled: starts the home endpoint, mints a token, and returns the three values
/// the machine's `sz-agent` needs. `None` ⇒ today's ssh/IPv4-only path.
pub(crate) fn injection_for(sandbox: &str) -> Option<superzej_svc::fly::IrohInject> {
    if !enabled() {
        return None;
    }
    ensure_started();
    let token = mint_token(sandbox)?;
    Some(superzej_svc::fly::IrohInject {
        home_node: home_node_id(),
        sandbox_auth: token,
        sandbox_id: sandbox.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips_32_bytes() {
        let mut b = [0u8; 32];
        for (i, x) in b.iter_mut().enumerate() {
            *x = i as u8;
        }
        assert_eq!(decode_hex32(&encode_hex(&b)), Some(b));
        assert_eq!(decode_hex32("short"), None);
    }
}
