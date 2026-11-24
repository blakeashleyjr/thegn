//! Host patterns for `network_allow` / `network_block`, the zone/clamp
//! intersection, the egress proxy, provider rules and credential hosts
//! (resolutions registry B.1). Pure: no I/O, no threads.
//!
//! Created by WS01-P2-01 with exactly the semantics the DNS filter used;
//! WS04-P1-01 extended it in place into the one host-pattern grammar. A host
//! pattern is a name rule, never an egress boundary.
//!
//! Grammar (owner decision E7: a one-way door — once a list is written in this
//! grammar, changing what an entry means silently narrows or widens a policy):
//!
//! | entry | what it matches |
//! | --- | --- |
//! | `*` | any host; only as a whole entry (no port suffix) |
//! | `x.com` | `x.com` and every subdomain of it |
//! | `*.x.com` | strict subdomains of `x.com`, never the apex |
//! | `=x.com` | exactly `x.com` |
//!
//! Each name form takes an optional port suffix: `:443` (one port),
//! `:8000-8100` (an inclusive range) or `:*` (any port). Without a suffix the
//! policy's own `ports` list applies ([`PortSpec::Default`]).
//!
//! Normalisation: surrounding whitespace is trimmed, one trailing dot is
//! dropped, and the name is lowercased and IDNA-encoded to A-labels
//! (`Bücher.de` and `xn--bcher-kva.de` are the same pattern). A name longer
//! than 253 bytes, with an empty label, with a `_` in its registrable part, with
//! a stray `*`/`=`, or that is an IP literal is refused at parse time. IP
//! literals are canonicalised first, so `2852039166`, `0x7f.1` and `127.1` are
//! refused exactly like `127.0.0.1`, with an error that points at `allow_ip`.
//!
//! Semantics:
//! 1. `*` matches any name, including a malformed one.
//! 2. `*.x` matches strict subdomains of `x` only, never the apex `x`.
//! 3. A bare `x` matches `x` and every subdomain of it (on a label
//!    boundary: `badx` does not match).
//! 4. Matching is case-insensitive, and one trailing dot on the name or the
//!    pattern is ignored.
//! 5. An empty label or any whitespace makes a pattern (or a name) malformed.
//!    A malformed pattern never matches, so a malformed allow entry never
//!    widens a policy and a malformed block entry never blocks.
//! 6. Ports are checked before names: an entry that carries a port suffix
//!    matches that port only, whatever the policy's `ports` say.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The longest DNS name accepted, excluding the root dot (RFC 1035).
const MAX_NAME_BYTES: usize = 253;

/// Kind of a parsed host pattern. WS04-P1-01 added `Exact` (`=x`) in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PatternKind {
    /// `*`: any host.
    Any,
    /// Bare `x.com`: the apex and every subdomain.
    Subtree,
    /// `*.x.com`: strict subdomains only (not the apex).
    Subdomains,
    /// `=x.com`: exactly that host.
    Exact,
}

/// The ports a pattern is restricted to. [`PortSpec::Default`] defers to the
/// policy's own `ports` list (an empty list there means "any port").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PortSpec {
    /// No suffix: the policy's `ports` apply.
    Default,
    /// `:<port>`, one port.
    One(u16),
    /// `:<lo>-<hi>`, inclusive.
    Range(u16, u16),
    /// `:*`, any port.
    Any,
}

impl PortSpec {
    /// Whether `port` is allowed, given the policy's `default_ports` (used only
    /// by [`PortSpec::Default`]; an empty list there means "any port").
    fn allows(&self, port: u16, default_ports: &[u16]) -> bool {
        match *self {
            PortSpec::Any => true,
            PortSpec::One(only) => only == port,
            PortSpec::Range(lo, hi) => (lo..=hi).contains(&port),
            PortSpec::Default => default_ports.is_empty() || default_ports.contains(&port),
        }
    }

    /// Whether every port `other` allows is also allowed by `self`.
    ///
    /// [`PortSpec::Default`] against an explicit port is never provable — this
    /// module does not know the policy's `ports` list — so that comparison
    /// answers `false`, which makes the clamp deny rather than widen.
    fn covers(&self, other: &PortSpec) -> bool {
        match (*self, *other) {
            (PortSpec::Any, _) => true,
            (_, PortSpec::Any) => false,
            (PortSpec::Default, PortSpec::Default) => true,
            (PortSpec::Default, _) | (_, PortSpec::Default) => false,
            (PortSpec::One(a), PortSpec::One(b)) => a == b,
            (PortSpec::One(a), PortSpec::Range(lo, hi)) => lo == a && hi == a,
            (PortSpec::Range(lo, hi), PortSpec::One(b)) => (lo..=hi).contains(&b),
            (PortSpec::Range(a, b), PortSpec::Range(lo, hi)) => a <= lo && hi <= b,
        }
    }
}

/// One parsed host pattern. `name` is the canonical (lowercase, IDNA A-label)
/// name; for [`PatternKind::Subdomains`] it is the suffix without the `*.`
/// prefix, and for [`PatternKind::Any`] it is `*`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPattern {
    pub kind: PatternKind,
    pub name: String,
    pub ports: PortSpec,
}

impl HostPattern {
    /// Parse one entry of the grammar. Every rejection is a
    /// [`PatternError`] with a stable message, so a config layer can attach the
    /// key path (`sandbox.egress.allow[2]`) to it.
    pub fn parse(raw: &str) -> Result<HostPattern, PatternError> {
        HostPattern::parse_inner(raw, false)
    }

    /// Parse with a bare entry meaning exactly that host (B16): credential
    /// `hosts` are exact by default, so `api.example.com` never widens to
    /// `*.api.example.com`. The explicit forms keep their meaning.
    pub fn parse_exact_default(raw: &str) -> Result<HostPattern, PatternError> {
        HostPattern::parse_inner(raw, true)
    }

    fn parse_inner(raw: &str, bare_is_exact: bool) -> Result<HostPattern, PatternError> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(PatternError::Empty);
        }
        if raw.chars().any(char::is_whitespace) {
            return Err(PatternError::Whitespace);
        }
        let (host, ports) = split_ports(raw)?;
        let (kind, name) = classify(host, bare_is_exact)?;
        if kind == PatternKind::Any {
            if ports != PortSpec::Default {
                return Err(PatternError::AnyWithPort);
            }
            return Ok(HostPattern {
                kind,
                name: "*".to_string(),
                ports,
            });
        }
        Ok(HostPattern {
            kind,
            name: canonical_name(name)?,
            ports,
        })
    }

    /// Whether `host` on `port` matches this pattern (rules 1-6 of the module
    /// docs). `default_ports` is the policy's own `ports` list, used only by
    /// entries without a port suffix; an empty list means "any port".
    pub fn matches(&self, host: &str, port: u16, default_ports: &[u16]) -> bool {
        self.ports.allows(port, default_ports) && self.matches_name(host)
    }

    /// Whether every host and port `other` matches is also matched by `self`.
    ///
    /// Monotone: if `a.covers(b)` then anything `b` matches, `a` matches too.
    /// The clamp uses it to accept a repo entry only when the trusted ceiling
    /// provably covers it, so an unpinnable comparison answers `false`.
    pub fn covers(&self, other: &HostPattern) -> bool {
        self.ports.covers(&other.ports) && self.name_covers(other)
    }

    /// Name-only test: this pattern's port suffix is ignored because the caller
    /// has no port. A policy check uses [`matches`](Self::matches) instead.
    fn matches_name(&self, host: &str) -> bool {
        if self.kind == PatternKind::Any {
            return true;
        }
        normalize_query(host).is_some_and(|host| self.matches_canonical(&host))
    }

    /// Match an already canonical name.
    fn matches_canonical(&self, host: &str) -> bool {
        match self.kind {
            PatternKind::Any => true,
            PatternKind::Exact => host == self.name,
            PatternKind::Subdomains => strict_subdomain(host, &self.name),
            PatternKind::Subtree => host == self.name || strict_subdomain(host, &self.name),
        }
    }

    /// Whether every name `other` matches is also matched by `self` (the port
    /// half of [`covers`](Self::covers)).
    fn name_covers(&self, other: &HostPattern) -> bool {
        if self.kind == PatternKind::Any {
            return true;
        }
        if other.kind == PatternKind::Any {
            return false;
        }
        if self.kind == PatternKind::Exact {
            // The narrowest rule: `=x` covers only `=x` itself.
            return other.kind == PatternKind::Exact && other.name == self.name;
        }
        match other.kind {
            // `=h` is covered when `self` matches `h` (the apex counts for
            // `Subtree`, never for `Subdomains`).
            PatternKind::Exact => self.matches_canonical(&other.name),
            // Everything under `m` must sit under `n`: `m` being on or below
            // `n` puts all of its subdomains below `n` too.
            PatternKind::Subdomains => subdomain_or_equal(&other.name, &self.name),
            // `m` *and* its subdomains must sit under `n`, so `m` has to be
            // strictly below `n` for a `Subdomains` ceiling (which excludes the
            // apex) and on-or-below for a `Subtree` one.
            PatternKind::Subtree => {
                if self.kind == PatternKind::Subdomains {
                    strict_subdomain(&other.name, &self.name)
                } else {
                    subdomain_or_equal(&other.name, &self.name)
                }
            }
            PatternKind::Any => false,
        }
    }
}

impl fmt::Display for HostPattern {
    /// The canonical string form, the one a config write (WS04-P1-07) stores.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            PatternKind::Any => f.write_str("*")?,
            PatternKind::Subtree => f.write_str(&self.name)?,
            PatternKind::Subdomains => write!(f, "*.{}", self.name)?,
            PatternKind::Exact => write!(f, "={}", self.name)?,
        }
        match self.ports {
            PortSpec::Default => Ok(()),
            PortSpec::One(port) => write!(f, ":{port}"),
            PortSpec::Range(lo, hi) => write!(f, ":{lo}-{hi}"),
            PortSpec::Any => f.write_str(":*"),
        }
    }
}

impl std::str::FromStr for HostPattern {
    type Err = PatternError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        HostPattern::parse(raw)
    }
}

impl Serialize for HostPattern {
    /// Serialises as its canonical string, so a policy round-trips through
    /// config, the plan and the wire unchanged.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for HostPattern {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        HostPattern::parse(&raw).map_err(<D::Error as serde::de::Error>::custom)
    }
}

/// Why an entry is not a host pattern. Each variant has one stable message
/// (see the `Display` impl) so a config layer can point at the exact key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternError {
    /// The entry is empty.
    Empty,
    /// The entry holds whitespace.
    Whitespace,
    /// The entry has an empty label (`a..b`, a leading or a second trailing dot).
    EmptyLabel,
    /// The name is longer than 253 bytes.
    TooLong,
    /// A `_` sits where a host label belongs.
    UnderscoreInRegistrable,
    /// The entry is an IP address, not a name.
    IpLiteral,
    /// A port suffix is not `:443`, `:8000-8100` or `:*`.
    InvalidPort,
    /// `*` matched with a port suffix; `*` stands alone.
    AnyWithPort,
    /// A `*` or `=` where the grammar does not allow one.
    StraySyntax,
    /// The name is not a valid host name after normalisation.
    MalformedName,
}

impl fmt::Display for PatternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            PatternError::Empty => "host pattern is empty",
            PatternError::Whitespace => "host pattern contains whitespace",
            PatternError::EmptyLabel => "host pattern has an empty label",
            PatternError::TooLong => "host pattern is longer than 253 bytes",
            PatternError::UnderscoreInRegistrable => {
                "host pattern has an underscore in its registrable domain"
            }
            PatternError::IpLiteral => {
                "host pattern is an IP address; addresses belong in `allow_ip`"
            }
            PatternError::InvalidPort => {
                "host pattern port must be `:<1-65535>`, `:<lo>-<hi>` or `:*`"
            }
            PatternError::AnyWithPort => "`*` matches every host and takes no port suffix",
            PatternError::StraySyntax => {
                "`*` and `=` are only allowed as the whole entry or as a leading `*.`/`=`"
            }
            PatternError::MalformedName => "host pattern is not a valid host name",
        };
        f.write_str(message)
    }
}

impl std::error::Error for PatternError {}

/// Whether `name` matches `pattern`, the DNS filter's `name_matches` bit for
/// bit: a malformed pattern never matches.
///
/// Name-only: a port suffix on `pattern` is ignored, because the caller has no
/// port to check. A policy decision uses [`HostPattern::matches`] instead.
pub fn name_matches(name: &str, pattern: &str) -> bool {
    HostPattern::parse(pattern).is_ok_and(|pattern| pattern.matches_name(name))
}

/// WS01-P2-01's `network_allow` / `network_block` rule set, now an alias over
/// the parsed patterns (B.1): block wins, and an empty allow list allows
/// everything not blocked.
///
/// A list is parsed once, so a rule set cannot hold a malformed entry: the
/// broken entry is a [`PatternError`] at the key (WS04-P1-04 attaches the key
/// path) instead of a rule that silently disappears — which would turn a
/// restricted allow list into "allow everything".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostRules {
    pub allow: Vec<HostPattern>,
    pub block: Vec<HostPattern>,
}

impl HostRules {
    /// Whether `host` on `port` passes the rules. `default_ports` is the
    /// policy's own `ports` list, used by entries without a port suffix.
    pub fn allows(&self, host: &str, port: u16, default_ports: &[u16]) -> bool {
        if self
            .block
            .iter()
            .any(|pattern| pattern.matches(host, port, default_ports))
        {
            return false;
        }
        if self.allow.is_empty() {
            return true;
        }
        self.allow
            .iter()
            .any(|pattern| pattern.matches(host, port, default_ports))
    }
}

/// Split an optional port suffix off the entry. Anything bracketed is an IPv6
/// literal by the grammar, so it is refused with the IP-literal error rather
/// than being read as a name with a port.
fn split_ports(raw: &str) -> Result<(&str, PortSpec), PatternError> {
    if raw.starts_with('[') {
        return Err(PatternError::IpLiteral);
    }
    match raw.rsplit_once(':') {
        Some((host, ports)) => Ok((host, parse_ports(ports)?)),
        None => Ok((raw, PortSpec::Default)),
    }
}

fn parse_ports(raw: &str) -> Result<PortSpec, PatternError> {
    if raw == "*" {
        return Ok(PortSpec::Any);
    }
    if let Some((lo, hi)) = raw.split_once('-') {
        let lo = parse_port(lo)?;
        let hi = parse_port(hi)?;
        if lo > hi {
            return Err(PatternError::InvalidPort);
        }
        return Ok(PortSpec::Range(lo, hi));
    }
    Ok(PortSpec::One(parse_port(raw)?))
}

fn parse_port(raw: &str) -> Result<u16, PatternError> {
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(PatternError::InvalidPort);
    }
    match raw.parse::<u16>() {
        Ok(port) if port > 0 => Ok(port),
        _ => Err(PatternError::InvalidPort),
    }
}

/// Classify the name part and hand back the name still to be canonicalised.
/// One trailing dot is dropped here (a name may legitimately end in the root
/// dot; two is malformed).
fn classify(host: &str, bare_is_exact: bool) -> Result<(PatternKind, &str), PatternError> {
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() {
        return Err(PatternError::Empty);
    }
    if host.split('.').any(str::is_empty) {
        return Err(PatternError::EmptyLabel);
    }
    if host == "*" {
        return Ok((PatternKind::Any, "*"));
    }
    if let Some(suffix) = host.strip_prefix("*.") {
        return Ok((PatternKind::Subdomains, suffix));
    }
    if let Some(name) = host.strip_prefix('=') {
        return Ok((PatternKind::Exact, name));
    }
    let kind = if bare_is_exact {
        PatternKind::Exact
    } else {
        PatternKind::Subtree
    };
    Ok((kind, host))
}

/// Lowercase, IDNA-encode and police the name: length, empty labels, stray
/// grammar, underscores where a host label belongs, and IP literals (which
/// `url` canonicalises to `Host::Ipv4`/`Host::Ipv6` for us, inet_aton forms
/// included).
fn canonical_name(name: &str) -> Result<String, PatternError> {
    // The caller guarantees a non-empty name.
    if name.len() > MAX_NAME_BYTES {
        return Err(PatternError::TooLong);
    }
    if name.contains(['*', '=']) {
        return Err(PatternError::StraySyntax);
    }
    if underscore_in_registrable(name) {
        return Err(PatternError::UnderscoreInRegistrable);
    }
    let canonical = match url::Host::parse(name) {
        Ok(url::Host::Domain(domain)) => domain,
        Ok(url::Host::Ipv4(_) | url::Host::Ipv6(_)) => return Err(PatternError::IpLiteral),
        Err(_) => return Err(PatternError::MalformedName),
    };
    if canonical.len() > MAX_NAME_BYTES {
        return Err(PatternError::TooLong);
    }
    if canonical.split('.').any(str::is_empty) {
        return Err(PatternError::EmptyLabel);
    }
    Ok(canonical)
}

/// Whether a `_` sits in the registrable part of the name. Service labels
/// (`_dmarc.x.com`, `_http._tcp.x.com`) are legitimate and survive; a `_` in
/// the last two labels — the registrable domain, without consulting a public
/// suffix list — is not.
fn underscore_in_registrable(name: &str) -> bool {
    let mut labels = name.rsplit('.');
    labels.next().is_some_and(|label| label.contains('_'))
        || labels.next().is_some_and(|label| label.contains('_'))
}

/// Whether `host` sits strictly below `suffix` on a label boundary. Both are
/// canonical ASCII names (IDNA A-labels), so the byte index is a char boundary.
fn strict_subdomain(host: &str, suffix: &str) -> bool {
    host.len() > suffix.len()
        && host.ends_with(suffix)
        && host.as_bytes()[host.len() - suffix.len() - 1] == b'.'
}

/// Whether `host` is `suffix` or sits strictly below it.
fn subdomain_or_equal(host: &str, suffix: &str) -> bool {
    host == suffix || strict_subdomain(host, suffix)
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

/// Canonicalise a host as it arrives in a request: the legacy tolerances (one
/// trailing dot, case, surrounding space) and then the same IDNA encoding a
/// pattern gets, so both sides of a comparison are A-labels. `None` when the
/// name is malformed; only `*` matches a malformed name.
fn normalize_query(host: &str) -> Option<String> {
    let normalized = norm(host)?;
    match url::Host::parse(&normalized) {
        Ok(url::Host::Domain(domain)) => Some(domain),
        Ok(url::Host::Ipv4(addr)) => Some(addr.to_string()),
        Ok(url::Host::Ipv6(addr)) => Some(addr.to_string()),
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(raw: &str) -> HostPattern {
        HostPattern::parse(raw).expect("pattern parses")
    }

    fn rules(allow: &[&str], block: &[&str]) -> HostRules {
        HostRules {
            allow: allow.iter().map(|entry| pattern(entry)).collect(),
            block: block.iter().map(|entry| pattern(entry)).collect(),
        }
    }

    // --- WS01-P2-01's semantics, unchanged -------------------------------

    #[test]
    fn bare_matches_apex_and_subdomains() {
        assert!(name_matches("example.com", "example.com"));
        assert!(name_matches("a.example.com", "example.com"));
        assert!(!name_matches("badexample.com", "example.com"));

        let bare = pattern("example.com");
        assert_eq!(bare.kind, PatternKind::Subtree);
        assert!(bare.matches("example.com", 443, &[]));
        assert!(bare.matches("deep.a.example.com", 443, &[]));
        assert!(!bare.matches("badexample.com", 443, &[]));
    }

    #[test]
    fn star_excludes_apex() {
        assert!(name_matches("a.example.com", "*.example.com"));
        assert!(!name_matches("example.com", "*.example.com"));
        assert!(!name_matches("a.other.com", "*.example.com"));

        let star = pattern("*.example.com");
        assert_eq!(star.kind, PatternKind::Subdomains);
        assert_eq!(star.name, "example.com");
        assert!(star.matches("A.Example.COM.", 443, &[]));
        assert!(!star.matches("example.com", 443, &[]));
        assert!(!star.matches("badexample.com", 443, &[]));
        assert!(!star.matches("a.example.com.evil", 443, &[]));
    }

    #[test]
    fn star_matches_any() {
        for name in ["example.com", "a.b.c", "", "a..b", "a b"] {
            assert!(name_matches(name, "*"), "{name:?}");
        }
        let any = pattern("*");
        assert_eq!(any.kind, PatternKind::Any);
        for host in ["example.com", "", "a..b"] {
            assert!(any.matches(host, 443, &[]), "{host:?}");
        }
        assert_eq!(pattern("*.").kind, PatternKind::Any);
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
    fn malformed_entry_is_a_parse_error_not_a_vanishing_rule() {
        // A rule set holds parsed patterns, so a broken entry fails at its key
        // instead of silently dropping out of the list — which would widen
        // "allow these hosts" into "allow all".
        let entries = ["api.example.com", "a..b", "github.com"];
        let parsed: Result<Vec<HostPattern>, PatternError> = entries
            .iter()
            .map(|entry| HostPattern::parse(entry))
            .collect();
        assert_eq!(parsed, Err(PatternError::EmptyLabel));
    }

    #[test]
    fn block_wins_over_allow() {
        assert!(!rules(&["example.com"], &["example.com"]).allows("example.com", 443, &[]));
        assert!(!rules(&["example.com"], &["*"]).allows("sub.example.com", 443, &[]));
    }

    #[test]
    fn empty_allow_allows_unblocked() {
        let block_only = rules(&[], &["evil.com"]);
        assert!(block_only.allows("good.com", 443, &[]));
        assert!(!block_only.allows("evil.com", 443, &[]));
        assert!(!block_only.allows("sub.evil.com", 443, &[]));
    }

    #[test]
    fn parse_classifies_kinds() {
        assert_eq!(
            pattern("*"),
            HostPattern {
                kind: PatternKind::Any,
                name: "*".into(),
                ports: PortSpec::Default,
            }
        );
        assert_eq!(
            pattern("*."),
            HostPattern {
                kind: PatternKind::Any,
                name: "*".into(),
                ports: PortSpec::Default,
            }
        );
        assert_eq!(
            pattern("*.Example.COM."),
            HostPattern {
                kind: PatternKind::Subdomains,
                name: "example.com".into(),
                ports: PortSpec::Default,
            }
        );
        assert_eq!(
            pattern(" Example.com "),
            HostPattern {
                kind: PatternKind::Subtree,
                name: "example.com".into(),
                ports: PortSpec::Default,
            }
        );
        assert_eq!(
            pattern(" =Example.COM "),
            HostPattern {
                kind: PatternKind::Exact,
                name: "example.com".into(),
                ports: PortSpec::Default,
            }
        );
    }

    // Moved verbatim from the deleted DNS filter module (WS01-P2-01); its policy
    // became `HostRules`.

    #[test]
    fn policy_allows_all_when_lists_empty() {
        assert!(rules(&[], &[]).allows("anything.example.com", 443, &[]));
    }

    #[test]
    fn policy_allow_list_restricts() {
        let p = rules(&["api.anthropic.com", "github.com"], &[]);
        assert!(p.allows("api.anthropic.com", 443, &[]));
        assert!(p.allows("sub.github.com", 443, &[]));
        assert!(!p.allows("evil.com", 443, &[]));
    }

    #[test]
    fn wildcard_pattern() {
        let p = rules(&["*.internal.example.com"], &[]);
        assert!(p.allows("foo.internal.example.com", 443, &[]));
        assert!(!p.allows("internal.example.com", 443, &[])); // wildcard doesn't match root
        assert!(!p.allows("external.example.com", 443, &[]));
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
        assert!(!p.allows("example.com", 443, &[]));
        assert!(!p.allows("sub.example.com", 443, &[]));
    }

    #[test]
    fn block_is_case_insensitive() {
        let policy = rules(&[], &["blocked.test"]);
        assert!(!policy.allows("BLOCKED.TEST", 443, &[]));
        assert!(!policy.allows("Blocked.Test.", 443, &[]));
        assert!(!policy.allows("sub.BLOCKED.test", 443, &[]));
    }

    #[test]
    fn pattern_case_is_normalised() {
        assert!(!rules(&[], &["Blocked.TEST."]).allows("blocked.test", 443, &[]));
    }

    #[test]
    fn allows_normalizes_subdomain_and_wildcard_pattern_case() {
        assert!(!rules(&[], &["Blocked.TEST"]).allows("sub.blocked.test.", 443, &[]));
        assert!(!rules(&[], &["*.Example.COM."]).allows("a.example.com", 443, &[]));
        assert!(!rules(&[], &["*"]).allows("any.", 443, &[]));
    }

    #[test]
    fn wildcard_and_universal_case_insensitive() {
        assert!(!rules(&[], &["*.Example.COM"]).allows("a.example.com.", 443, &[]));
        assert!(name_matches("A.Example.COM.", "*.example.com"));
        assert!(!rules(&[], &["*"]).allows("ANY.", 443, &[]));
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
            assert!(HostPattern::parse(pattern).is_err(), "pattern {pattern:?}");
            assert!(!name_matches("a.b", pattern), "pattern {pattern:?}");
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
        assert!(p.allows("example.com.", 443, &[]));
        assert!(p.allows("sub.example.com.", 443, &[]));
    }

    #[test]
    fn wildcard_does_not_match_unrelated_suffix() {
        let p = rules(&["*.example.com"], &[]);
        assert!(p.allows("a.example.com", 443, &[]));
        assert!(!p.allows("example.com", 443, &[]));
        assert!(!p.allows("a.other.com", 443, &[]));
    }

    // --- WS04-P1-01: the extended grammar ---------------------------------

    #[test]
    fn exact_only() {
        let exact = pattern("=api.example.com");
        assert_eq!(exact.kind, PatternKind::Exact);
        assert_eq!(exact.name, "api.example.com");
        assert!(exact.matches("api.example.com", 443, &[]));
        assert!(exact.matches("API.Example.COM.", 443, &[]));
        assert!(!exact.matches("v2.api.example.com", 443, &[]));
        assert!(!exact.matches("example.com", 443, &[]));
        assert_eq!(exact.to_string(), "=api.example.com");
        // An exact entry cannot smuggle a wildcard.
        assert_eq!(
            HostPattern::parse("=*.example.com"),
            Err(PatternError::StraySyntax)
        );
    }

    #[test]
    fn idna_to_ascii() {
        let rule = pattern("Bücher.DE.");
        assert_eq!(rule.kind, PatternKind::Subtree);
        assert_eq!(rule.name, "xn--bcher-kva.de");
        assert_eq!(rule.to_string(), "xn--bcher-kva.de");
        // The unicode and the A-label form are one pattern: either spelling
        // matches either spelling of the request.
        assert!(rule.matches("bücher.de", 443, &[]));
        assert!(rule.matches("xn--bcher-kva.de", 443, &[]));
        assert!(rule.matches("WWW.XN--BCHER-KVA.DE", 443, &[]));
        assert!(!rule.matches("bucher.de", 443, &[]));

        let star = pattern("*.Bücher.de");
        assert_eq!(star.kind, PatternKind::Subdomains);
        assert_eq!(star.name, "xn--bcher-kva.de");
        assert!(star.matches("a.bücher.de", 443, &[]));
        assert!(!star.matches("bücher.de", 443, &[]));
    }

    #[test]
    fn idna_expansion_is_length_checked() {
        // Each `ä` label becomes `xn--4ca`, so a 252-byte entry lands past the
        // 253-byte limit once it is canonicalised.
        let raw = "ä.".repeat(84);
        assert_eq!(raw.len(), 252);
        assert_eq!(HostPattern::parse(&raw), Err(PatternError::TooLong));
    }

    #[test]
    fn inet_aton_forms_are_ip_literals() {
        // `0x7f.1` and friends are IPv4 to `url`, so they must be refused like
        // the dotted-quad spelling — otherwise an address could slip in as a
        // name rule.
        for raw in [
            "127.0.0.1",
            "127.1",
            "0x7f.1",
            "0x7f000001",
            "0177.0.0.1",
            "2852039166",
            "2130706433",
            "192.168.0.1:443",
            "[::1]",
            "[2001:db8::1]:443",
        ] {
            assert_eq!(
                HostPattern::parse(raw),
                Err(PatternError::IpLiteral),
                "{raw}"
            );
        }
    }

    #[test]
    fn trailing_dot() {
        let rule = pattern("API.Example.COM.");
        assert_eq!(rule.to_string(), "api.example.com");
        assert!(rule.matches("api.example.com.", 443, &[]));
        assert!(rule.matches("API.EXAMPLE.COM", 443, &[]));
        // Only one trailing dot is ignored; two are malformed.
        assert_eq!(
            HostPattern::parse("api.example.com.."),
            Err(PatternError::EmptyLabel)
        );
    }

    #[test]
    fn port_ranges() {
        let range = pattern("api.example.com:8000-8100");
        assert_eq!(range.ports, PortSpec::Range(8000, 8100));
        assert_eq!(range.to_string(), "api.example.com:8000-8100");
        for port in [8000u16, 8050, 8100] {
            assert!(range.matches("sub.api.example.com", port, &[]), "{port}");
        }
        for port in [7999u16, 8101, 443] {
            assert!(!range.matches("sub.api.example.com", port, &[]), "{port}");
        }
        // A suffix replaces the policy's ports, it does not intersect with them.
        assert!(range.matches("api.example.com", 8050, &[443]));

        let one = pattern("api.example.com:443");
        assert_eq!(one.ports, PortSpec::One(443));
        assert_eq!(one.to_string(), "api.example.com:443");
        assert!(one.matches("api.example.com", 443, &[80]));
        assert!(!one.matches("api.example.com", 80, &[80]));

        // No suffix defers to the policy: an empty list means any port.
        let default = pattern("api.example.com");
        assert_eq!(default.ports, PortSpec::Default);
        assert!(default.matches("api.example.com", 80, &[]));
        assert!(default.matches("api.example.com", 443, &[443]));
        assert!(!default.matches("api.example.com", 80, &[443]));
    }

    #[test]
    fn port_suffix_any() {
        let any = pattern("api.example.com:*");
        assert_eq!(any.ports, PortSpec::Any);
        assert_eq!(any.to_string(), "api.example.com:*");
        for port in [1u16, 443, 65535] {
            assert!(any.matches("api.example.com", port, &[80]), "{port}");
        }
        // `*` stands alone, so it takes no port suffix.
        assert_eq!(pattern("*").ports, PortSpec::Default);
        assert_eq!(HostPattern::parse("*:443"), Err(PatternError::AnyWithPort));
        assert_eq!(HostPattern::parse("*:*"), Err(PatternError::AnyWithPort));
    }

    #[test]
    fn name_length_and_labels() {
        // 253 bytes is the DNS limit; one more is refused.
        let longest = "a.".repeat(127);
        assert_eq!(pattern(&longest).name.len(), 253);
        assert_eq!(
            HostPattern::parse(&"a.".repeat(128)),
            Err(PatternError::TooLong)
        );
        assert_eq!(HostPattern::parse("a..b"), Err(PatternError::EmptyLabel));
        // Service labels keep their underscore; a registrable one does not.
        assert_eq!(pattern("_dmarc.example.com").name, "_dmarc.example.com");
        assert_eq!(
            HostPattern::parse("_example.com"),
            Err(PatternError::UnderscoreInRegistrable)
        );
        assert_eq!(
            HostPattern::parse("a=b.com"),
            Err(PatternError::StraySyntax)
        );
    }

    #[test]
    fn covers_is_monotone() {
        // Names.
        assert!(pattern("example.com").covers(&pattern("example.com")));
        assert!(pattern("example.com").covers(&pattern("a.example.com")));
        assert!(pattern("example.com").covers(&pattern("a.b.example.com")));
        assert!(pattern("example.com").covers(&pattern("*.example.com")));
        assert!(pattern("example.com").covers(&pattern("*.a.example.com")));
        assert!(pattern("example.com").covers(&pattern("=example.com")));
        assert!(pattern("example.com").covers(&pattern("=a.example.com")));
        assert!(!pattern("example.com").covers(&pattern("otherexample.com")));
        assert!(!pattern("example.com").covers(&pattern("com")));
        assert!(pattern("*.example.com").covers(&pattern("a.example.com")));
        assert!(pattern("*.example.com").covers(&pattern("*.a.example.com")));
        assert!(!pattern("*.example.com").covers(&pattern("example.com")));
        assert!(!pattern("*.example.com").covers(&pattern("*.com")));
        // `*` is the whole space.
        assert!(pattern("*").covers(&pattern("*")));
        assert!(pattern("*").covers(&pattern("anything.test")));
        assert!(!pattern("anything.test").covers(&pattern("*")));
        // `=x` is the narrowest name rule.
        assert!(pattern("=example.com").covers(&pattern("=example.com")));
        assert!(!pattern("=example.com").covers(&pattern("example.com")));
        assert!(!pattern("=example.com").covers(&pattern("=a.example.com")));

        // Ports.
        assert!(pattern("example.com:*").covers(&pattern("a.example.com")));
        assert!(pattern("example.com:*").covers(&pattern("a.example.com:443")));
        assert!(!pattern("example.com").covers(&pattern("a.example.com:*")));
        assert!(!pattern("example.com").covers(&pattern("a.example.com:443")));
        assert!(!pattern("example.com:443").covers(&pattern("a.example.com")));
        assert!(pattern("example.com:443").covers(&pattern("a.example.com:443")));
        assert!(!pattern("example.com:443").covers(&pattern("a.example.com:8443")));
        assert!(pattern("example.com:443").covers(&pattern("a.example.com:443-443")));
        assert!(!pattern("example.com:443").covers(&pattern("a.example.com:443-444")));
        assert!(pattern("example.com:8000-8100").covers(&pattern("a.example.com:8000")));
        assert!(!pattern("example.com:8000-8100").covers(&pattern("a.example.com:8101")));
        assert!(pattern("example.com:8000-8100").covers(&pattern("a.example.com:8000-8100")));
        assert!(!pattern("example.com:8000-8100").covers(&pattern("a.example.com:8000-8101")));
    }

    #[test]
    fn covers_implies_matches() {
        let patterns: Vec<HostPattern> = [
            "*",
            "example.com",
            "*.example.com",
            "=example.com",
            "a.example.com",
            "*.a.example.com",
            "=a.example.com",
            "example.com:*",
            "example.com:443",
            "example.com:8000-8100",
            "example.com:443-443",
            "*.example.com:443",
            "=example.com:443",
            "other.test",
        ]
        .into_iter()
        .map(pattern)
        .collect();
        let hosts = [
            "example.com",
            "a.example.com",
            "b.a.example.com",
            "aexample.com",
            "Example.COM.",
            "other.test",
            "",
            "a..b",
        ];
        let ports = [1u16, 80, 443, 8000, 8100, 65535];
        // An empty `ports` list means "any port".
        let defaults: [&[u16]; 3] = [&[], &[443], &[80, 443]];

        let mut covering = 0usize;
        for ceiling in &patterns {
            for entry in &patterns {
                if !ceiling.covers(entry) {
                    continue;
                }
                covering += 1;
                for host in hosts {
                    for port in ports {
                        for allowed in defaults {
                            if entry.matches(host, port, allowed) {
                                assert!(
                                    ceiling.matches(host, port, allowed),
                                    "{} covers {} but {host}:{port} with {allowed:?} escapes",
                                    ceiling,
                                    entry
                                );
                            }
                        }
                    }
                }
            }
        }
        // The corpus must actually exercise the implication.
        assert!(covering > 20, "only {covering} covering pairs");
    }

    #[test]
    fn exact_default_for_credentials() {
        let host = HostPattern::parse_exact_default("api.example.com").expect("pattern");
        assert_eq!(host.kind, PatternKind::Exact);
        assert!(host.matches("api.example.com", 443, &[]));
        assert!(!host.matches("v2.api.example.com", 443, &[]));
        // The explicit forms keep their meaning.
        assert_eq!(
            HostPattern::parse_exact_default("*.example.com")
                .expect("pattern")
                .kind,
            PatternKind::Subdomains
        );
        assert_eq!(
            HostPattern::parse_exact_default("example.com")
                .expect("pattern")
                .kind,
            PatternKind::Exact
        );
    }

    #[test]
    fn canonical_round_trip() {
        for raw in [
            "*",
            "x.com",
            "*.x.com",
            "=x.com",
            "x.com:443",
            "x.com:8000-8100",
            "x.com:*",
            "=X.COM.:443",
            "*.Bücher.de",
        ] {
            let parsed = pattern(raw);
            let rendered = parsed.to_string();
            assert_eq!(pattern(&rendered), parsed, "{raw} -> {rendered}");
            assert_eq!(rendered.parse::<HostPattern>(), Ok(parsed), "{raw}");
        }
    }

    #[test]
    fn serialises_as_canonical_string() {
        let rule = pattern("*.Bücher.de:8000-8100");
        let json = serde_json::to_string(&rule).expect("serialise");
        assert_eq!(json, "\"*.xn--bcher-kva.de:8000-8100\"");
        let back: HostPattern = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(back, rule);
        // A malformed entry is a deserialisation error, not a silent pass-through.
        assert!(serde_json::from_str::<HostPattern>("\"a..b\"").is_err());
    }

    #[test]
    fn malformed_query_never_matches_a_name_rule() {
        let rule = pattern("example.com");
        for host in ["", "a..b", "a b", "a/b.com", "example.com.."] {
            assert!(!rule.matches(host, 443, &[]), "{host:?}");
        }
        // An address-shaped request is no name rule's business.
        assert!(!rule.matches("127.0.0.1", 443, &[]));
        assert!(!rule.matches("[::1]", 443, &[]));
        // `*` still matches a malformed name (rule 1).
        let any = pattern("*");
        assert!(any.matches("", 443, &[]));
        assert!(any.matches("a..b", 443, &[]));
        assert!(any.matches("[::1]", 443, &[]));
    }

    #[test]
    fn pattern_errors_have_stable_messages() {
        let cases = [
            ("", PatternError::Empty),
            ("a b", PatternError::Whitespace),
            ("a..b", PatternError::EmptyLabel),
            ("_example.com", PatternError::UnderscoreInRegistrable),
            ("127.0.0.1", PatternError::IpLiteral),
            ("x.com:0", PatternError::InvalidPort),
            ("x.com:8100-8000", PatternError::InvalidPort),
            ("*:443", PatternError::AnyWithPort),
            ("a.*.b.com", PatternError::StraySyntax),
            ("a/b.com", PatternError::MalformedName),
        ];
        for (raw, expected) in cases {
            assert_eq!(HostPattern::parse(raw), Err(expected), "{raw:?}");
        }
        assert_eq!(
            HostPattern::parse(&"a.".repeat(128)),
            Err(PatternError::TooLong)
        );
        // One message per variant, independent of the input: a config layer
        // attaches the key path to it.
        let messages: std::collections::HashSet<String> = [
            PatternError::Empty,
            PatternError::Whitespace,
            PatternError::EmptyLabel,
            PatternError::TooLong,
            PatternError::UnderscoreInRegistrable,
            PatternError::IpLiteral,
            PatternError::InvalidPort,
            PatternError::AnyWithPort,
            PatternError::StraySyntax,
            PatternError::MalformedName,
        ]
        .into_iter()
        .map(|err| err.to_string())
        .collect();
        assert_eq!(messages.len(), 10);
        assert_eq!(
            PatternError::IpLiteral.to_string(),
            "host pattern is an IP address; addresses belong in `allow_ip`"
        );
    }
}
