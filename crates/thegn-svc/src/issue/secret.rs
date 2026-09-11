//! Tracker-account token resolution.
//!
//! Parses the config string once into a typed [`SecretRef`] (with
//! [`BareAs::Literal`] — the historic issue/CI-token meaning,
//! `thegn_core::secretref`) and resolves it. `keyring:` needs OS
//! credential-store access, which svc cannot link, so the host installs the
//! shared typed resolver at startup (`crate::secret::install_resolver`);
//! without one a `keyring:` ref fails closed rather than being sent to the
//! tracker as a literal API key.
//!
//! Never logs a value: every diagnostic names the ref via
//! [`SecretRef::audit_name`], which is value-free by construction.

use thegn_core::secretref::{BareAs, SecretRef};

/// Resolve one `[[issue_accounts]]`/`[issues.*]` token to its value. `None`
/// when the ref is empty, names nothing, or cannot be resolved here.
pub(crate) fn resolve_account_token(raw: &str, provider: &str) -> Option<String> {
    let r = SecretRef::parse(raw, BareAs::Literal);
    crate::secret::resolve_ref(&r, &format!("issues:{provider}"))
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
}
