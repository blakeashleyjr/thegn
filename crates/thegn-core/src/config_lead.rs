//! The `[lead]` config section (THE-2171): whether the Lead is configured and
//! how it launches. Absent = no Lead; experimental (stable clamps it to `None`).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::control::{Scope, ScopeSet};

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
