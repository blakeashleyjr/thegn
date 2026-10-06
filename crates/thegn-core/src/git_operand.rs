//! Pure operand validation for git argv positions (THE-387).
//!
//! Git's ref namespace admits names whose short form starts with `-`
//! (`git update-ref refs/heads/--help HEAD`). Placed in an option-parsing
//! position such a name turns into a flag or another subcommand. This is argv
//! option injection, not shell injection. Every mutation entry point validates
//! its operands here and refuses BEFORE spawning git; a valid-but-dash ref is
//! reported unsupported rather than reinterpreted.

use std::fmt;

/// Longest operand accepted (git's own ref-path limit is far larger; this is
/// a sanity bound against pathological input).
const MAX_LEN: usize = 1024;

/// Why an operand was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperandError {
    pub kind: &'static str,
    pub reason: &'static str,
    /// Escaped (`{:?}`-style) rendering of the offending value, safe to show.
    pub shown: String,
}

impl fmt::Display for OperandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid {} {}: {}", self.kind, self.shown, self.reason)
    }
}

impl std::error::Error for OperandError {}

fn err(kind: &'static str, s: &str, reason: &'static str) -> OperandError {
    OperandError {
        kind,
        reason,
        shown: format!("{s:?}"),
    }
}

/// Checks shared by every operand kind.
fn common(kind: &'static str, s: &str) -> Result<(), OperandError> {
    if s.is_empty() {
        return Err(err(kind, s, "empty"));
    }
    if s.len() > MAX_LEN {
        return Err(err(kind, s, "too long"));
    }
    if s.starts_with('-') {
        return Err(err(kind, s, "begins with '-' (would be read as an option)"));
    }
    if s.chars().any(|c| c.is_control()) {
        return Err(err(kind, s, "contains a control character"));
    }
    Ok(())
}

/// `git check-ref-format` rules for a short branch/tag/remote-branch name.
fn ref_format(kind: &'static str, s: &str) -> Result<(), OperandError> {
    common(kind, s)?;
    if s == "@" {
        return Err(err(kind, s, "is the reserved name '@'"));
    }
    if s.contains("..") || s.contains("@{") || s.contains("//") {
        return Err(err(kind, s, "contains a forbidden sequence"));
    }
    if s.chars()
        .any(|c| matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
    {
        return Err(err(kind, s, "contains a forbidden character"));
    }
    if s.starts_with('/') || s.ends_with('/') || s.ends_with('.') {
        return Err(err(kind, s, "bad leading or trailing character"));
    }
    if s.split('/')
        .any(|c| c.starts_with('.') || c.ends_with(".lock"))
    {
        return Err(err(
            kind,
            s,
            "component begins with '.' or ends with '.lock'",
        ));
    }
    Ok(())
}

/// A local branch short name.
pub fn branch_name(s: &str) -> Result<&str, OperandError> {
    ref_format("branch name", s).map(|()| s)
}

/// A tag short name.
pub fn tag_name(s: &str) -> Result<&str, OperandError> {
    ref_format("tag name", s).map(|()| s)
}

/// A remote name (may contain `/`, never a URL or refspec).
pub fn remote_name(s: &str) -> Result<&str, OperandError> {
    ref_format("remote name", s).map(|()| s)
}

/// A commit-ish / revision expression for a mutation (`HEAD~2`, a sha, a
/// branch). Only the option-position hazards are checked; resolution is git's.
pub fn revision(s: &str) -> Result<&str, OperandError> {
    common("revision", s)?;
    if s.chars().any(char::is_whitespace) {
        return Err(err("revision", s, "contains whitespace"));
    }
    Ok(s)
}

/// A bisect term: a plain word that is not a `git bisect` subcommand.
pub fn bisect_term(s: &str) -> Result<&str, OperandError> {
    common("bisect term", s)?;
    const RESERVED: &[&str] = &[
        "start",
        "bad",
        "good",
        "new",
        "old",
        "terms",
        "skip",
        "next",
        "reset",
        "replay",
        "log",
        "run",
        "visualize",
        "view",
        "help",
    ];
    // The built-in marks `good`/`bad`/`new`/`old` are legitimate terms.
    if RESERVED.contains(&s) && !matches!(s, "good" | "bad" | "new" | "old") {
        return Err(err("bisect term", s, "collides with a bisect subcommand"));
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(err("bisect term", s, "must be alphanumeric"));
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dash_names_refused_everywhere() {
        for n in [
            "--help",
            "--detach",
            "--abort",
            "-f",
            "-D",
            "--set-upstream-to=x",
        ] {
            assert!(branch_name(n).is_err(), "{n}");
            assert!(tag_name(n).is_err(), "{n}");
            assert!(remote_name(n).is_err(), "{n}");
            assert!(revision(n).is_err(), "{n}");
            assert!(bisect_term(n).is_err(), "{n}");
        }
    }

    #[test]
    fn ordinary_names_pass() {
        for n in ["main", "feat/x-1", "release_1.2", "a.b", "v1.0.0"] {
            assert_eq!(branch_name(n).unwrap(), n);
            assert_eq!(tag_name(n).unwrap(), n);
        }
        assert_eq!(remote_name("origin").unwrap(), "origin");
        assert_eq!(revision("HEAD~2").unwrap(), "HEAD~2");
        assert_eq!(revision("@{u}").unwrap(), "@{u}");
    }

    #[test]
    fn ref_format_rules() {
        for n in [
            "", "@", "a..b", "a@{b", "a//b", "a b", "a~", "a^", "a:b", "a?", "a*", "a[", "a\\b",
            "/a", "a/", "a.", ".a", "a/.b", "x.lock", "a/x.lock", "a\nb", "a\0b",
        ] {
            assert!(branch_name(n).is_err(), "{n:?}");
        }
        assert!(branch_name(&"a".repeat(MAX_LEN + 1)).is_err());
    }

    #[test]
    fn revision_rejects_whitespace_and_control() {
        assert!(revision("a b").is_err());
        assert!(revision("a\nb").is_err());
        assert!(revision("").is_err());
    }

    #[test]
    fn bisect_terms() {
        for ok in ["good", "bad", "new", "old", "fixed", "broken_2"] {
            assert!(bisect_term(ok).is_ok(), "{ok}");
        }
        for bad in ["reset", "start", "run", "a b", "a/b", "x;y"] {
            assert!(bisect_term(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn error_display_is_escaped() {
        let e = branch_name("--help").unwrap_err();
        assert!(e.to_string().contains("\"--help\""));
        assert!(branch_name("a\nb").unwrap_err().to_string().contains("\\n"));
    }
}
