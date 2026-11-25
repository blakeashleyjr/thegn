//! Vercel sandbox egress policy: deny by default, an opt-in registry preset,
//! or an explicit allow-all. Pure; the host applies the result on create and
//! on resume (see `thegn_svc::vercel`).

use crate::config::SandboxEgress;
use crate::egress::pattern::{HostPattern, PatternKind, PortSpec};
use crate::vercel::admit::AdmitError;

/// Hosts a build needs: package registries and git hosting. Exact names only,
/// so the preset never widens to a whole domain. Used by
/// [`SandboxEgress::Registries`].
pub const REGISTRY_PRESET: &[&str] = &[
    "crates.io",
    "static.crates.io",
    "index.crates.io",
    "registry.npmjs.org",
    "pypi.org",
    "files.pythonhosted.org",
    "github.com",
    "codeload.github.com",
    "objects.githubusercontent.com",
];

fn err(reason: &'static str) -> AdmitError {
    AdmitError {
        field: "egress_allow",
        reason,
    }
}

/// Expand one `egress_allow` entry (the host-pattern grammar) into the names
/// Vercel's allow list takes: a bare `x.com` is the apex and every subdomain,
/// so it becomes `x.com` and `*.x.com`. A catch-all `*` and port suffixes are
/// refused (allow-all is the explicit `egress = "allow-all"` choice).
fn expand_entry(raw: &str) -> Result<Vec<String>, AdmitError> {
    let pattern = HostPattern::parse(raw).map_err(|_| err("each entry must be a host pattern"))?;
    if pattern.kind == PatternKind::Any {
        return Err(err(
            "'*' is not accepted; set egress = \"allow-all\" to open egress",
        ));
    }
    if pattern.ports != PortSpec::Default {
        return Err(err("port suffixes are not supported on vercel"));
    }
    let name = pattern.name.as_str();
    Ok(match pattern.kind {
        PatternKind::Subtree => vec![name.to_string(), format!("*.{name}")],
        PatternKind::Subdomains => vec![format!("*.{name}")],
        PatternKind::Exact | PatternKind::Any => vec![name.to_string()],
    })
}

/// Check every `egress_allow` entry.
pub fn validate_allow(allow: &[String]) -> Result<(), AdmitError> {
    allow.iter().try_for_each(|e| expand_entry(e).map(|_| ()))
}

/// The allowed-domain list to send as Vercel's network policy for `mode` plus
/// `allow`: empty ⇒ deny-all, `["*"]` ⇒ allow-all, else a sorted custom list.
/// `allow-all` ignores `allow`.
pub fn resolve(mode: SandboxEgress, allow: &[String]) -> Result<Vec<String>, AdmitError> {
    if mode == SandboxEgress::AllowAll {
        return Ok(vec!["*".to_string()]);
    }
    let mut out: Vec<String> = if mode == SandboxEgress::Registries {
        REGISTRY_PRESET.iter().map(|h| (*h).to_string()).collect()
    } else {
        Vec::new()
    };
    for entry in allow {
        out.extend(expand_entry(entry)?);
    }
    out.sort();
    out.dedup();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_string()).collect()
    }

    #[test]
    fn default_resolves_to_deny_all() {
        let domains = resolve(SandboxEgress::default(), &[]).unwrap();
        assert!(domains.is_empty());
        assert_eq!(
            crate::vercel::api::network_policy_body(&domains),
            serde_json::json!({"mode": "deny-all"})
        );
    }

    #[test]
    fn registries_expands_the_preset() {
        let d = resolve(SandboxEgress::Registries, &[]).unwrap();
        for host in [
            "crates.io",
            "static.crates.io",
            "registry.npmjs.org",
            "pypi.org",
            "files.pythonhosted.org",
            "github.com",
            "codeload.github.com",
            "objects.githubusercontent.com",
        ] {
            assert!(d.iter().any(|h| h == host), "{host}");
        }
        assert!(!d.iter().any(|h| h == "*"));
    }

    #[test]
    fn allow_all_is_a_single_star_and_ignores_the_list() {
        assert_eq!(
            resolve(SandboxEgress::AllowAll, &s(&["a.com"])).unwrap(),
            s(&["*"])
        );
    }

    #[test]
    fn extra_entries_expand_by_the_pattern_grammar() {
        let d = resolve(SandboxEgress::Deny, &s(&["x.com", "*.y.com", "=z.com"])).unwrap();
        assert_eq!(d, s(&["*.x.com", "*.y.com", "x.com", "z.com"]));
        let both = resolve(SandboxEgress::Registries, &s(&["github.com"])).unwrap();
        assert_eq!(both.iter().filter(|h| *h == "github.com").count(), 1);
    }

    #[test]
    fn bad_entries_are_refused() {
        for bad in ["*", "a.com:443", "", "a b"] {
            assert!(validate_allow(&s(&[bad])).is_err(), "{bad}");
        }
        assert!(validate_allow(&s(&["a.com", "*.b.org"])).is_ok());
    }
}
