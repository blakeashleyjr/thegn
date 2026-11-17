//! `[env.<name>.provider] model_credentials`: which agent login files the
//! `upload` mode copies into a sandbox's tmpfs, where they land, the narrowed
//! egress policy applied first, and the per-mode disclosure line.
//!
//! Spine stub (b156 lane 1): signatures are fixed; lane 2 fills the bodies.
//! Until then the stub fails closed: no agent has upload files, so `upload`
//! sends nothing.

use crate::config::ModelCredentials;

/// One login file `upload` copies: `host_rel` is the host path relative to the
/// agent home; `vm_name` is the file name inside `<login_dir>/<agent>/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoginFile {
    pub host_rel: &'static str,
    pub vm_name: &'static str,
}

/// The host login files for `agent`; `Some` only for claude/claude-code/codex.
pub fn login_files(_agent: &str) -> Option<&'static [LoginFile]> {
    None
}

/// Files synthesized in the sandbox rather than copied from the host.
pub fn synthesized_files(_agent: &str) -> Vec<(&'static str, Vec<u8>)> {
    Vec::new()
}

/// The tmpfs root the login dir lives under (fail closed if not a tmpfs).
pub const LOGIN_TMPFS_ROOT: &str = "/dev/shm";

/// `"/dev/shm/thegn-login-<16 lowercase hex>"`; any other nonce is refused.
pub fn login_dir(nonce_hex: &str) -> Result<String, crate::vercel::admit::AdmitError> {
    if nonce_hex.len() == 16
        && nonce_hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Ok(format!("{LOGIN_TMPFS_ROOT}/thegn-login-{nonce_hex}"))
    } else {
        Err(crate::vercel::admit::AdmitError {
            field: "login_dir",
            reason: "nonce must be 16 lowercase hex characters",
        })
    }
}

/// The only exec-env keys the bridge adds outside its allowlist (path values).
pub const LOGIN_ENV_KEYS: &[&str] = &["CLAUDE_CONFIG_DIR", "CODEX_HOME"];

/// The env pair pointing `agent` at its uploaded login dir.
pub fn agent_home_env(_agent: &str, _login_dir: &str) -> Option<(&'static str, String)> {
    None
}

/// The model API hosts `agent` needs reachable.
pub fn model_egress_hosts(_agent: &str) -> &'static [&'static str] {
    &[]
}

/// The narrowed sandbox egress allowlist applied before any upload: sorted,
/// deduped, never empty, never `"*"`.
pub fn narrowed_egress(
    agents: &[String],
    user_domains: &[String],
    install_agents: bool,
) -> Vec<String> {
    let mut out: Vec<String> = agents
        .iter()
        .flat_map(|a| model_egress_hosts(a).iter().map(|h| (*h).to_string()))
        .chain(user_domains.iter().filter(|d| d.as_str() != "*").cloned())
        .collect();
    if install_agents || out.is_empty() {
        out.push("registry.npmjs.org".to_string());
    }
    out.sort();
    out.dedup();
    out
}

/// What leaves the machine for a mode (doctor and the wizard print it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Disclosure {
    pub mode: &'static str,
    pub leaves_machine: &'static str,
    pub accepted: bool,
}

/// The disclosure line for `mode`.
pub fn disclosure(mode: ModelCredentials) -> Disclosure {
    Disclosure {
        mode: mode.as_str(),
        leaves_machine: "nothing: no model credential is sent; model-backed agents cannot log in",
        accepted: mode_refusal(mode).is_none(),
    }
}

/// The refusal text for a mode thegn does not offer (shared with admission).
pub fn mode_refusal(mode: ModelCredentials) -> Option<&'static str> {
    match mode {
        ModelCredentials::None | ModelCredentials::Upload => None,
        ModelCredentials::Proxy => Some(crate::vercel::admit::PROXY_REFUSAL),
        ModelCredentials::Inject => Some(crate::vercel::admit::INJECT_REFUSAL),
    }
}
