//! Bounded, redacted CI-log data shared by every read path.
//!
//! Provider output is untrusted text.  This module is deliberately pure so a
//! provider, cache, prompt, terminal, CLI, or control surface can all apply
//! the same bounds and redaction rules before exposing it.

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// The marker used for every value removed from a CI log.
pub const REDACTION_MARKER: &str = "***redacted***";
/// An absolute storage ceiling. Configured limits may be lower, never higher.
pub const HARD_MAX_LOG_BYTES: usize = 4 * 1024 * 1024;
/// An absolute line ceiling used by the cache as a last-resort guard.
pub const HARD_MAX_LOG_LINES: usize = 100_000;

/// A redacted, bounded log entry suitable for persistence or transport.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiLogEntry {
    pub worktree: String,
    pub run_id: String,
    pub job_id: String,
    pub job_name: String,
    pub text: String,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default = "default_true")]
    pub redacted: bool,
    pub fetched_at: i64,
    #[serde(default)]
    pub head_sha: String,
}

fn default_true() -> bool {
    true
}

/// The identity used to deduplicate an autofix handoff.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CiLogCandidate {
    pub worktree: String,
    pub run_id: String,
    pub job_id: String,
    pub head_sha: String,
}

impl CiLogEntry {
    /// Construct an entry while enforcing the public log contract.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        worktree: impl Into<String>,
        run_id: impl Into<String>,
        job_id: impl Into<String>,
        job_name: impl Into<String>,
        text: &str,
        max_lines: usize,
        max_bytes: usize,
        fetched_at: i64,
    ) -> Self {
        let redacted_text = redact(text);
        let (bounded, truncated) = bounded_tail(&redacted_text, max_lines, max_bytes);
        Self {
            worktree: worktree.into(),
            run_id: run_id.into(),
            job_id: job_id.into(),
            job_name: job_name.into(),
            text: bounded,
            truncated,
            redacted: redacted_text != text,
            fetched_at,
            head_sha: String::new(),
        }
    }

    pub fn candidate(&self) -> CiLogCandidate {
        CiLogCandidate {
            worktree: self.worktree.clone(),
            run_id: self.run_id.clone(),
            job_id: self.job_id.clone(),
            head_sha: self.head_sha.clone(),
        }
    }
}

/// Keep the newest complete UTF-8 lines under both limits.
///
/// A zero limit means keep nothing; it never means unlimited. Lines are kept
/// as complete newline-delimited chunks, so a multi-byte character is never
/// split and the returned text is always valid UTF-8.
pub fn bounded_tail(text: &str, max_lines: usize, max_bytes: usize) -> (String, bool) {
    if text.is_empty() {
        return (String::new(), false);
    }
    if max_lines == 0 || max_bytes == 0 {
        return (String::new(), true);
    }

    let chunks: Vec<&str> = text.split_inclusive('\n').collect();
    let mut kept = Vec::new();
    let mut bytes = 0;
    for chunk in chunks.iter().rev().take(max_lines) {
        if bytes + chunk.len() > max_bytes {
            break;
        }
        bytes += chunk.len();
        kept.push(*chunk);
    }
    kept.reverse();
    let out = kept.concat();
    let truncated = out.len() != text.len() || kept.len() < chunks.len();
    (out, truncated)
}

static URL_USERINFO: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)(https?://)[^\s/@:]+(?::[^\s/@]*)?@").unwrap());
static AUTH_HEADER: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(\b(?:authorization|proxy-authorization)\s*:\s*)[^\s]+(?:\s+[^\s]+)?").unwrap()
});
static BEARER: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)(\bbearer\s+)[^\s]+").unwrap());
static DOUBLE_QUOTED_ASSIGNMENT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"(?i)(\b(?:[A-Za-z_][A-Za-z0-9_-]*(?:token|password|passwd|secret|auth|credential)[A-Za-z0-9_-]*|[A-Za-z_][A-Za-z0-9-]*[_-]key(?:[_-]id)?|token|password|passwd|secret|key|api[_-]?key|auth|credential)\s*[=:]\s*)"[^"]*""#,
    )
    .unwrap()
});
static SINGLE_QUOTED_ASSIGNMENT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"(?i)(\b(?:[A-Za-z_][A-Za-z0-9_-]*(?:token|password|passwd|secret|auth|credential)[A-Za-z0-9_-]*|[A-Za-z_][A-Za-z0-9-]*[_-]key(?:[_-]id)?|token|password|passwd|secret|key|api[_-]?key|auth|credential)\s*[=:]\s*)'[^']*'"#,
    )
    .unwrap()
});
static ASSIGNMENT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"(?i)(\b(?:[A-Za-z_][A-Za-z0-9_-]*(?:token|password|passwd|secret|auth|credential)[A-Za-z0-9_-]*|[A-Za-z_][A-Za-z0-9-]*[_-]key(?:[_-]id)?|token|password|passwd|secret|key|api[_-]?key|auth|credential)\s*[=:]\s*)[^\s"'`,;]+"#,
    )
    .unwrap()
});
static JWT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{4,}\.[A-Za-z0-9_-]{8,}\b").unwrap()
});
static AWS_KEY_ID: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b(?:AKIA|ASIA)[A-Z0-9]{16}\b").unwrap());
static PROVIDER_TOKEN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\b(?:gh[pousr]_[A-Za-z0-9_]{20,}|github_pat_[A-Za-z0-9_]{20,}|glpat-[A-Za-z0-9_-]{20,})\b").unwrap()
});

/// Redact credentials commonly emitted by CI tools while retaining ordinary
/// diagnostics and line structure.
pub fn redact(text: &str) -> String {
    // Replace PEM contents line-by-line so a stack of log lines remains a
    // stack of log lines and neither the header nor the key material survives.
    let mut in_pem = false;
    let mut pem_safe = String::with_capacity(text.len());
    for part in text.split_inclusive('\n') {
        let line = part.strip_suffix('\n').unwrap_or(part);
        let begin = line.trim_start().starts_with("-----BEGIN ");
        if begin {
            in_pem = true;
        }
        if in_pem {
            pem_safe.push_str(REDACTION_MARKER);
        } else {
            pem_safe.push_str(line);
        }
        if in_pem && line.trim_start().starts_with("-----END ") {
            in_pem = false;
        }
        if part.ends_with('\n') {
            pem_safe.push('\n');
        }
    }

    let s = URL_USERINFO.replace_all(&pem_safe, format!("$1{REDACTION_MARKER}@"));
    let s = AUTH_HEADER.replace_all(&s, format!("$1{REDACTION_MARKER}"));
    let s = BEARER.replace_all(&s, format!("$1{REDACTION_MARKER}"));
    let s = DOUBLE_QUOTED_ASSIGNMENT.replace_all(&s, format!("$1\"{REDACTION_MARKER}\""));
    let s = SINGLE_QUOTED_ASSIGNMENT.replace_all(&s, format!("$1'{REDACTION_MARKER}'"));
    let s = ASSIGNMENT.replace_all(&s, format!("$1{REDACTION_MARKER}"));
    let s = JWT.replace_all(&s, REDACTION_MARKER);
    let s = AWS_KEY_ID.replace_all(&s, REDACTION_MARKER);
    PROVIDER_TOKEN
        .replace_all(&s, REDACTION_MARKER)
        .into_owned()
}

/// Select the terminal run identities retained by a configured cache policy.
pub fn retained_run_ids(run_ids_newest_first: &[String], keep: usize) -> BTreeSet<String> {
    run_ids_newest_first.iter().take(keep).cloned().collect()
}

/// Compare the full candidate identity, including the PR head SHA.
pub fn same_candidate(a: &CiLogCandidate, b: &CiLogCandidate) -> bool {
    a == b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_newest_complete_lines_and_marks_truncation() {
        let (out, truncated) = bounded_tail("old\nnew-é\n", 1, 32);
        assert_eq!(out, "new-é\n");
        assert!(truncated);
        let (out, truncated) = bounded_tail("éé\nlast\n", 10, 6);
        assert_eq!(out, "last\n");
        assert!(truncated);
    }

    #[test]
    fn zero_limits_are_not_unlimited() {
        assert_eq!(bounded_tail("one\n", 0, 100), (String::new(), true));
        assert_eq!(bounded_tail("one\n", 100, 0), (String::new(), true));
    }

    #[test]
    fn redacts_supported_secret_shapes_and_keeps_diagnostics() {
        let text = concat!(
            "Authorization: Bearer top-secret\n",
            "url=https://alice:password@example.test/x\n",
            "token=plain-secret password: other-secret\n",
            "KEY=plain-key PRIVATE_KEY=private-key ACCESS_KEY=access-key\n",
            "monkey=ordinary-diagnostic\n",
            "jwt eyJabcdefghijk.abcde.lmnopqrstuv\n",
            "AWS AKIA1234567890ABCDEF\n",
            "ghp_123456789012345678901234567890123456\n",
            "glpat-123456789012345678901234567890123456\n",
            "-----BEGIN PRIVATE KEY-----\nsecret-pem\n-----END PRIVATE KEY-----\n",
            "cargo: warning: ordinary diagnostic\n",
        );
        let out = redact(text);
        for secret in [
            "top-secret",
            "password@example",
            "plain-secret",
            "other-secret",
            "plain-key",
            "private-key",
            "access-key",
            "eyJabcdefghijk.abcde.lmnopqrstuv",
            "AKIA1234567890ABCDEF",
            "ghp_123456789012345678901234567890123456",
            "glpat-123456789012345678901234567890123456",
            "secret-pem",
        ] {
            assert!(!out.contains(secret), "secret survived: {secret} in {out}");
        }
        assert!(out.contains(REDACTION_MARKER));
        assert!(out.contains("ordinary diagnostic"));
        assert!(out.contains("ordinary-diagnostic"));
        assert_eq!(out.lines().count(), text.lines().count());
    }

    #[test]
    fn candidate_identity_includes_head_sha() {
        let a = CiLogCandidate {
            worktree: "w".into(),
            run_id: "r".into(),
            job_id: "j".into(),
            head_sha: "a".into(),
        };
        let mut b = a.clone();
        b.head_sha = "b".into();
        assert!(!same_candidate(&a, &b));
    }
}
