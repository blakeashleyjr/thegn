//! Canonical issue identity helpers for pipeline admission (THE-262).

use std::fmt;

/// Split an issue id using the historical pipeline artifact-key rule.
///
/// The first component is a provider only when there is a non-empty suffix.
pub fn split_issue_id(issue_id: &str) -> (Option<&str>, &str) {
    match issue_id.split_once(':') {
        Some((provider, key)) if !key.is_empty() => (Some(provider), key),
        _ => (None, issue_id),
    }
}

/// A malformed issue id at a roster write boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueIdentityError {
    Invalid { raw: String },
}

impl fmt::Display for IssueIdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid { raw } => write!(f, "invalid pipeline issue id {raw:?}"),
        }
    }
}

impl std::error::Error for IssueIdentityError {}

/// Trim and normalize the provider portion of an issue id for persistence.
pub fn canonical_issue_id(raw: &str) -> Result<String, IssueIdentityError> {
    let invalid = || IssueIdentityError::Invalid {
        raw: raw.to_string(),
    };
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return Err(invalid());
    }
    if trimmed.ends_with(':') {
        return Err(invalid());
    }
    let (provider, key) = split_issue_id(trimmed);
    if key.is_empty() {
        return Err(invalid());
    }
    if let Some(provider) = provider {
        if provider.is_empty()
            || !provider
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(invalid());
        }
        Ok(format!("{}:{key}", provider.to_ascii_lowercase()))
    } else {
        Ok(key.to_string())
    }
}

/// Compare issue aliases conservatively for duplicate, sibling, and lease checks.
pub fn same_issue(a: &str, b: &str) -> bool {
    let (provider_a, key_a) = split_issue_id(a.trim());
    let (provider_b, key_b) = split_issue_id(b.trim());
    key_a.eq_ignore_ascii_case(key_b)
        && match (provider_a, provider_b) {
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
            _ => true,
        }
}

#[cfg(test)]
mod tests {
    use super::{canonical_issue_id, same_issue, split_issue_id};

    #[test]
    fn canonical_issue_id_lowercases_the_provider_and_trims() {
        assert_eq!(
            canonical_issue_id("  LINEAR:THE-74 ").unwrap(),
            "linear:THE-74"
        );
        assert_eq!(canonical_issue_id("THE-74").unwrap(), "THE-74");
        assert_eq!(
            canonical_issue_id("plugin:acme:K-1").unwrap(),
            "plugin:acme:K-1"
        );
    }

    #[test]
    fn canonical_issue_id_refuses_empty_whitespace_and_control_text() {
        for raw in ["", "   ", "linear:", ":X-1", "linear:THE 74", "linear:X\n"] {
            let error = canonical_issue_id(raw).unwrap_err();
            assert!(error.to_string().contains(&format!("{raw:?}")));
        }
        for raw in ["bad.provider:X-1", "bad/provider:X-1"] {
            assert!(canonical_issue_id(raw).is_err());
        }
    }

    #[test]
    fn same_issue_matches_prefixed_and_unprefixed_aliases() {
        assert!(same_issue("linear:THE-74", "THE-74"));
        assert!(same_issue(" LINEAR:the-74 ", "THE-74"));
    }

    #[test]
    fn same_issue_keeps_distinct_providers_distinct() {
        assert!(!same_issue("jira:X-1", "linear:X-1"));
    }

    #[test]
    fn split_issue_id_matches_issue_key_for_every_existing_shape() {
        for (input, old_raw) in [
            ("linear:THE-76", "THE-76"),
            ("THE-76", "THE-76"),
            ("", ""),
            ("linear:", "linear:"),
            ("a:b:c", "b:c"),
            (":x", "x"),
        ] {
            assert_eq!(split_issue_id(input).1, old_raw, "{input:?}");
        }
    }
}
