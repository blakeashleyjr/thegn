//! Host patterns for `network_allow` / `network_block` and the zone/clamp
//! intersection (resolutions registry B.1). Pure: no I/O, no threads.
//!
//! Created by WS01-P2-01 with exactly the semantics the DNS filter used;
//! WS04-P1-01 extends it in place (`=x` exact, ports, IDNA, IP-literal
//! rejection). A host pattern is a name rule, never an egress boundary.
//!
//! Semantics:
//! 1. `*` matches any name, including a malformed one.
//! 2. `*.x` matches strict subdomains of `x` only, never the apex `x`.
//! 3. A bare `x` matches `x` and every subdomain of `x` (on a label
//!    boundary: `badx` does not match).
//! 4. Matching is case-insensitive, and one trailing dot on the name or the
//!    pattern is ignored.
//! 5. An empty label or any whitespace makes a pattern (or a name) malformed.
//!    A malformed pattern never matches, so a malformed allow entry never
//!    widens a policy and a malformed block entry never blocks.

/// Kind of a parsed host pattern. WS04-P1-01 adds `Exact` (`=x`) and a
/// `ports` field in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostPatternKind {
    /// `*`: any host.
    Any,
    /// Bare `x.com`: the apex and every subdomain.
    Subtree,
    /// `*.x.com`: strict subdomains only (not the apex).
    Subdomains,
}

/// One parsed host pattern. `name` is the normalised name; for
/// [`HostPatternKind::Subdomains`] it is the suffix without the `*.` prefix,
/// and for [`HostPatternKind::Any`] it is `*`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPattern {
    pub kind: HostPatternKind,
    pub name: String,
}

impl HostPattern {
    /// Today's normalisation: trim, strip ONE trailing dot, ASCII-lowercase;
    /// `None` for empty input, whitespace, or an empty label. A `None`
    /// pattern never matches.
    pub fn parse(raw: &str) -> Option<HostPattern> {
        let normalized = norm(raw)?;
        if normalized == "*" {
            return Some(HostPattern {
                kind: HostPatternKind::Any,
                name: normalized,
            });
        }
        if let Some(suffix) = normalized.strip_prefix("*.") {
            return Some(HostPattern {
                kind: HostPatternKind::Subdomains,
                name: suffix.to_string(),
            });
        }
        Some(HostPattern {
            kind: HostPatternKind::Subtree,
            name: normalized,
        })
    }

    /// Whether `host` matches this pattern (rules 1-4 of the module doc).
    pub fn matches(&self, host: &str) -> bool {
        let host = norm(host);
        match self.kind {
            HostPatternKind::Any => true,
            HostPatternKind::Subdomains => host
                .as_deref()
                .is_some_and(|host| host.ends_with(&format!(".{}", self.name))),
            HostPatternKind::Subtree => host.as_deref().is_some_and(|host| {
                host == self.name || host.ends_with(&format!(".{}", self.name))
            }),
        }
    }
}

/// Whether `name` matches `pattern`, the DNS filter's `name_matches` bit for
/// bit: a malformed pattern never matches.
pub fn name_matches(name: &str, pattern: &str) -> bool {
    HostPattern::parse(pattern).is_some_and(|pattern| pattern.matches(name))
}

/// The legacy `nat`-mode advisory semantics of `network_allow` /
/// `network_block`: block wins; an empty allow list allows everything not
/// blocked; a malformed allow entry never matches (so it never widens).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostRules {
    pub allow: Vec<String>,
    pub block: Vec<String>,
}

impl HostRules {
    /// Whether `name` passes the rules.
    pub fn allows(&self, name: &str) -> bool {
        if self.block.iter().any(|pattern| name_matches(name, pattern)) {
            return false;
        }
        if self.allow.is_empty() {
            return true;
        }
        self.allow.iter().any(|pattern| name_matches(name, pattern))
    }
}

/// Trim, strip one trailing dot, lowercase; `None` when empty, containing
/// whitespace, or holding an empty label.
pub(crate) fn norm(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let without_dot = trimmed.strip_suffix('.').unwrap_or(trimmed);
    if without_dot.is_empty()
        || without_dot.chars().any(char::is_whitespace)
        || without_dot.split('.').any(str::is_empty)
    {
        return None;
    }
    let mut normalized = without_dot.to_string();
    normalized.make_ascii_lowercase();
    Some(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(allow: &[&str], block: &[&str]) -> HostRules {
        HostRules {
            allow: allow.iter().map(|s| s.to_string()).collect(),
            block: block.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn bare_matches_apex_and_subdomains() {
        assert!(name_matches("example.com", "example.com"));
        assert!(name_matches("a.example.com", "example.com"));
        assert!(!name_matches("badexample.com", "example.com"));
    }

    #[test]
    fn wildcard_matches_strict_subdomains_not_apex() {
        assert!(name_matches("a.example.com", "*.example.com"));
        assert!(!name_matches("example.com", "*.example.com"));
        assert!(!name_matches("a.other.com", "*.example.com"));
    }

    #[test]
    fn star_matches_any() {
        for name in ["example.com", "a.b.c", "", "a..b", "a b"] {
            assert!(name_matches(name, "*"), "{name:?}");
        }
    }

    #[test]
    fn exact_case_insensitive() {
        assert!(name_matches("example.com", "Example.COM"));
        assert!(name_matches("EXAMPLE.com", "example.com"));
    }

    #[test]
    fn trailing_dot_ignored() {
        assert!(name_matches("example.com.", "example.com"));
        assert!(name_matches("example.com", "example.com."));
        assert!(name_matches("example.com.", "example.com."));
        // Only ONE trailing dot is stripped.
        assert!(!name_matches("example.com..", "example.com"));
    }

    #[test]
    fn malformed_pattern_never_matches() {
        for pattern in ["", "   ", "a..b", "a b", "."] {
            assert!(HostPattern::parse(pattern).is_none(), "{pattern:?}");
            assert!(!name_matches("a.b", pattern), "{pattern:?}");
        }
    }

    #[test]
    fn malformed_allow_never_widens() {
        // An empty allow list allows everything; a malformed allow entry does
        // not turn into "allow everything" and does not match anything.
        assert!(rules(&[], &[]).allows("example.com"));
        assert!(!rules(&["a..b"], &[]).allows("example.com"));
        assert!(!rules(&[""], &[]).allows("example.com"));
    }

    #[test]
    fn block_wins_over_allow() {
        assert!(!rules(&["example.com"], &["example.com"]).allows("example.com"));
        assert!(!rules(&["example.com"], &["*"]).allows("sub.example.com"));
    }

    #[test]
    fn empty_allow_allows_unblocked() {
        let block_only = rules(&[], &["evil.com"]);
        assert!(block_only.allows("good.com"));
        assert!(!block_only.allows("evil.com"));
        assert!(!block_only.allows("sub.evil.com"));
    }

    #[test]
    fn parse_classifies_kinds() {
        assert_eq!(
            HostPattern::parse("*"),
            Some(HostPattern {
                kind: HostPatternKind::Any,
                name: "*".into()
            })
        );
        assert_eq!(
            HostPattern::parse("*."),
            Some(HostPattern {
                kind: HostPatternKind::Any,
                name: "*".into()
            })
        );
        assert_eq!(
            HostPattern::parse("*.Example.COM."),
            Some(HostPattern {
                kind: HostPatternKind::Subdomains,
                name: "example.com".into()
            })
        );
        assert_eq!(
            HostPattern::parse(" Example.com "),
            Some(HostPattern {
                kind: HostPatternKind::Subtree,
                name: "example.com".into()
            })
        );
    }

    // Moved verbatim from `dns_filter.rs` (WS01-P2-01); `DnsPolicy` became
    // `HostRules`.

    #[test]
    fn policy_allows_all_when_lists_empty() {
        assert!(rules(&[], &[]).allows("anything.example.com"));
    }

    #[test]
    fn policy_allow_list_restricts() {
        let p = rules(&["api.anthropic.com", "github.com"], &[]);
        assert!(p.allows("api.anthropic.com"));
        assert!(p.allows("sub.github.com"));
        assert!(!p.allows("evil.com"));
    }

    #[test]
    fn wildcard_pattern() {
        let p = rules(&["*.internal.example.com"], &[]);
        assert!(p.allows("foo.internal.example.com"));
        assert!(!p.allows("internal.example.com")); // wildcard doesn't match root
        assert!(!p.allows("external.example.com"));
    }

    #[test]
    fn name_matches_trailing_dots_normalised() {
        assert!(name_matches("example.com", "example.com."));
        assert!(name_matches("sub.example.com", "example.com."));
        assert!(name_matches("sub.example.com", "example.com"));
        assert!(!name_matches("notexample.com", "example.com"));
    }

    #[test]
    fn name_matches_exact_and_suffix() {
        assert!(name_matches("example.com", "example.com"));
        assert!(name_matches("a.b.example.com", "example.com"));
        // suffix must be on a label boundary
        assert!(!name_matches("fooexample.com", "example.com"));
    }

    #[test]
    fn name_matches_universal_star() {
        // `*` is the deny-all pattern the resolution engine emits.
        assert!(name_matches("example.com", "*"));
        assert!(name_matches("anything.internal", "*"));
        assert!(name_matches("a.b.c.d", "*"));
        // Blocking `*` denies every name even against an allow list.
        let p = rules(&["example.com"], &["*"]);
        assert!(!p.allows("example.com"));
        assert!(!p.allows("sub.example.com"));
    }

    #[test]
    fn block_is_case_insensitive() {
        let policy = rules(&[], &["blocked.test"]);
        assert!(!policy.allows("BLOCKED.TEST"));
        assert!(!policy.allows("Blocked.Test."));
        assert!(!policy.allows("sub.BLOCKED.test"));
    }

    #[test]
    fn pattern_case_is_normalised() {
        assert!(!rules(&[], &["Blocked.TEST."]).allows("blocked.test"));
    }

    #[test]
    fn allows_normalizes_subdomain_and_wildcard_pattern_case() {
        assert!(!rules(&[], &["Blocked.TEST"]).allows("sub.blocked.test."));
        assert!(!rules(&[], &["*.Example.COM."]).allows("a.example.com"));
        assert!(!rules(&[], &["*"]).allows("any."));
    }

    #[test]
    fn wildcard_and_universal_case_insensitive() {
        assert!(!rules(&[], &["*.Example.COM"]).allows("a.example.com."));
        assert!(name_matches("A.Example.COM.", "*.example.com"));
        assert!(!rules(&[], &["*"]).allows("ANY."));
        assert!(name_matches("ANY.", "*"));
    }

    #[test]
    fn mixed_case_matches_exact_subdomain_and_wildcard_patterns() {
        for (name, pattern) in [
            ("BLOCKED.TEST", "blocked.test"),
            ("BLOCKED.TEST.", "blocked.test."),
            ("sub.BLOCKED.TEST", "blocked.test"),
            ("sub.BLOCKED.TEST.", "blocked.test."),
            ("a.Example.COM", "*.example.com"),
            ("a.Example.COM.", "*.example.com."),
            ("ANY", "*"),
            ("ANY.", "*."),
            ("blocked.test", "Blocked.Test"),
            ("blocked.test.", "Blocked.Test."),
            ("sub.blocked.test", "Blocked.Test"),
            ("sub.blocked.test.", "Blocked.Test."),
            ("a.example.com", "*.Example.COM"),
            ("a.example.com.", "*.Example.COM."),
            ("anything", "*"),
            ("anything.", "*."),
        ] {
            assert!(name_matches(name, pattern), "{name:?} against {pattern:?}");
        }
    }

    #[test]
    fn malformed_patterns_never_match() {
        for pattern in ["", "a..b", "a b", "."] {
            assert!(!name_matches("a.b", pattern), "pattern {pattern:?}");
            assert!(
                rules(&[], &[pattern]).allows("a.b"),
                "malformed block {pattern:?}"
            );
        }
        assert!(name_matches("", "*"));
        assert!(!name_matches("a..b", "a.b"));
    }

    #[test]
    fn norm_trims_whitespace_and_one_trailing_dot() {
        assert_eq!(norm("  A.Test.  "), Some("a.test".into()));
        assert_eq!(norm("a.."), None);
        assert_eq!(norm("a\tb"), None);
    }

    #[test]
    fn policy_trims_trailing_dot_on_query() {
        let p = rules(&["example.com"], &[]);
        assert!(p.allows("example.com."));
        assert!(p.allows("sub.example.com."));
    }

    #[test]
    fn wildcard_does_not_match_unrelated_suffix() {
        let p = rules(&["*.example.com"], &[]);
        assert!(p.allows("a.example.com"));
        assert!(!p.allows("example.com"));
        assert!(!p.allows("a.other.com"));
    }
}
