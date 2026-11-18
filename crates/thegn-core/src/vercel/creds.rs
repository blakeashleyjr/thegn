//! `[env.<name>.provider] model_credentials`: which agent login files the
//! `upload` mode copies into a sandbox's tmpfs, where they land, the narrowed
//! egress policy applied first, and the per-mode disclosure line.

use crate::config::ModelCredentials;

/// One login file `upload` copies: `host_rel` is the host path relative to the
/// agent home; `vm_name` is the file name inside `<login_dir>/<agent>/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoginFile {
    pub host_rel: &'static str,
    pub vm_name: &'static str,
}

/// The host login files for `agent`; `Some` only for claude/claude-code/codex.
pub fn login_files(agent: &str) -> Option<&'static [LoginFile]> {
    const CLAUDE: &[LoginFile] = &[LoginFile {
        host_rel: ".credentials.json",
        vm_name: ".credentials.json",
    }];
    const CODEX: &[LoginFile] = &[LoginFile {
        host_rel: "auth.json",
        vm_name: "auth.json",
    }];
    match agent {
        "claude" | "claude-code" => Some(CLAUDE),
        "codex" => Some(CODEX),
        _ => None,
    }
}

/// Files synthesized in the sandbox rather than copied from the host.
pub fn synthesized_files(agent: &str) -> Vec<(&'static str, Vec<u8>)> {
    match agent {
        // UNVERIFIED that claude needs nothing else (live step).
        "claude" | "claude-code" => vec![(
            ".claude.json",
            br#"{"hasCompletedOnboarding":true}"#.to_vec(),
        )],
        _ => Vec::new(),
    }
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
pub fn agent_home_env(agent: &str, login_dir: &str) -> Option<(&'static str, String)> {
    match agent {
        "claude" | "claude-code" => Some(("CLAUDE_CONFIG_DIR", format!("{login_dir}/claude"))),
        "codex" => Some(("CODEX_HOME", format!("{login_dir}/codex"))),
        _ => None,
    }
}

/// The model API hosts `agent` needs reachable.
/// All UNVERIFIED (live step).
pub fn model_egress_hosts(agent: &str) -> &'static [&'static str] {
    match agent {
        "claude" | "claude-code" => &[
            "api.anthropic.com",
            "console.anthropic.com",
            "platform.claude.com",
            "statsig.anthropic.com",
        ],
        "codex" => &["api.openai.com", "auth.openai.com", "chatgpt.com"],
        _ => &[],
    }
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
        .chain(
            user_domains
                .iter()
                .filter(|d| {
                    !d.is_empty() && d.as_str() != "*" && !d.chars().any(char::is_whitespace)
                })
                .cloned(),
        )
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
        leaves_machine: match mode {
            ModelCredentials::None => {
                "nothing: no model credential is sent; model-backed agents cannot log in"
            }
            ModelCredentials::Upload => {
                "your <agent> login file(s) are copied into the sandbox's memory (tmpfs) at each \
                 terminal open; egress is narrowed to the model hosts"
            }
            ModelCredentials::Proxy => {
                "refused: not implemented (needs a public HTTPS endpoint, Vercel OIDC \
                 verification, per-sandbox scoping and revocation in tgproxy)"
            }
            ModelCredentials::Inject => {
                "refused: would hand the model key to Vercel's firewall (the key is stored by \
                 Vercel); not offered"
            }
        },
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

#[cfg(test)]
mod tests {
    use super::*;

    fn v(x: &[&str]) -> Vec<String> {
        x.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn login_files_tables() {
        assert_eq!(
            login_files("claude").unwrap()[0].host_rel,
            ".credentials.json"
        );
        assert_eq!(login_files("claude-code"), login_files("claude"));
        assert_eq!(login_files("codex").unwrap()[0].vm_name, "auth.json");
        assert!(login_files("aider").is_none());
    }

    #[test]
    fn synthesized_only_for_claude() {
        let f = synthesized_files("claude");
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].0, ".claude.json");
        assert_eq!(f[0].1, br#"{"hasCompletedOnboarding":true}"#);
        assert!(synthesized_files("codex").is_empty());
    }

    #[test]
    fn agent_home_env_pairs() {
        let d = "/dev/shm/thegn-login-0123456789abcdef";
        assert_eq!(
            agent_home_env("claude", d),
            Some(("CLAUDE_CONFIG_DIR", format!("{d}/claude")))
        );
        assert_eq!(
            agent_home_env("codex", d),
            Some(("CODEX_HOME", format!("{d}/codex")))
        );
        assert_eq!(agent_home_env("x", d), None);
        assert!(LOGIN_ENV_KEYS.contains(&"CODEX_HOME"));
    }

    #[test]
    fn login_dir_nonce() {
        assert_eq!(
            login_dir("0123456789abcdef").unwrap(),
            "/dev/shm/thegn-login-0123456789abcdef"
        );
        assert!(login_dir("0123456789ABCDEF").is_err());
        assert!(login_dir("short").is_err());
        assert!(login_dir("0123456789abcdeg").is_err());
    }

    #[test]
    fn egress_hosts_and_unknown() {
        assert!(model_egress_hosts("claude").contains(&"api.anthropic.com"));
        assert!(model_egress_hosts("codex").contains(&"chatgpt.com"));
        assert!(model_egress_hosts("nope").is_empty());
    }

    #[test]
    fn narrowed_egress_filters_and_dedupes() {
        let out = narrowed_egress(
            &v(&["codex", "codex"]),
            &v(&[
                "*",
                "",
                "bad host",
                "example.com",
                "example.com",
                "api.openai.com",
            ]),
            false,
        );
        assert_eq!(
            out,
            v(&[
                "api.openai.com",
                "auth.openai.com",
                "chatgpt.com",
                "example.com"
            ])
        );
        let none = narrowed_egress(&[], &v(&["*"]), false);
        assert_eq!(none, v(&["registry.npmjs.org"]));
        let inst = narrowed_egress(&v(&["claude"]), &[], true);
        assert!(inst.contains(&"registry.npmjs.org".to_string()));
        assert!(!inst.contains(&"*".to_string()));
    }

    #[test]
    fn disclosure_lines_and_refusals() {
        let d = disclosure(ModelCredentials::None);
        assert!(d.accepted);
        assert!(d.leaves_machine.starts_with("nothing:"));
        assert!(disclosure(ModelCredentials::Upload).accepted);
        assert!(
            disclosure(ModelCredentials::Upload)
                .leaves_machine
                .contains("tmpfs")
        );
        let p = disclosure(ModelCredentials::Proxy);
        assert!(!p.accepted && p.leaves_machine.starts_with("refused: not implemented"));
        let i = disclosure(ModelCredentials::Inject);
        assert!(!i.accepted && i.leaves_machine.starts_with("refused: would hand"));
        assert_eq!(
            mode_refusal(ModelCredentials::Proxy),
            Some(crate::vercel::admit::PROXY_REFUSAL)
        );
        assert_eq!(
            mode_refusal(ModelCredentials::Inject),
            Some(crate::vercel::admit::INJECT_REFUSAL)
        );
        assert_eq!(mode_refusal(ModelCredentials::None), None);
        assert_eq!(mode_refusal(ModelCredentials::Upload), None);
    }
}
