//! Config admission for `provider = "vercel"` envs. Errors carry a field label
//! and a fixed reason only, never a configured value.

use std::fmt;

use crate::config::ModelCredentials;

/// A refused `[env.<name>.provider]` field: label + fixed reason, no value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmitError {
    pub field: &'static str,
    pub reason: &'static str,
}

impl fmt::Display for AdmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.field, self.reason)
    }
}

impl std::error::Error for AdmitError {}

const MAX_TEMPLATE_BYTES: usize = 256;

/// The refusal for `model_credentials = "proxy"` (shared with `creds`).
pub const PROXY_REFUSAL: &str = "proxy is not implemented: it needs a public HTTPS endpoint, \
     Vercel OIDC verification, per-sandbox scoping and revocation (THE-2233)";

/// The refusal for `model_credentials = "inject"` (shared with `creds`).
pub const INJECT_REFUSAL: &str =
    "inject would hand the model key to Vercel; not offered (THE-2233)";

/// The public-suffix-style domain any Vercel user can deploy to; never a
/// trusted interactive host.
const UNTRUSTED_SUFFIX: &str = "vercel.app";

const MAX_DNS_NAME_BYTES: usize = 253;

/// A lowercase DNS name with at least one dot: labels of `[a-z0-9-]`, 1..=63
/// bytes, no leading/trailing hyphen, no empty label (so no leading, trailing
/// or doubled dot), no wildcard, at most 253 bytes, and the last label starts
/// with a letter (so no IP literal).
pub fn valid_host_suffix(s: &str) -> bool {
    s.len() <= MAX_DNS_NAME_BYTES
        && s.contains('.')
        && s.split('.').all(|label| {
            let b = label.as_bytes();
            !b.is_empty()
                && b.len() <= 63
                && b[0] != b'-'
                && b[b.len() - 1] != b'-'
                && b.iter()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        })
        && s.rsplit('.')
            .next()
            .is_some_and(|tld| tld.as_bytes().first().is_some_and(u8::is_ascii_lowercase))
}

/// `s` (lowercase, no trailing dot) is `vercel.app` or a name under it.
pub(crate) fn under_untrusted_suffix(s: &str) -> bool {
    s == UNTRUSTED_SUFFIX
        || s.strip_suffix(UNTRUSTED_SUFFIX)
            .is_some_and(|rest| rest.ends_with('.'))
}

fn err(field: &'static str, reason: &'static str) -> AdmitError {
    AdmitError { field, reason }
}

fn prefixed_alnum(s: &str, prefix: &str) -> bool {
    s.strip_prefix(prefix)
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// `^[a-z]{3}[0-9]$` (e.g. `iad1`).
fn valid_region(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 4 && b[..3].iter().all(u8::is_ascii_lowercase) && b[3].is_ascii_digit()
}

/// Validate a vercel env's provider table (team/project ids, size, region,
/// template, unsupported keys). Labels only; never echoes a value.
pub fn validate_env(pc: &crate::config::EnvProviderConfig) -> Result<(), AdmitError> {
    if !valid_team_id(&pc.team_id) {
        return Err(err("team_id", "must look like team_<alphanumeric>"));
    }
    if !valid_project_id(&pc.project_id) {
        return Err(err("project_id", "must look like prj_<alphanumeric>"));
    }
    crate::vercel::policy::vcpus_for(&pc.size, pc.min_vcpu)?;
    if !pc.region.is_empty() && !valid_region(&pc.region) {
        return Err(err("region", "must look like iad1"));
    }
    if pc.template.len() > MAX_TEMPLATE_BYTES
        || pc
            .template
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(err("template", "must be 1 token of at most 256 bytes"));
    }
    if pc.host_cache {
        return Err(err("host_cache", "unsupported on vercel"));
    }
    if pc.connect == crate::config::ProviderConnect::Ssh {
        return Err(err("connect", "ssh is unsupported on vercel"));
    }
    if pc.binary_cache_push {
        return Err(err("binary_cache_push", "unsupported on vercel"));
    }
    match pc.model_credentials {
        ModelCredentials::None | ModelCredentials::Upload => {}
        ModelCredentials::Proxy => return Err(err("model_credentials", PROXY_REFUSAL)),
        ModelCredentials::Inject => return Err(err("model_credentials", INJECT_REFUSAL)),
    }
    for suffix in &pc.interactive_host_suffixes {
        if !valid_host_suffix(suffix) {
            return Err(err(
                "interactive_host_suffixes",
                "each entry must be a lowercase DNS name with a dot, no leading dot, \
                 no wildcard, no IP address, at most 253 bytes",
            ));
        }
        if under_untrusted_suffix(suffix) {
            return Err(err(
                "interactive_host_suffixes",
                "vercel.app is deployable by any Vercel user and is never trusted",
            ));
        }
    }
    crate::vercel::egress::validate_allow(&pc.egress_allow)?;
    Ok(())
}

/// `^[a-z0-9][a-z0-9-]{0,62}$` (conservative; Vercel's charset is UNVERIFIED).
pub fn valid_sandbox_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// `team_[A-Za-z0-9]+`.
pub fn valid_team_id(s: &str) -> bool {
    prefixed_alnum(s, "team_")
}

/// `prj_[A-Za-z0-9]+`.
pub fn valid_project_id(s: &str) -> bool {
    prefixed_alnum(s, "prj_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EnvProviderConfig;

    fn ok() -> EnvProviderConfig {
        EnvProviderConfig {
            team_id: "team_abc1".into(),
            project_id: "prj_Xy9".into(),
            ..Default::default()
        }
    }

    fn field(pc: &EnvProviderConfig) -> &'static str {
        validate_env(pc).unwrap_err().field
    }

    #[test]
    fn valid_config_passes() {
        assert!(validate_env(&ok()).is_ok());
        let mut pc = ok();
        pc.size = "8".into();
        pc.region = "iad1".into();
        pc.template = "node24".into();
        assert!(validate_env(&pc).is_ok());
    }

    #[test]
    fn ids_are_checked() {
        let mut pc = ok();
        pc.team_id = "team_".into();
        assert_eq!(field(&pc), "team_id");
        pc.team_id = "".into();
        assert_eq!(field(&pc), "team_id");
        let mut pc = ok();
        pc.project_id = "prj_a-b".into();
        assert_eq!(field(&pc), "project_id");
        assert!(valid_team_id("team_A1"));
        assert!(!valid_team_id("prj_A1"));
        assert!(valid_project_id("prj_A1"));
        assert!(!valid_project_id("team_A1"));
    }

    #[test]
    fn size_region_template_checked() {
        let mut pc = ok();
        pc.size = "3".into();
        assert_eq!(field(&pc), "size");
        let mut pc = ok();
        pc.region = "IAD1".into();
        assert_eq!(field(&pc), "region");
        pc.region = "iad11".into();
        assert_eq!(field(&pc), "region");
        pc.region = "ia11".into();
        assert_eq!(field(&pc), "region");
        let mut pc = ok();
        pc.template = "a b".into();
        assert_eq!(field(&pc), "template");
        pc.template = "a\u{7}".into();
        assert_eq!(field(&pc), "template");
        pc.template = "x".repeat(257);
        assert_eq!(field(&pc), "template");
        pc.template = "x".repeat(256);
        assert!(validate_env(&pc).is_ok());
    }

    #[test]
    fn unsupported_keys_refused() {
        let mut pc = ok();
        pc.host_cache = true;
        assert_eq!(field(&pc), "host_cache");
        let mut pc = ok();
        pc.connect = crate::config::ProviderConnect::Ssh;
        assert_eq!(field(&pc), "connect");
        let mut pc = ok();
        pc.binary_cache_push = true;
        assert_eq!(field(&pc), "binary_cache_push");
    }

    #[test]
    fn error_display_has_no_value() {
        let mut pc = ok();
        pc.region = "SECRETVALUE".into();
        let e = validate_env(&pc).unwrap_err();
        assert!(!e.to_string().contains("SECRETVALUE"));
        assert!(e.to_string().starts_with("region: "));
    }

    #[test]
    fn model_credentials_modes() {
        let mode = |m| EnvProviderConfig {
            model_credentials: m,
            ..ok()
        };
        assert!(validate_env(&mode(ModelCredentials::None)).is_ok());
        assert!(validate_env(&mode(ModelCredentials::Upload)).is_ok());
        let e = validate_env(&mode(ModelCredentials::Proxy)).unwrap_err();
        assert_eq!(e.field, "model_credentials");
        assert_eq!(e.reason, PROXY_REFUSAL);
        assert!(e.reason.contains("THE-2233"));
        let e = validate_env(&mode(ModelCredentials::Inject)).unwrap_err();
        assert_eq!(e.field, "model_credentials");
        assert_eq!(e.reason, INJECT_REFUSAL);
    }

    #[test]
    fn interactive_host_suffixes_checked() {
        let with = |s: &[&str]| EnvProviderConfig {
            interactive_host_suffixes: s.iter().map(|x| (*x).to_string()).collect(),
            ..ok()
        };
        assert!(validate_env(&with(&[])).is_ok());
        assert!(validate_env(&with(&["vercel.run", "vercel.sh", "a-1.vercel.com"])).is_ok());
        for bad in [
            "vercel.app",
            "x.vercel.app",
            "*.vercel.run",
            ".vercel.run",
            "vercel.run.",
            "vercel..run",
            "localhost",
            "Vercel.run",
            "-a.vercel.run",
            "1.2.3.4",
            "127.0.0.1",
            "1.0x7f",
            "",
        ] {
            assert_eq!(
                field(&with(&["vercel.run", bad])),
                "interactive_host_suffixes",
                "{bad:?}"
            );
        }
        // Not under vercel.app, just sharing the tail text.
        assert!(validate_env(&with(&["notvercel.app"])).is_ok());
        let long = format!("{}com", "a.".repeat(126));
        assert!(long.len() > 253);
        assert_eq!(field(&with(&[&long])), "interactive_host_suffixes");
    }

    #[test]
    fn egress_allow_checked() {
        let with = |s: &[&str]| EnvProviderConfig {
            egress_allow: s.iter().map(|x| (*x).to_string()).collect(),
            ..ok()
        };
        assert!(validate_env(&with(&["a.com", "*.b.org"])).is_ok());
        for bad in ["*", "a.com:443", "a b"] {
            assert_eq!(field(&with(&[bad])), "egress_allow", "{bad:?}");
        }
    }

    #[test]
    fn host_suffix_error_has_no_value() {
        let pc = EnvProviderConfig {
            interactive_host_suffixes: vec!["SECRET.example".into()],
            ..ok()
        };
        assert!(
            !validate_env(&pc)
                .unwrap_err()
                .to_string()
                .contains("SECRET")
        );
    }

    #[test]
    fn sandbox_names() {
        assert!(valid_sandbox_name("a"));
        assert!(valid_sandbox_name("0-a-b"));
        assert!(valid_sandbox_name(&"a".repeat(63)));
        assert!(!valid_sandbox_name(&"a".repeat(64)));
        assert!(!valid_sandbox_name(""));
        assert!(!valid_sandbox_name("-a"));
        assert!(!valid_sandbox_name("A"));
        assert!(!valid_sandbox_name("a_b"));
        assert!(!valid_sandbox_name("a/b"));
    }
}
