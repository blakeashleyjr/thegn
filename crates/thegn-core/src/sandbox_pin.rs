//! Per-worktree sandbox profile selection and its user-facing explanation.

use anyhow::{Result, bail};

use crate::config::SandboxProfile;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EnvSelection {
    pub name: Option<String>,
    pub profile: Option<SandboxProfile>,
}

impl EnvSelection {
    pub const fn none() -> Self {
        Self {
            name: None,
            profile: None,
        }
    }

    pub fn named(name: &str) -> Self {
        Self {
            name: Some(name.to_string()),
            profile: None,
        }
    }

    pub const fn with_profile(mut self, profile: SandboxProfile) -> Self {
        self.profile = Some(profile);
        self
    }

    pub const fn is_empty(&self) -> bool {
        self.name.is_none() && self.profile.is_none()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProfileExplain {
    pub title: &'static str,
    pub one_liner: &'static str,
    pub hides: &'static [&'static str],
    pub network: &'static str,
    pub credentials: &'static str,
    pub agents: &'static str,
    pub how_to_change: &'static str,
}

pub fn parse_pin(value: &str) -> Result<SandboxProfile> {
    match value.trim().to_ascii_lowercase().as_str() {
        "standard" | "hardened" | "guarded" => Ok(SandboxProfile::Hardened),
        "sealed" | "locked" | "isolated" => Ok(SandboxProfile::Sealed),
        "open" | "off" | "none" => {
            bail!("open is a config-only escape hatch; choose standard or sealed")
        }
        "sealed-tunnel" | "tunnel-only" | "vpn-only" => {
            bail!(
                "sealed-tunnel needs a VPN that cannot be enforced yet; choose standard or sealed"
            )
        }
        _ => bail!("unknown sandbox profile {value:?}; choose standard or sealed"),
    }
}

pub const fn explain(profile: SandboxProfile) -> ProfileExplain {
    match profile {
        SandboxProfile::Sealed => ProfileExplain {
            title: "Sealed",
            one_liner: "Private home, no network, and every capability dropped.",
            hides: &[
                "Home is a private empty directory with a small shell/git allowlist.",
                "Credentials, ~/.ssh, agent logins, and thegn state are hidden.",
            ],
            network: "No network. git fetch/push and API-backed agents will not work.",
            credentials: "Host credentials and the SSH agent socket are hidden.",
            agents: "Local tools and offline builds work; remote environments and host runs are refused.",
            how_to_change: "Change later: palette > Isolation profile, or thegn sandbox profile set <standard|sealed>.",
        },
        _ => ProfileExplain {
            title: "Standard (default)",
            one_liner: "A read-only view of files outside this worktree.",
            hides: &[
                "Files outside the worktree are read-only.",
                "Your home remains readable, including credentials, ~/.ssh, cloud tokens, and agent CLI logins.",
            ],
            network: "Network works for fetch, clone, installs, and API-backed agents.",
            credentials: "Credentials in your read-only home remain visible.",
            agents: "Works with local, SSH, and provider environments; default capabilities remain.",
            how_to_change: "Change later: palette > Isolation profile, or thegn sandbox profile set <standard|sealed>.",
        },
    }
}

#[cfg(test)]
#[path = "sandbox_pin_tests.rs"]
mod sandbox_pin_tests;
