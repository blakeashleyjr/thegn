//! Tracker-account token resolution.
//!
//! Parses the config string once into a typed [`SecretRef`] (with
//! [`BareAs::Literal`] — the historic issue/CI-token meaning,
//! `thegn_core::secretref`) and resolves it. `keyring:` needs OS
//! credential-store access, which svc cannot link, so the host installs a
//! resolver at startup ([`install_keyring_resolver`]); without one a `keyring:`
//! ref resolves to `None` with an actionable warning rather than being sent to
//! the tracker as a literal API key.
//!
//! Never logs a value: every diagnostic names the ref via
//! [`SecretRef::audit_name`], which is value-free by construction.

use std::sync::OnceLock;

use thegn_core::secretref::{BareAs, SecretRef};

/// The process's `keyring:` resolver, installed by the host at startup.
///
/// A plain `fn` pointer (not a boxed closure) keeps this `Send + Sync` with no
/// allocation, exactly like `thegn-host`'s `forge_handle` process-global.
static KEYRING: OnceLock<fn(&str) -> Option<String>> = OnceLock::new();

/// Install the process's `keyring:` resolver. Idempotent — the first call wins.
///
/// The resolver is handed the **canonical ref string** (`"keyring:<account>"`),
/// not a bare account name, so the host can pass it straight to its
/// string-taking broker (`thegn-host`'s `secret::resolve_for`) — which parses a
/// bare string as an env-var *name* and would otherwise read the wrong thing
/// entirely.
///
/// A process that never installs (any unit test, any svc-only consumer)
/// degrades to env/file/literal resolution only, which is today's behaviour
/// minus the bogus literal.
pub fn install_keyring_resolver(f: fn(&str) -> Option<String>) {
    // `get_or_init` rather than `set(..)`: a second install is a no-op by
    // design (the first wins), and this says so without an ignored `Result`
    // (the ignored-result ratchet, `test/ignored-result-ratchet.txt`).
    KEYRING.get_or_init(|| f);
}

/// Resolve one `[[issue_accounts]]`/`[issues.*]` token to its value. `None`
/// when the ref is empty, names nothing, or cannot be resolved here.
pub(crate) fn resolve_account_token(raw: &str, provider: &str) -> Option<String> {
    let r = SecretRef::parse(raw, BareAs::Literal);
    if !r.is_configured() {
        return None;
    }
    match &r {
        SecretRef::Env { var } => std::env::var(var).ok().filter(|s| !s.trim().is_empty()),
        SecretRef::File { path } => {
            let p = thegn_core::util::expand_tilde(path);
            std::fs::read_to_string(&p)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        }
        SecretRef::Literal(v) => Some(v.expose().to_string()).filter(|s| !s.is_empty()),
        SecretRef::Keyring { .. } => match KEYRING.get() {
            // `audit_name()` of a keyring ref IS its canonical config string
            // (`keyring:<account>`) and carries no secret — see the install
            // contract above for why the hook gets the whole ref, not the
            // account alone.
            Some(f) => f(&r.audit_name()),
            None => {
                tracing::warn!(
                    target: "thegn::secret",
                    provider,
                    secret_ref = %r.audit_name(),
                    "keyring: tracker token cannot be resolved here — install the host resolver, or use file:/env:"
                );
                None
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_ref_resolves_and_empty_is_none() {
        // SAFETY: single-threaded test; unique var name.
        unsafe { std::env::set_var("TG_ISSUE_TOKEN_TEST_VAR", "lin_env") };
        assert_eq!(
            resolve_account_token("env:TG_ISSUE_TOKEN_TEST_VAR", "linear").as_deref(),
            Some("lin_env")
        );
        unsafe { std::env::remove_var("TG_ISSUE_TOKEN_TEST_VAR") };
        assert_eq!(
            resolve_account_token("env:TG_ISSUE_TOKEN_TEST_VAR", "linear"),
            None
        );
    }

    #[test]
    fn file_ref_reads_and_trims() {
        let f = std::env::temp_dir().join(format!("tg-issue-token-{}.tok", std::process::id()));
        std::fs::write(&f, "  lin_file\n").unwrap();
        assert_eq!(
            resolve_account_token(&format!("file:{}", f.display()), "linear").as_deref(),
            Some("lin_file")
        );
        std::fs::remove_file(&f).expect("fixture file removes");
        // An unreadable file is "not configured", not an empty token.
        assert_eq!(
            resolve_account_token(&format!("file:{}", f.display()), "linear"),
            None
        );
    }

    #[test]
    fn bare_string_is_the_literal_token() {
        assert_eq!(
            resolve_account_token("lin_abc123", "linear").as_deref(),
            Some("lin_abc123")
        );
    }

    #[test]
    fn empty_and_blank_refs_are_none() {
        assert_eq!(resolve_account_token("", "linear"), None);
        assert_eq!(resolve_account_token("   ", "linear"), None);
        assert_eq!(resolve_account_token("keyring:", "linear"), None);
        assert_eq!(resolve_account_token("env:", "linear"), None);
    }

    /// The THE-72 regression: a `keyring:` ref must never be handed to the
    /// provider as the literal string `"keyring:…"`. Holds whether or not a
    /// resolver is installed — the test hook below answers `None` for any
    /// account but its own, so this is order-independent under `cargo test`
    /// (one process, threads) as well as nextest (process per test).
    #[test]
    fn keyring_ref_is_never_the_literal_string() {
        let got = resolve_account_token("keyring:__tg_issue_never_set__", "linear");
        assert_ne!(got.as_deref(), Some("keyring:__tg_issue_never_set__"));
        assert_eq!(got, None);
    }

    /// This is the only test that installs the process-global resolver, so its
    /// hook is always the one in effect afterwards.
    #[test]
    fn installed_hook_resolves_keyring() {
        // The hook sees the canonical ref string, not a bare account name.
        fn hook(secret_ref: &str) -> Option<String> {
            (secret_ref == "keyring:work-linear").then(|| "lin_from_keyring".to_string())
        }
        install_keyring_resolver(hook);
        assert_eq!(
            resolve_account_token("keyring:work-linear", "linear").as_deref(),
            Some("lin_from_keyring")
        );
        assert_eq!(resolve_account_token("keyring:other", "linear"), None);
    }
}
