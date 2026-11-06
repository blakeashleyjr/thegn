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
    pub extra: &'static [&'static str],
    pub how_to_change: &'static str,
}

pub fn parse_pin(value: &str) -> Result<SandboxProfile> {
    let profile = SandboxProfile::from_str_validated(value).map_err(anyhow::Error::msg)?;
    match profile {
        SandboxProfile::Hardened | SandboxProfile::Sealed => Ok(profile),
        SandboxProfile::Open => {
            bail!("open is a config-only escape hatch; choose standard or sealed")
        }
        SandboxProfile::SealedTunnel => bail!(
            "sealed-tunnel needs a VPN that cannot be enforced yet; choose standard or sealed"
        ),
    }
}

pub const fn explain(profile: SandboxProfile) -> ProfileExplain {
    match profile {
        SandboxProfile::Sealed => ProfileExplain {
            title: "Sealed",
            one_liner: "Private home, no network, all Linux capabilities dropped.",
            hides: &[
                "Home is private and empty except for a small shell/git allowlist.",
                "Credentials, ~/.ssh, agent logins and thegn state are hidden.",
            ],
            network: "No network or VPN attachment; git fetch/push will not work.",
            credentials: "Credentials and the SSH agent socket are hidden.",
            agents: "SSH/provider envs and run-on-host are refused.",
            extra: &[
                "An unsealable launch is refused, never downgraded.",
                "API-backed agent CLIs fail; local tools and offline builds work.",
                "Root is read-only; new privileges are blocked; 256 process limit.",
            ],
            how_to_change: "Change via palette > Isolation profile or the CLI.",
        },
        SandboxProfile::Hardened => ProfileExplain {
            title: "Standard",
            one_liner: "A read-only view of files outside this worktree.",
            hides: &[
                "Files outside the worktree are read-only.",
                "Home stays readable, including credentials, ~/.ssh and cloud tokens.",
            ],
            network: "Network and VPN attachments work.",
            credentials: "Credentials in your read-only home remain visible.",
            agents: "SSH/provider environments work; default capabilities remain.",
            extra: &["Root is read-only; new privileges blocked; 512 process limit."],
            how_to_change: "Change via palette > Isolation profile or the CLI.",
        },
        SandboxProfile::Open => ProfileExplain {
            title: "Open",
            one_liner: "Runs without the standard profile's filesystem and process hardening.",
            hides: &["Files outside this worktree follow the configured host policy."],
            network: "Network follows the configured sandbox network mode.",
            credentials: "Home and credentials follow the configured sandbox mounts.",
            agents: "Open is config-only; configured environments remain available.",
            extra: &[],
            how_to_change: "Choose standard or sealed for a worktree override.",
        },
        SandboxProfile::SealedTunnel => ProfileExplain {
            title: "Sealed tunnel",
            one_liner: "Sealed isolation with a tunnel-only network request.",
            hides: &["Home is private and empty except for a small shell/git allowlist."],
            network: "A VPN tunnel must be enforceable as the only network route.",
            credentials: "Credentials and the SSH agent socket are hidden.",
            agents: "SSH/provider environments and run-on-host are refused.",
            extra: &["Unsealable launches are refused, never downgraded."],
            how_to_change: "Choose standard or sealed for a worktree override.",
        },
    }
}

#[cfg(test)]
#[path = "sandbox_pin_tests.rs"]
mod sandbox_pin_tests;
