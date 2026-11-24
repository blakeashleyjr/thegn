//! The egress policy's data shape (resolutions B11, B15): what one lease may
//! reach, as pure serde data.
//!
//! **Data only.** The evaluator (`EgressPolicy::decide`), the precedence order,
//! `digest()` and `EgressPolicyView` land in WS04-P1-03; presets and
//! `resolve_egress` in WS04-P1-05. This module freezes the field names and their
//! JSON spellings first, so the proxy, the daemon client, the manifest rows and
//! the Seatbelt/MXC datapaths can be written against one shape before any of
//! them exists (WS04-P1-07). Changing a name or a spelling later costs a
//! [`PROTO_VERSION`](crate::egress::wire::PROTO_VERSION) bump.
//!
//! **Ownership of the config enums.** WS04-P1-04 owns the config surface and
//! creates `crates/thegn-core/src/config_egress.rs`, which is where
//! [`EgressApprovals`], [`EgressAuditLevel`], [`EgressGuardClass`], [`IpRule`]
//! and [`LocalRule`] belong. Until that item lands they are defined here,
//! because the wire needs their spellings now; when `config_egress.rs` arrives
//! these definitions move and this module re-exports the moved types, so no
//! caller of `egress::policy` or of the wire changes.
//!
//! [`HostPattern`] serialises as its canonical config string: `*` for the
//! machine-wide pattern, `*.example.com` for strict subdomains, `example.com`
//! for the apex and every subdomain. A malformed pattern is a deserialisation
//! error, never a silent widening: a malformed allow entry can never match.
//!
//! Nothing here carries behaviour: no I/O, no threads, no clock.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::egress::pattern::{HostPattern, HostPatternKind};

/// How an unmatched connection is handled; the `[sandbox.egress] approvals`
/// key.
///
/// `Off` is the *stricter* value (an unmatched connection is denied instead of
/// asked), so a policy that omits the field can never ask a human by accident.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EgressApprovals {
    /// Deny an unmatched connection without asking (`off`).
    #[default]
    Off,
    /// Ask the user, holding the connection without resolving DNS (`prompt`).
    Prompt,
}

/// How much of the proxy's traffic is audited; the `[sandbox.egress] audit` key.
///
/// The order is the clamp order (`off < denied < decisions`): a repo overlay may
/// only raise it (WS04-P1-06), so `Off` is the permissive-by-silence default and
/// the serde default for an absent field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EgressAuditLevel {
    /// Write no audit records (`off`).
    #[default]
    Off,
    /// One record per denial (`denied`).
    Denied,
    /// One record per decision: denials and allowed connections (`decisions`).
    Decisions,
}

/// An address class the destination guard waives for a lease; the
/// `[sandbox.egress] guard_allow` key.
///
/// There is deliberately no `metadata` or `link_local` variant: cloud metadata
/// addresses and link-local addresses are never waivable (B16, WS04-P1-02).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EgressGuardClass {
    /// Loopback addresses (`loopback`).
    Loopback,
    /// RFC 1918 and ULA private ranges (`private`).
    Private,
    /// The carrier-grade NAT range 100.64.0.0/10 (`cgnat`).
    Cgnat,
    /// The host's own interface addresses (`host`).
    Host,
}

/// One `allow_ip` entry: a public address or prefix and, optionally, the one
/// port it is allowed on.
///
/// WS04-P1-04 owns the parser and moves this type to `config_egress.rs`; here it
/// is the wire shape only, so the address stays the canonical config text
/// (`198.51.100.7`, `203.0.113.0/24`) rather than a parsed prefix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpRule {
    /// The canonical address or CIDR prefix (`203.0.113.0/24`, `198.51.100.7`).
    pub cidr: String,
    /// The one port the rule pins; `None` means the policy's [`EgressPolicy::ports`].
    pub port: Option<u16>,
}

/// One `local` entry: an exact guard waiver for one loopback destination.
///
/// A waiver is exact on purpose: `127.0.0.1:5432` waives that address and that
/// port only, never a range and never a host name that resolves elsewhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalRule {
    /// The exact host: an IP literal (`127.0.0.1`, `::1`) or the loopback alias
    /// `host.thegn.internal`.
    pub host: String,
    /// The exact port the waiver applies to.
    pub port: u16,
}

/// The resolved policy one lease enforces (B15): the allow/deny host patterns,
/// the IP and local waivers, the guard classes, the default ports and whether a
/// TLS connection must present an SNI that matches its CONNECT host.
///
/// Ordering inside `allow`/`deny` carries no meaning; the evaluator's precedence
/// (WS04-P1-03) is deny-first, and a deny rule always wins.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EgressPolicy {
    /// Host patterns whose match allows the connection.
    pub allow: Vec<HostPattern>,
    /// Host patterns whose match denies the connection; deny always wins.
    pub deny: Vec<HostPattern>,
    /// Public address or prefix rules, each optionally pinned to one port.
    pub allow_ip: Vec<IpRule>,
    /// Exact loopback guard waivers.
    pub local: Vec<LocalRule>,
    /// Address classes the destination guard waives.
    pub guard_allow: Vec<EgressGuardClass>,
    /// The ports a pattern without a port suffix applies to (443, 80).
    pub ports: Vec<u16>,
    /// The ports on which a TLS handshake is required.
    pub tls_ports: Vec<u16>,
    /// Require the SNI to equal the CONNECT host on a TLS port.
    pub require_sni: bool,
    /// Whether an unmatched host is denied or asks the user.
    pub approvals: EgressApprovals,
}

/// The canonical config string of a host pattern: `*`, `*.example.com` or
/// `example.com`.
fn canonical(pattern: &HostPattern) -> String {
    match pattern.kind {
        HostPatternKind::Any => "*".to_string(),
        HostPatternKind::Subdomains => format!("*.{}", pattern.name),
        HostPatternKind::Subtree => pattern.name.clone(),
    }
}

// `HostPattern` lives in `egress::pattern` (WS01-P2-01, extended by WS04-P1-01);
// its serde form is defined here so this item touches no other file. When
// WS04-P1-01 lands the canonical-string impls move next to the type unchanged.

impl Serialize for HostPattern {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&canonical(self))
    }
}

impl<'de> Deserialize<'de> for HostPattern {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        match HostPattern::parse(&raw) {
            Some(pattern) => Ok(pattern),
            None => Err(serde::de::Error::custom(format!(
                "malformed host pattern {raw:?}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> EgressPolicy {
        EgressPolicy {
            allow: vec![
                HostPattern::parse("github.com").expect("valid pattern"),
                HostPattern::parse("*.githubusercontent.com").expect("valid pattern"),
            ],
            deny: vec![HostPattern::parse("*.evil.example").expect("valid pattern")],
            allow_ip: vec![IpRule {
                cidr: "203.0.113.0/24".into(),
                port: Some(443),
            }],
            local: vec![LocalRule {
                host: "127.0.0.1".into(),
                port: 5432,
            }],
            guard_allow: vec![EgressGuardClass::Loopback, EgressGuardClass::Cgnat],
            ports: vec![443, 80],
            tls_ports: vec![443],
            require_sni: true,
            approvals: EgressApprovals::Prompt,
        }
    }

    #[test]
    fn host_pattern_is_its_canonical_string() {
        for (raw, wire) in [
            ("Example.COM.", "example.com"),
            ("*.Example.com.", "*.example.com"),
            ("*", "*"),
        ] {
            let pattern = HostPattern::parse(raw).expect("valid pattern");
            let json = serde_json::to_string(&pattern).expect("pattern serialises");
            assert_eq!(json, format!("\"{wire}\""));
            let back: HostPattern = serde_json::from_str(&json).expect("pattern parses");
            assert_eq!(back, pattern);
        }
    }

    #[test]
    fn malformed_host_pattern_is_a_wire_error() {
        for raw in ["\"a..b\"", "\"a b\"", "\"\"", "\".\""] {
            assert!(
                serde_json::from_str::<HostPattern>(raw).is_err(),
                "{raw} parsed as a pattern"
            );
        }
    }

    #[test]
    fn policy_roundtrips() {
        let policy = policy();
        let json = serde_json::to_string(&policy).expect("policy serialises");
        assert_eq!(
            serde_json::from_str::<EgressPolicy>(&json).expect("policy parses"),
            policy
        );
        let value: serde_json::Value = serde_json::from_str(&json).expect("json value");
        assert_eq!(value["allow"][0], "github.com");
        assert_eq!(value["allow"][1], "*.githubusercontent.com");
        assert_eq!(value["deny"][0], "*.evil.example");
        assert_eq!(value["guard_allow"][1], "cgnat");
        assert_eq!(value["approvals"], "prompt");
        assert_eq!(value["ports"], serde_json::json!([443, 80]));
    }

    #[test]
    fn default_policy_allows_nothing() {
        let policy = EgressPolicy::default();
        assert!(policy.allow.is_empty());
        assert!(policy.deny.is_empty());
        assert!(policy.allow_ip.is_empty());
        assert!(policy.local.is_empty());
        assert!(policy.guard_allow.is_empty());
        assert_eq!(policy.approvals, EgressApprovals::Off);
        assert!(!policy.require_sni);
    }

    #[test]
    fn config_enum_spellings() {
        let approvals = [
            (EgressApprovals::Off, "\"off\""),
            (EgressApprovals::Prompt, "\"prompt\""),
        ];
        for (value, wire) in approvals {
            assert_eq!(serde_json::to_string(&value).expect("serialises"), wire);
            assert_eq!(
                serde_json::from_str::<EgressApprovals>(wire).expect("parses"),
                value
            );
        }
        assert_eq!(EgressApprovals::default(), EgressApprovals::Off);

        let levels = [
            (EgressAuditLevel::Off, "\"off\""),
            (EgressAuditLevel::Denied, "\"denied\""),
            (EgressAuditLevel::Decisions, "\"decisions\""),
        ];
        for (value, wire) in levels {
            assert_eq!(serde_json::to_string(&value).expect("serialises"), wire);
            assert_eq!(
                serde_json::from_str::<EgressAuditLevel>(wire).expect("parses"),
                value
            );
        }
        assert_eq!(EgressAuditLevel::default(), EgressAuditLevel::Off);

        let classes = [
            (EgressGuardClass::Loopback, "\"loopback\""),
            (EgressGuardClass::Private, "\"private\""),
            (EgressGuardClass::Cgnat, "\"cgnat\""),
            (EgressGuardClass::Host, "\"host\""),
        ];
        for (value, wire) in classes {
            assert_eq!(serde_json::to_string(&value).expect("serialises"), wire);
            assert_eq!(
                serde_json::from_str::<EgressGuardClass>(wire).expect("parses"),
                value
            );
        }
        // Metadata and link-local are never waivable, so they have no spelling.
        for refused in ["\"metadata\"", "\"link_local\"", "\"any\""] {
            assert!(serde_json::from_str::<EgressGuardClass>(refused).is_err());
        }
    }

    #[test]
    fn ip_and_local_rules_roundtrip() {
        let rules = (
            IpRule {
                cidr: "198.51.100.7".into(),
                port: None,
            },
            LocalRule {
                host: "::1".into(),
                port: 8080,
            },
        );
        let json = serde_json::to_string(&rules).expect("rules serialise");
        assert_eq!(
            json,
            r#"[{"cidr":"198.51.100.7","port":null},{"host":"::1","port":8080}]"#
        );
        let back: (IpRule, LocalRule) = serde_json::from_str(&json).expect("rules parse");
        assert_eq!(back, rules);
    }

    #[test]
    fn unknown_policy_fields_ignored() {
        let json = serde_json::json!({
            "allow": ["github.com"],
            "deny": [],
            "allow_ip": [],
            "local": [],
            "guard_allow": [],
            "ports": [443],
            "tls_ports": [443],
            "require_sni": true,
            "approvals": "off",
            "future_key": { "from": "v1.1" }
        });
        let policy: EgressPolicy = serde_json::from_value(json).expect("unknown field ignored");
        assert_eq!(policy.ports, vec![443]);

        let ip: IpRule = serde_json::from_value(serde_json::json!({
            "cidr": "203.0.113.0/24",
            "port": null,
            "comment": "public docs range"
        }))
        .expect("unknown ip field ignored");
        assert_eq!(ip.port, None);
    }
}
