//! The `[lead]` config section (THE-2171): whether the Lead is configured and
//! how it launches. Absent = no Lead; experimental (stable clamps it to `None`
//! unless `preview = true`, which the in-app setup writes).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::{config_enum, config_warn};
use crate::control::{Scope, ScopeSet};

config_enum! {
    /// How the Lead's sandbox reaches the network (THE-2220). "isolated" runs
    /// it in its own network namespace through `pasta` (outbound internet,
    /// no host loopback or abstract sockets; only the Lead listener port is
    /// forwarded); without `pasta` the Lead refuses to start. "host" is kept
    /// to diagnose old configs, but Lead admission refuses it because it
    /// exposes host loopback services and abstract sockets.
    pub enum LeadNetwork: "lead network" {
        Isolated = "isolated", Host = "host",
    } default = Isolated;
}

/// Default Lead token lifetime: 12 hours.
pub const DEFAULT_TOKEN_TTL_SECS: u64 = 43_200;
/// Shortest Lead token lifetime: 5 minutes.
pub const MIN_TOKEN_TTL_SECS: u64 = 300;
/// Longest Lead token lifetime (also the sliding-refresh cap): 7 days.
pub const MAX_TOKEN_TTL_SECS: u64 = 604_800;

/// `[lead]` — the Lead meta agent: a hosted agent CLI in a global tab.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct LeadConfig {
    /// `[[agents]]` entry the Lead runs (required).
    pub agent: String,
    /// Account id (see `crate::account`) whose credentials the Lead uses;
    /// unset = the agent entry's own env.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// MCP scopes for the Lead's `thegn mcp serve`, clamped by `[mcp.serve]`.
    /// Default `["read", "write"]`. `admin` and `exec` are refused.
    pub scopes: Vec<String>,
    /// `[mcp_proxy.profiles]` entry exposing the Lead's own MCP servers.
    pub mcp_profile: String,
    /// Skill seeded into the notes dir.
    pub skill: String,
    /// The Lead's only writable directory. Default `$XDG_STATE_HOME/thegn/lead`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes_dir: Option<PathBuf>,
    /// Network namespace for the Lead sandbox. Default `isolated`.
    pub network: LeadNetwork,
    /// Lead control-token lifetime in seconds (300..=604800, default 43200).
    /// A token in use slides forward once less than half remains, capped at
    /// seven days from minting.
    #[schemars(range(min = 300, max = 604_800))]
    pub token_ttl_secs: u64,
    /// Outward caps (`issues.comment`, `issues.update`) the Lead may call
    /// without a per-call owner approval. Default empty: every outward call
    /// asks first.
    pub outward_allow: Vec<String>,
    /// Preview marker written by the in-app setup flow (never hand-edited).
    /// On the stable channel `[lead]` is ignored unless this is `true`, so the
    /// Lead runs there only after the user set it up explicitly.
    pub preview: bool,
}

impl Default for LeadConfig {
    fn default() -> Self {
        Self {
            agent: String::new(),
            account: None,
            scopes: vec!["read".into(), "write".into()],
            mcp_profile: "lead".into(),
            skill: "lead".into(),
            notes_dir: None,
            network: LeadNetwork::default(),
            token_ttl_secs: DEFAULT_TOKEN_TTL_SECS,
            outward_allow: Vec::new(),
            preview: false,
        }
    }
}

fn valid_slug(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

impl LeadConfig {
    /// Config errors for this section; `agents` is `[[agents]]`.
    pub fn validate(&self, agents: &[crate::config::NamedCommand]) -> Vec<String> {
        let mut errs = Vec::new();
        if self.agent.trim().is_empty() {
            errs.push("[lead] agent is required (name an [[agents]] entry)".to_string());
        } else if !agents.iter().any(|a| a.name == self.agent) {
            errs.push(format!(
                "[lead] agent {:?} names no [[agents]] entry",
                self.agent
            ));
        }
        for s in &self.scopes {
            match s.trim() {
                "admin" | "exec" => errs.push(format!(
                    "[lead] scopes: {s:?} is refused for the Lead (admin and exec are never granted)"
                )),
                "read" | "write" | "git" | "merge_add" => {}
                other => errs.push(format!("[lead] scopes: unknown scope {other:?}")),
            }
        }
        if !valid_slug(&self.mcp_profile) {
            errs.push(format!(
                "[lead] mcp_profile {:?} must be non-empty and use only [a-z0-9_-]",
                self.mcp_profile
            ));
        }
        if !valid_slug(&self.skill) {
            errs.push(format!(
                "[lead] skill {:?} must be non-empty and use only [a-z0-9_-]",
                self.skill
            ));
        }
        if !(MIN_TOKEN_TTL_SECS..=MAX_TOKEN_TTL_SECS).contains(&self.token_ttl_secs) {
            errs.push(format!(
                "[lead] token_ttl_secs must be between {MIN_TOKEN_TTL_SECS} and {MAX_TOKEN_TTL_SECS} (got {})",
                self.token_ttl_secs
            ));
        }
        for cap in &self.outward_allow {
            if !crate::outward_gate::OUTWARD_CAPS.contains(&cap.as_str()) {
                errs.push(format!(
                    "[lead] outward_allow: {cap:?} is not an outward cap (expected one of {})",
                    crate::outward_gate::OUTWARD_CAPS.join(", ")
                ));
            }
        }
        if let Some(dir) = &self.notes_dir {
            let expanded = crate::util::expand_tilde(&dir.to_string_lossy());
            if !std::path::Path::new(&expanded).is_absolute() {
                errs.push(format!(
                    "[lead] notes_dir {} must be an absolute path",
                    dir.display()
                ));
            }
        }
        errs
    }

    /// The configured scopes as a [`ScopeSet`]; `admin`/`exec` and unknown
    /// names are dropped (only ever narrows).
    pub fn scope_set(&self) -> ScopeSet {
        let set = ScopeSet::parse(&self.scopes.join(","));
        let mut out = ScopeSet::empty();
        for sc in [Scope::Read, Scope::Write, Scope::Git, Scope::MergeAdd] {
            if set.contains(sc) {
                out.insert(sc);
            }
        }
        out
    }

    /// The notes dir with `~` expanded; default `$XDG_STATE_HOME/thegn/lead`.
    pub fn notes_dir_resolved(&self) -> PathBuf {
        match &self.notes_dir {
            Some(p) => PathBuf::from(crate::util::expand_tilde(&p.to_string_lossy())),
            None => crate::util::xdg_state_home().join("thegn").join("lead"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NamedCommand;

    fn agents() -> Vec<NamedCommand> {
        vec![toml::from_str::<NamedCommand>("name = \"claude\"\ncommand = \"claude\"").unwrap()]
    }

    fn ok() -> LeadConfig {
        LeadConfig {
            agent: "claude".into(),
            ..Default::default()
        }
    }

    #[test]
    fn defaults() {
        let d = LeadConfig::default();
        assert_eq!(d.scopes, ["read", "write"]);
        assert_eq!(d.mcp_profile, "lead");
        assert_eq!(d.skill, "lead");
        assert!(d.account.is_none() && d.notes_dir.is_none());
        assert!(d.notes_dir_resolved().ends_with("thegn/lead"));
        assert_eq!(d.network, LeadNetwork::Isolated);
        assert_eq!(d.token_ttl_secs, 43_200);
        assert!(d.outward_allow.is_empty());
        assert!(!d.preview);
    }

    #[test]
    fn network_parses_and_unknown_is_refused() {
        let c: LeadConfig = toml::from_str("agent = \"claude\"\nnetwork = \"host\"").unwrap();
        assert_eq!(c.network, LeadNetwork::Host);
        assert_eq!(LeadNetwork::Isolated.as_str(), "isolated");
        // The strict parse refuses an unknown mode ...
        assert!(LeadNetwork::from_str_validated("bridge").is_err());
        // ... and deserialization follows the repo-wide `config_enum!`
        // contract: warn and fall back to the default, which for the Lead is
        // the safe `isolated` mode (never `host`). `thegn config validate`
        // flags the unknown value through the schema enum walk.
        let c: LeadConfig = toml::from_str("network = \"bridge\"").unwrap();
        assert_eq!(c.network, LeadNetwork::default());
        assert_eq!(c.network, LeadNetwork::Isolated);
    }

    #[test]
    fn token_ttl_range() {
        for ok_ttl in [300, 43_200, 604_800] {
            let c = LeadConfig {
                token_ttl_secs: ok_ttl,
                ..ok()
            };
            assert!(c.validate(&agents()).is_empty(), "{ok_ttl}");
        }
        for bad in [0, 299, 604_801] {
            let c = LeadConfig {
                token_ttl_secs: bad,
                ..ok()
            };
            assert!(c.validate(&agents())[0].contains("token_ttl_secs"), "{bad}");
        }
    }

    #[test]
    fn outward_allow_accepts_only_outward_caps() {
        let c = LeadConfig {
            outward_allow: vec!["issues.comment".into(), "issues.update".into()],
            ..ok()
        };
        assert!(c.validate(&agents()).is_empty());
        let c = LeadConfig {
            outward_allow: vec!["sessions.open".into()],
            ..ok()
        };
        let errs = c.validate(&agents());
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("outward_allow"));
    }

    #[test]
    fn serde_round_trip() {
        let cfg: LeadConfig = toml::from_str("agent = \"claude\"\naccount = \"w\"").unwrap();
        assert_eq!(cfg.account.as_deref(), Some("w"));
        assert_eq!(cfg.scopes, ["read", "write"]);
        let back: LeadConfig = toml::from_str(&toml::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn valid_config_has_no_errors() {
        assert!(ok().validate(&agents()).is_empty());
    }

    #[test]
    fn agent_errors() {
        assert_eq!(LeadConfig::default().validate(&agents()).len(), 1);
        let c = LeadConfig {
            agent: "nope".into(),
            ..Default::default()
        };
        assert!(c.validate(&agents())[0].contains("no [[agents]]"));
    }

    #[test]
    fn scope_errors() {
        for bad in ["admin", "exec", "bogus"] {
            let c = LeadConfig {
                scopes: vec![bad.into()],
                ..ok()
            };
            assert_eq!(c.validate(&agents()).len(), 1, "{bad}");
        }
    }

    #[test]
    fn slug_errors() {
        for bad in ["", "Lead", "a b", "a/b"] {
            let c = LeadConfig {
                mcp_profile: bad.into(),
                skill: bad.into(),
                ..ok()
            };
            assert_eq!(c.validate(&agents()).len(), 2, "{bad:?}");
        }
    }

    #[test]
    fn notes_dir_must_be_absolute() {
        let c = LeadConfig {
            notes_dir: Some("rel/dir".into()),
            ..ok()
        };
        assert_eq!(c.validate(&agents()).len(), 1);
        let c = LeadConfig {
            notes_dir: Some("~/x".into()),
            ..ok()
        };
        assert!(c.validate(&agents()).is_empty());
        let c = LeadConfig {
            notes_dir: Some("/abs".into()),
            ..ok()
        };
        assert_eq!(c.notes_dir_resolved(), PathBuf::from("/abs"));
    }

    #[test]
    fn scope_set_drops_admin_exec_and_unknown() {
        let c = LeadConfig {
            scopes: vec!["read".into(), "admin".into(), "exec".into(), "zzz".into()],
            ..ok()
        };
        let s = c.scope_set();
        assert!(s.contains(Scope::Read));
        assert!(!s.contains(Scope::Admin) && !s.contains(Scope::Exec));
        assert_eq!(ok().scope_set().to_csv(), "read,write");
    }
}
