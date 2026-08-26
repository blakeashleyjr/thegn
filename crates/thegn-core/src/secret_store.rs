//! The `SecretStore` provider seam (THE-66).
//!
//! One chokepoint, backends behind a seam: this is the pure, substrate-free
//! vocabulary (the trait, the kind enum, the classed error). The keyring FFI
//! and file I/O implementations live host-side (`thegn-host/src/secret.rs`),
//! exactly like every other seam whose real work needs a substrate the core
//! crate must not depend on.
//!
//! Shared with `add-mcp-proxy-hub`: there must never be two secret-resolution
//! layers. `thegn mcp secret …` is a namespaced view over this same store.
//!
//! Backends map to [`SecretRef`](crate::secretref::SecretRef) schemes:
//! `keyring:` → [`SecretBackendKind::Keyring`], `file:` →
//! [`SecretBackendKind::File`], `env:`/bare-env → [`SecretBackendKind::Env`].
//! `exec` (an external `pass`/`op`/vault-agent command) is **reserved**: the
//! seam is the deliverable, the implementation is future work.

use crate::config::{config_enum, config_warn};
use crate::seam::{ErrorClass, Probe, ProbeReport, SeamError};

config_enum! {
    /// The secret backends. `keyring`/`file`/`env` are implemented; `exec`
    /// (external secret-manager command) is reserved — accepted by config,
    /// rejected by `--strict` validation, no sub-table until implemented.
    pub enum SecretBackendKind : "secret backend" {
        Keyring = "keyring",
        File    = "file",
        Env     = "env",
        Exec    = "exec" reserved,
    } default = Keyring;
}

/// What a backend can do beyond `get`. Persisting (set/del/list) is the
/// keyring's and the file store's business; the env backend can only read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct SecretBackendCaps {
    /// The backend can persist a value (`set`) and remove it (`del`).
    pub writable: bool,
    /// The backend can enumerate the names it holds (never the values).
    pub listable: bool,
}

/// A seam-classed secret error, so a caller and `thegn doctor` can tell
/// **unavailable** (no credential store here — falls through to file/env) from
/// **denied** (a store that is present but locked/refusing) from **not found**
/// (the named account/var/file is simply unset).
#[derive(Debug, Clone)]
pub struct SecretError {
    class: ErrorClass,
    msg: String,
}

impl SecretError {
    /// No usable backend of this kind here (headless box, no Secret Service).
    /// Classed to fall through a degradation ladder.
    pub fn unavailable(msg: impl Into<String>) -> Self {
        SecretError {
            class: ErrorClass::NotInstalled,
            msg: msg.into(),
        }
    }
    /// A store that is present but refused (locked keychain, no unlock UI).
    /// A real answer — does not fall through.
    pub fn denied(msg: impl Into<String>) -> Self {
        SecretError {
            class: ErrorClass::Auth,
            msg: msg.into(),
        }
    }
    /// The named account / variable / file is unset.
    pub fn not_found(msg: impl Into<String>) -> Self {
        SecretError {
            class: ErrorClass::NotFound,
            msg: msg.into(),
        }
    }
    /// The value the caller asked for (message only — never a secret).
    pub fn message(&self) -> &str {
        &self.msg
    }
}

impl std::fmt::Display for SecretError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({:?})", self.msg, self.class)
    }
}
impl std::error::Error for SecretError {}

impl SeamError for SecretError {
    fn class(&self) -> ErrorClass {
        self.class
    }
    fn unsupported(op: &'static str) -> Self {
        SecretError {
            class: ErrorClass::Unsupported,
            msg: format!("operation not supported by this backend: {op}"),
        }
    }
}

/// One secret backend. Object-safe (the broker holds `Vec<Box<dyn SecretStore>>`
/// / picks one per ref scheme). `get` resolves an account name to its value;
/// the persistence operations are optional (their caps bit governs them) and
/// default to `unsupported`.
pub trait SecretStore: Probe + Send + Sync {
    /// The backend kind this store implements.
    fn kind(&self) -> SecretBackendKind;
    /// What this backend can do beyond reading.
    fn caps(&self) -> SecretBackendCaps;
    /// Resolve an account/name to its secret value, or a classed error.
    fn get(&self, account: &str) -> Result<String, SecretError>;
    /// Persist `value` under `account` (optional).
    fn set(&self, _account: &str, _value: &str) -> Result<(), SecretError> {
        Err(SecretError::unsupported("set"))
    }
    /// Remove `account` (optional).
    fn del(&self, _account: &str) -> Result<(), SecretError> {
        Err(SecretError::unsupported("del"))
    }
    /// The names this backend holds — **never the values** (optional).
    fn list(&self) -> Result<Vec<String>, SecretError> {
        Err(SecretError::unsupported("list"))
    }
}

/// A reserved-kind probe row (`exec`), so `thegn doctor` explains why the
/// external-command backend is unavailable. The implemented backends build
/// their own [`ProbeReport`] host-side.
pub fn reserved_probe(kind: SecretBackendKind) -> ProbeReport {
    ProbeReport::reserved("secret", kind.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seam::Kind;

    #[test]
    fn kinds_implemented_or_reserved() {
        // exec is the only reserved kind; the other three are implemented.
        let implemented: Vec<&str> = SecretBackendKind::implemented()
            .map(|k| k.as_str())
            .collect();
        assert_eq!(implemented, vec!["keyring", "file", "env"]);
        assert!(SecretBackendKind::Exec.is_reserved());
        assert!(SecretBackendKind::from_str_validated("exec").is_err());
        assert_eq!(
            SecretBackendKind::from_str_validated("keyring").unwrap(),
            SecretBackendKind::Keyring
        );
    }

    #[test]
    fn error_classes_map_to_ladder_behavior() {
        // Unavailable falls through (try the next layer); denied/not-found are
        // real answers.
        assert!(SecretError::unavailable("no dbus").falls_through());
        assert!(!SecretError::denied("locked").falls_through());
        assert!(!SecretError::not_found("unset").falls_through());
        assert_eq!(SecretError::not_found("x").class(), ErrorClass::NotFound);
    }

    #[test]
    fn reserved_probe_names_exec() {
        let r = reserved_probe(SecretBackendKind::Exec);
        assert_eq!(r.seam, "secret");
        assert!(r.availability.is_unavailable());
    }

    // A tiny in-memory store proves the trait is object-safe and the defaults
    // behave.
    struct EnvOnly;
    impl Probe for EnvOnly {
        fn probe(&self) -> ProbeReport {
            ProbeReport::new("secret", "env", crate::seam::Availability::Ready)
        }
    }
    impl SecretStore for EnvOnly {
        fn kind(&self) -> SecretBackendKind {
            SecretBackendKind::Env
        }
        fn caps(&self) -> SecretBackendCaps {
            SecretBackendCaps::default()
        }
        fn get(&self, account: &str) -> Result<String, SecretError> {
            if account == "PRESENT" {
                Ok("v".into())
            } else {
                Err(SecretError::not_found(account.to_string()))
            }
        }
    }

    #[test]
    fn object_safe_and_defaults_are_unsupported() {
        let s: Box<dyn SecretStore> = Box::new(EnvOnly);
        assert_eq!(s.get("PRESENT").unwrap(), "v");
        assert!(s.get("nope").is_err());
        assert_eq!(
            s.set("a", "b").unwrap_err().class(),
            ErrorClass::Unsupported
        );
        assert_eq!(s.del("a").unwrap_err().class(), ErrorClass::Unsupported);
        assert_eq!(s.list().unwrap_err().class(), ErrorClass::Unsupported);
        assert!(!s.caps().writable);
    }
}
