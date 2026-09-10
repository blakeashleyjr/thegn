//! Host-injected typed secret resolution for service-layer consumers.
//!
//! `thegn-svc` deliberately does not link an OS keyring implementation. The
//! host installs the one broker chokepoint at startup; issue, CI, VPN, and
//! snapshot consumers hand it a parsed [`SecretRef`] plus a value-free consumer
//! tag. Svc-only users and tests retain the legacy env/file/literal fallback,
//! while `keyring:` always fails closed without an installed host resolver.

use std::sync::OnceLock;

use thegn_core::secretref::{BareAs, SecretRef};

/// The host broker callback. A function pointer keeps the process-global seam
/// allocation-free and `Send + Sync`; first installation wins.
pub type Resolver = fn(&SecretRef, &str) -> Option<String>;

static RESOLVER: OnceLock<Resolver> = OnceLock::new();

/// Install the host's typed broker resolver. Idempotent: the first call wins.
pub fn install_resolver(resolver: Resolver) {
    RESOLVER.get_or_init(|| resolver);
}

/// Parse one configured field using its explicit historic bare-string meaning,
/// then resolve through the installed broker.
pub fn resolve(raw: &str, bare_as: BareAs, consumer: &str) -> Option<String> {
    resolve_ref(&SecretRef::parse(raw, bare_as), consumer)
}

/// Resolve an already-parsed ref. Values are never logged here; the installed
/// host broker emits the canonical metadata-only audit event.
pub fn resolve_ref(reference: &SecretRef, consumer: &str) -> Option<String> {
    if !reference.is_configured() {
        return None;
    }
    if let Some(resolve) = RESOLVER.get() {
        return resolve(reference, consumer);
    }
    resolve_local(reference)
}

/// Compatibility fallback for svc-only embeddings and unit tests. It preserves
/// the pre-broker env/file/literal behavior, but a keyring ref never degrades to
/// plaintext or another backend.
fn resolve_local(reference: &SecretRef) -> Option<String> {
    match reference {
        SecretRef::Env { var } => std::env::var(var).ok(),
        SecretRef::File { path } => {
            let path = thegn_core::util::expand_tilde(path);
            std::fs::read_to_string(path).ok()
        }
        SecretRef::Literal(value) => Some(value.expose().to_string()),
        SecretRef::Keyring { .. } => None,
    }
    .map(|value| value.trim().to_string())
    .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_fallback_preserves_explicit_bare_semantics_and_fails_keyring_closed() {
        assert_eq!(
            resolve("literal-token", BareAs::Literal, "test").as_deref(),
            Some("literal-token")
        );
        assert_eq!(
            resolve("keyring:not-installed", BareAs::Literal, "test"),
            None
        );
        assert_eq!(resolve("", BareAs::Literal, "test"), None);
    }
}
