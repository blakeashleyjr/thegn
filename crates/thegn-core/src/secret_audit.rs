//! The value-free secret audit trail (THE-66).
//!
//! Every broker resolution emits one structured event on target
//! `thegn::secret::audit` carrying **who** asked (a static consumer tag), for
//! **which** ref (its value-free [`audit_name`](crate::secretref::SecretRef::audit_name)),
//! from **which** backend, and the **outcome** — never the secret value. A leak
//! investigation replays these; `thegn secret audit` and `thegn doctor` read
//! the same presence pass.
//!
//! Free when off: with no tracing subscriber installed the event is not
//! constructed past the cheap fields (house instrumentation rule), and
//! resolution already runs off the render loop, so this adds no wake source.

use crate::secretref::SecretRef;

/// What happened when a ref was resolved. Names an outcome class, never a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretOutcome {
    /// The value was found and returned.
    Resolved,
    /// The ref names nothing set (unset var, absent keyring entry, missing file).
    Missing,
    /// A backend was present but refused (locked keychain).
    Denied,
    /// No usable backend of the ref's kind here (headless, no Secret Service).
    Unavailable,
}

impl SecretOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            SecretOutcome::Resolved => "resolved",
            SecretOutcome::Missing => "missing",
            SecretOutcome::Denied => "denied",
            SecretOutcome::Unavailable => "unavailable",
        }
    }
    /// Whether the ref produced a usable value.
    pub fn ok(self) -> bool {
        self == SecretOutcome::Resolved
    }
}

/// One audit record. Every field is value-free by construction: `ref_name` is
/// [`SecretRef::audit_name`] (a literal is redacted there), `backend` is a kind
/// string, `consumer` is a component tag.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SecretAudit {
    /// The value-free ref name (`keyring:work-linear`, `env:FLY_API_TOKEN`,
    /// `literal:***redacted***`).
    pub ref_name: String,
    /// The backend kind that answered (`keyring`/`file`/`env`/`literal`).
    pub backend: &'static str,
    /// The component that asked (`provider:fly`, `issues:linear`, `snapshot`,
    /// `mcp:<upstream>`, `agent_task`, …).
    pub consumer: String,
    /// What happened.
    pub outcome: SecretOutcome,
}

impl SecretAudit {
    /// Build a record for a resolution of `r` by `consumer` with `outcome`.
    pub fn new(r: &SecretRef, consumer: impl Into<String>, outcome: SecretOutcome) -> Self {
        SecretAudit {
            ref_name: r.audit_name(),
            backend: match r.backend_kind() {
                // Interned to `&'static str` for the tracing field.
                "keyring" => "keyring",
                "file" => "file",
                "env" => "env",
                _ => "literal",
            },
            consumer: consumer.into(),
            outcome,
        }
    }

    /// Emit the structured tracing event. A no-op cost when no subscriber is
    /// installed. Values never appear — the fields are all value-free.
    pub fn record(&self) {
        tracing::event!(
            target: "thegn::secret::audit",
            tracing::Level::DEBUG,
            ref_name = %self.ref_name,
            backend = self.backend,
            consumer = %self.consumer,
            outcome = self.outcome.as_str(),
            "secret resolve",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secretref::BareAs;

    const SENTINEL: &str = "SENTINEL_sk-live-0xC0FFEE_leak";

    #[test]
    fn outcome_strings_and_ok() {
        assert!(SecretOutcome::Resolved.ok());
        assert!(!SecretOutcome::Missing.ok());
        assert_eq!(SecretOutcome::Unavailable.as_str(), "unavailable");
    }

    #[test]
    fn audit_names_a_keyring_ref_faithfully() {
        let r = SecretRef::parse("keyring:work-linear", BareAs::EnvName);
        let a = SecretAudit::new(&r, "issues:linear", SecretOutcome::Resolved);
        assert_eq!(a.ref_name, "keyring:work-linear");
        assert_eq!(a.backend, "keyring");
        assert_eq!(a.consumer, "issues:linear");
    }

    /// The enforced-not-promised redaction guarantee: a literal secret's value
    /// appears in NO rendering of the audit event — not Debug, not the JSON
    /// serialization (the optional sink), not the constructed fields.
    #[test]
    fn a_literal_secret_never_appears_in_any_audit_rendering() {
        let r = SecretRef::parse(SENTINEL, BareAs::Literal);
        let a = SecretAudit::new(&r, "issues:linear", SecretOutcome::Resolved);

        let dbg = format!("{a:?}");
        assert!(!dbg.contains(SENTINEL), "leaked via Debug: {dbg}");

        let json = serde_json::to_string(&a).unwrap();
        assert!(!json.contains(SENTINEL), "leaked via serde: {json}");

        assert!(
            !a.ref_name.contains(SENTINEL),
            "leaked via ref_name: {}",
            a.ref_name
        );
        assert_eq!(a.backend, "literal");
    }
}
