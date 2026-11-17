//! Config admission for `provider = "vercel"` envs. Errors carry a field label
//! and a fixed reason only, never a configured value.

use std::fmt;

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
