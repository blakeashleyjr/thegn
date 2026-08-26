//! The typed secret-reference vocabulary (THE-66).
//!
//! Every config field that names a secret is a plain `String` on the wire (no
//! schema change, no home-manager churn) but parses **once at load** into a
//! [`SecretRef`] so the rest of thegn manipulates a type, not a stringly-typed
//! convention. The type does three jobs the old two `expand_env_ref` /
//! `secret::resolve` layers could not:
//!
//! 1. **Names the historic bare-string meanings.** Two config layers disagreed
//!    on what a scheme-less string meant: `api_key_env = "FLY_API_TOKEN"` was an
//!    env-var *name*, while an issue-tracker `token = "lin_abc"` was the literal
//!    *value*. [`SecretRef::parse`] takes a [`BareAs`] marker so each field
//!    family keeps its meaning, spelled out instead of implied.
//! 2. **Makes "no secret in logs" a compile-time fact, not a promise.** A
//!    [`SecretRef::Literal`] carries its value in a [`LiteralSecret`] newtype
//!    with a redacted `Debug`, no `Display`, and no `Serialize` — the only way
//!    to read the value is the explicit [`SecretRef::expose_literal`] the broker
//!    calls. A unit test asserts a known sentinel never appears in any
//!    rendering.
//! 3. **Gives the broker one vocabulary.** Resolution (keyring/file/env I/O)
//!    lives in the host/svc broker impls, which match on the four variants; the
//!    audit trail names a ref by [`SecretRef::audit_name`] (scheme + operand,
//!    never a literal value).
//!
//! Pure and substrate-free: no I/O here. `keyring:`/`file:`/`env:` are resolved
//! by the broker; only [`SecretRef::Literal`] carries an inline value.

use crate::redact::PLACEHOLDER;

/// How a scheme-less (bare) string on a secret field is interpreted. Preserves
/// each field family's historic meaning so no existing config breaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BareAs {
    /// A bare string is an environment-variable **name** (the historic
    /// `api_key_env` / provider-token meaning). `"FLY_API_TOKEN"` ⇒
    /// [`SecretRef::Env`].
    EnvName,
    /// A bare string is the literal secret **value** (the historic
    /// issue-tracker / CI token meaning). `"lin_abc123"` ⇒
    /// [`SecretRef::Literal`], which warns and is migratable.
    Literal,
}

/// A literal secret value pasted into config. Deprecated but not broken: it
/// resolves exactly as before, while `thegn config validate` warns and
/// `thegn secret migrate` moves it into the keyring (or a `0600` file).
///
/// The value is private and reachable only through [`LiteralSecret::expose`].
/// `Debug` is redacted; there is deliberately no `Display` and no `Serialize`,
/// so a literal cannot leak into a log line, a formatted error, or a serialized
/// config by accident.
#[derive(Clone, PartialEq, Eq)]
pub struct LiteralSecret(String);

impl LiteralSecret {
    /// Wrap a raw secret value.
    pub fn new(value: impl Into<String>) -> Self {
        LiteralSecret(value.into())
    }
    /// The one deliberate read path — the broker calls this to resolve. Named
    /// loudly so a review notices a value leaving the type.
    pub fn expose(&self) -> &str {
        &self.0
    }
    /// Whether the literal is the empty string (⇒ "not configured").
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for LiteralSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LiteralSecret({PLACEHOLDER})")
    }
}

/// A parsed reference to a secret. Carries no I/O; the broker resolves the
/// first three variants and reads [`SecretRef::Literal`] inline.
///
/// `Debug` is safe to log: the non-literal variants carry only names/paths
/// (not secrets), and the literal variant delegates to [`LiteralSecret`]'s
/// redacted `Debug`. There is intentionally no `Display` and no `Serialize` —
/// config keeps the raw `String`; this type is the parsed, load-time view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SecretRef {
    /// `keyring:<account>` — an OS keyring entry under the thegn service.
    Keyring { account: String },
    /// `env:VAR` (and bare-as-env-name fields) — an environment variable.
    Env { var: String },
    /// `file:PATH` — a `0600` file (agenix/sops tmpfs targets land here). `~`
    /// is expanded by the broker at read time.
    File { path: String },
    /// A legacy literal value pasted into config plaintext (deprecated).
    Literal(LiteralSecret),
}

impl SecretRef {
    /// Parse a config string into a typed ref. `bare` decides what a
    /// scheme-less string means for this field family.
    ///
    /// Whitespace around the whole string and around a scheme's operand is
    /// trimmed, matching the old `resolve`/`expand_env_ref` behavior. A bare
    /// literal is **not** trimmed of internal content but is trimmed of
    /// surrounding whitespace only when parsed as an env name (an env var name
    /// never has spaces); a literal value is preserved verbatim except for the
    /// outer trim the old path also applied.
    pub fn parse(s: &str, bare: BareAs) -> SecretRef {
        let t = s.trim();
        if let Some(account) = t.strip_prefix("keyring:") {
            return SecretRef::Keyring {
                account: account.trim().to_string(),
            };
        }
        if let Some(var) = t.strip_prefix("env:") {
            return SecretRef::Env {
                var: var.trim().to_string(),
            };
        }
        if let Some(path) = t.strip_prefix("file:") {
            return SecretRef::File {
                path: path.trim().to_string(),
            };
        }
        match bare {
            BareAs::EnvName => SecretRef::Env { var: t.to_string() },
            BareAs::Literal => SecretRef::Literal(LiteralSecret::new(t.to_string())),
        }
    }

    /// Whether this ref actually names something (a non-empty operand). An
    /// empty field parses to an un-configured ref; callers treat that as "not
    /// set", exactly as the old resolve returned `None` for an empty string.
    pub fn is_configured(&self) -> bool {
        match self {
            SecretRef::Keyring { account } => !account.is_empty(),
            SecretRef::Env { var } => !var.is_empty(),
            SecretRef::File { path } => !path.is_empty(),
            SecretRef::Literal(v) => !v.is_empty(),
        }
    }

    /// The backend kind that resolves this ref (`keyring`/`env`/`file`/`literal`).
    /// Used by the audit event and doctor.
    pub fn backend_kind(&self) -> &'static str {
        match self {
            SecretRef::Keyring { .. } => "keyring",
            SecretRef::Env { .. } => "env",
            SecretRef::File { .. } => "file",
            SecretRef::Literal(_) => "literal",
        }
    }

    /// Whether this ref is a deprecated inline literal (⇒ warn + migrate).
    pub fn is_literal(&self) -> bool {
        matches!(self, SecretRef::Literal(_))
    }

    /// A value-free name for this ref, safe to log and to record in the audit
    /// trail: the scheme plus its (non-secret) operand for keyring/env/file, and
    /// a redacted placeholder for a literal.
    pub fn audit_name(&self) -> String {
        match self {
            SecretRef::Keyring { account } => format!("keyring:{account}"),
            SecretRef::Env { var } => format!("env:{var}"),
            SecretRef::File { path } => format!("file:{path}"),
            SecretRef::Literal(_) => format!("literal:{PLACEHOLDER}"),
        }
    }

    /// The canonical `config.toml` string for a **non-literal** ref, so
    /// `thegn secret migrate` can write the returned ref back into config. A
    /// literal has no safe config form (that is the thing being migrated away),
    /// so it returns `None`.
    pub fn to_config_string(&self) -> Option<String> {
        match self {
            SecretRef::Keyring { account } => Some(format!("keyring:{account}")),
            SecretRef::Env { var } => Some(format!("env:{var}")),
            SecretRef::File { path } => Some(format!("file:{path}")),
            SecretRef::Literal(_) => None,
        }
    }

    /// The inline literal value, for the broker's resolve path only. `None` for
    /// every non-literal variant (those need I/O the broker performs).
    pub fn expose_literal(&self) -> Option<&str> {
        match self {
            SecretRef::Literal(v) => Some(v.expose()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A distinctive byte sequence that must never appear in any rendering of a
    /// literal ref. If it shows up in a `Debug`, an audit name, or a config
    /// string, redaction has a hole.
    const SENTINEL: &str = "SENTINEL_sk-live-0xC0FFEE_leak";

    #[test]
    fn bare_env_name_stays_env() {
        let r = SecretRef::parse("FLY_API_TOKEN", BareAs::EnvName);
        assert_eq!(
            r,
            SecretRef::Env {
                var: "FLY_API_TOKEN".into()
            }
        );
        assert_eq!(r.backend_kind(), "env");
        assert!(r.is_configured());
    }

    #[test]
    fn bare_literal_field_is_literal() {
        let r = SecretRef::parse("lin_abc123", BareAs::Literal);
        assert!(r.is_literal());
        assert_eq!(r.backend_kind(), "literal");
        assert_eq!(r.expose_literal(), Some("lin_abc123"));
    }

    #[test]
    fn schemes_parse_uniformly_regardless_of_bare_marker() {
        for bare in [BareAs::EnvName, BareAs::Literal] {
            assert_eq!(
                SecretRef::parse("keyring:work-linear", bare),
                SecretRef::Keyring {
                    account: "work-linear".into()
                }
            );
            assert_eq!(
                SecretRef::parse("env:VAR", bare),
                SecretRef::Env { var: "VAR".into() }
            );
            assert_eq!(
                SecretRef::parse("file:/etc/tok", bare),
                SecretRef::File {
                    path: "/etc/tok".into()
                }
            );
        }
    }

    #[test]
    fn keyring_ref_works_for_a_literal_field_too() {
        // The whole point: a keyring ref on an issue-token (bare=Literal) field
        // is a keyring ref, not a literal token sent to the tracker.
        let r = SecretRef::parse("keyring:work-linear", BareAs::Literal);
        assert_eq!(r.backend_kind(), "keyring");
        assert_eq!(r.expose_literal(), None);
    }

    #[test]
    fn whitespace_is_trimmed() {
        assert_eq!(
            SecretRef::parse("  keyring:  a  ", BareAs::Literal),
            SecretRef::Keyring {
                account: "a".into()
            }
        );
        assert_eq!(
            SecretRef::parse(" env: V ", BareAs::EnvName),
            SecretRef::Env { var: "V".into() }
        );
    }

    #[test]
    fn empty_is_unconfigured() {
        assert!(!SecretRef::parse("", BareAs::EnvName).is_configured());
        assert!(!SecretRef::parse("   ", BareAs::Literal).is_configured());
        assert!(!SecretRef::parse("keyring:", BareAs::EnvName).is_configured());
        assert!(!SecretRef::parse("file:", BareAs::EnvName).is_configured());
    }

    #[test]
    fn to_config_string_roundtrips_non_literals() {
        assert_eq!(
            SecretRef::parse("keyring:acct", BareAs::EnvName).to_config_string(),
            Some("keyring:acct".into())
        );
        assert_eq!(
            SecretRef::parse("env:V", BareAs::EnvName).to_config_string(),
            Some("env:V".into())
        );
        assert_eq!(
            SecretRef::parse("file:/p", BareAs::EnvName).to_config_string(),
            Some("file:/p".into())
        );
        // A literal has no safe config form.
        assert_eq!(
            SecretRef::parse("x", BareAs::Literal).to_config_string(),
            None
        );
    }

    /// The redaction sentinel test: a literal's value must appear in NO
    /// rendering — not `Debug` of the ref, not `Debug` of the inner secret, not
    /// the audit name, not the config string.
    #[test]
    fn literal_value_never_appears_in_any_rendering() {
        let r = SecretRef::parse(SENTINEL, BareAs::Literal);

        let dbg_ref = format!("{r:?}");
        assert!(
            !dbg_ref.contains(SENTINEL),
            "leaked via SecretRef Debug: {dbg_ref}"
        );
        assert!(
            dbg_ref.contains(PLACEHOLDER),
            "Debug should show placeholder: {dbg_ref}"
        );

        let inner = LiteralSecret::new(SENTINEL);
        let dbg_inner = format!("{inner:?}");
        assert!(
            !dbg_inner.contains(SENTINEL),
            "leaked via LiteralSecret Debug: {dbg_inner}"
        );

        let audit = r.audit_name();
        assert!(!audit.contains(SENTINEL), "leaked via audit_name: {audit}");

        // A non-literal ref's operand is a name/path, not a secret, so it *is*
        // allowed to appear — assert the audit name is faithful there.
        let kr = SecretRef::parse("keyring:work-linear", BareAs::Literal);
        assert_eq!(kr.audit_name(), "keyring:work-linear");

        // The only sanctioned exposure returns the value verbatim.
        assert_eq!(r.expose_literal(), Some(SENTINEL));
    }
}
