//! The one host-key verification policy table (THE-66).
//!
//! Before this module, three different host-key policies were scattered across
//! five call sites as bare `-o StrictHostKeyChecking=…` literals, each with its
//! justification in a nearby comment rather than in a checkable place. This is
//! the chokepoint — the ssh equivalent of `wire.rs::color_spec` for colors:
//! every ssh invocation thegn builds names one of four [connection
//! classes](HostKeyClass) and asks here for its host-key `-o` options and its
//! agent-forwarding decision. A shrink-only ratchet — a Rust test
//! (`host_key_literals_stay_in_the_chokepoint`) in each crate's
//! `platform_ratchet_tests.rs`, allowlists `test/hostkey-{core,svc,host}-ratchet.txt`,
//! running in `just test` — keeps new `StrictHostKeyChecking` /
//! `UserKnownHostsFile` / `HostKeyAlias` literals from appearing outside this file.
//!
//! Pure: it returns argv tokens; it performs no I/O and reads no config.

/// The four connection classes, each with exactly one host-key policy. See the
/// policy table in `thegn doctor` (and the design doc) for the justifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKeyClass {
    /// The user's own hosts: `[host.*]`, `[env.*.ssh]`, git-remote targets.
    /// Their `~/.ssh` trust store is authoritative — thegn adds nothing and
    /// must not loosen it.
    UserDeclared,
    /// A managed instance (VPS/machine0/autoscale VM) just provisioned. First
    /// contact is unauthenticated by construction; pin from the first key into
    /// a per-instance `known_hosts` deleted with the instance, so it can never
    /// pollute the user's global file.
    ManagedFresh,
    /// A loopback endpoint reached over an already-authenticated transport
    /// (sprite SSH-over-WSS, iroh dumbpipe to `127.0.0.1:<ephemeral>`). Endpoint
    /// identity is established by the outer transport (WSS auth / iroh ticket);
    /// an inner TOFU pin against a churning port would only produce false
    /// mismatches, so the inner check is disabled — or pinned by a stable
    /// `HostKeyAlias` where one exists (the better form).
    LoopbackTunneled,
    /// An in-sandbox git bootstrap (`core.sshCommand` in a fresh sandbox home).
    /// A fresh sandbox has no `known_hosts`; accept-new, scoped inside the
    /// sandbox home so the write never touches the real user's file.
    SandboxBootstrap,
}

/// Per-connection specifics the policy needs. Empty for classes that take none.
#[derive(Debug, Clone, Default)]
pub struct HostKeyContext {
    /// `ManagedFresh`: the per-instance `known_hosts` file to pin into. `None`
    /// means accept-new against the global file (autoscale engine VMs, which
    /// have no per-instance registry entry).
    pub known_hosts: Option<String>,
    /// `LoopbackTunneled`: a stable host-key alias (e.g. `thegn-iroh-<hash>`).
    /// `Some` ⇒ accept-new pinned by alias; `None` ⇒ inner check disabled.
    pub alias: Option<String>,
}

impl HostKeyClass {
    /// A short human label for the `thegn doctor` policy table.
    pub fn label(self) -> &'static str {
        match self {
            HostKeyClass::UserDeclared => "user-declared",
            HostKeyClass::ManagedFresh => "managed-fresh",
            HostKeyClass::LoopbackTunneled => "loopback-tunneled",
            HostKeyClass::SandboxBootstrap => "sandbox-bootstrap",
        }
    }

    /// The one-line policy description for the doctor table.
    pub fn policy_summary(self) -> &'static str {
        match self {
            HostKeyClass::UserDeclared => "defer to the user's ssh trust config (add nothing)",
            HostKeyClass::ManagedFresh => {
                "accept-new into a per-instance known_hosts, deleted with the instance"
            }
            HostKeyClass::LoopbackTunneled => {
                "inner check off (or HostKeyAlias pin); endpoint verified by the outer transport"
            }
            HostKeyClass::SandboxBootstrap => "accept-new, scoped inside the sandbox home",
        }
    }

    /// The justification for the doctor table.
    pub fn justification(self) -> &'static str {
        match self {
            HostKeyClass::UserDeclared => "the user's trust store is authoritative",
            HostKeyClass::ManagedFresh => {
                "first contact is unauthenticated; pin cannot leak globally"
            }
            HostKeyClass::LoopbackTunneled => "WSS auth / iroh ticket already established identity",
            HostKeyClass::SandboxBootstrap => {
                "fresh sandbox has no known_hosts; write stays inside"
            }
        }
    }

    /// Whether ssh agent forwarding (`-A`) is permitted for this class. Only
    /// `UserDeclared` honors a caller's configured choice; every managed or
    /// tunneled class forces forwarding off — an ephemeral box has no business
    /// signing with the user's agent.
    pub fn forward_agent_allowed(self, configured: bool) -> bool {
        match self {
            HostKeyClass::UserDeclared => configured,
            _ => false,
        }
    }

    /// Every class, for the doctor table and coverage tests.
    pub const ALL: [HostKeyClass; 4] = [
        HostKeyClass::UserDeclared,
        HostKeyClass::ManagedFresh,
        HostKeyClass::LoopbackTunneled,
        HostKeyClass::SandboxBootstrap,
    ];
}

/// The host-key `-o` option tokens for a connection class. This is the policy
/// table: the only place `StrictHostKeyChecking` / `UserKnownHostsFile` /
/// `HostKeyAlias` are constructed.
pub fn host_key_args(class: HostKeyClass, ctx: &HostKeyContext) -> Vec<String> {
    let o = |s: String| vec!["-o".to_string(), s];
    match class {
        // The user's trust store decides; add nothing.
        HostKeyClass::UserDeclared => Vec::new(),
        HostKeyClass::ManagedFresh => {
            let mut v = o("StrictHostKeyChecking=accept-new".into());
            if let Some(kh) = &ctx.known_hosts {
                v.extend(o(format!("UserKnownHostsFile={kh}")));
            }
            v
        }
        HostKeyClass::LoopbackTunneled => match &ctx.alias {
            Some(alias) => {
                let mut v = o("StrictHostKeyChecking=accept-new".into());
                v.extend(o(format!("HostKeyAlias={alias}")));
                v
            }
            None => {
                let mut v = o("StrictHostKeyChecking=no".into());
                v.extend(o("UserKnownHostsFile=/dev/null".into()));
                v
            }
        },
        // accept-new, global scope inside the sandbox home.
        HostKeyClass::SandboxBootstrap => o("StrictHostKeyChecking=accept-new".into()),
    }
}

/// The host-key options rendered as a single space-joined `-o …` string, for
/// call sites that pass ssh options through one env var / git config value
/// (`NIX_SSHOPTS`, `core.sshCommand`) rather than an argv vector.
pub fn host_key_opts_str(class: HostKeyClass, ctx: &HostKeyContext) -> String {
    host_key_args(class, ctx).join(" ")
}

/// The forbidden host-key option literals, in one place next to the chokepoint
/// that owns them. Any ssh `-o` host-key option must be built by
/// [`host_key_args`], never written as a literal at a call site — the per-crate
/// host-key ratchet tests (`platform_ratchet_tests.rs` in core/svc/host) use
/// this predicate so the forbidden set has a single definition.
pub const HOST_KEY_LITERALS: [&str; 3] = [
    "StrictHostKeyChecking",
    "UserKnownHostsFile",
    "HostKeyAlias",
];

/// Whether a (comment-stripped) source body names a host-key option literal.
pub fn is_host_key_literal(body: &str) -> bool {
    HOST_KEY_LITERALS.iter().any(|l| body.contains(l))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_declared_defers_and_adds_nothing() {
        assert!(host_key_args(HostKeyClass::UserDeclared, &HostKeyContext::default()).is_empty());
    }

    #[test]
    fn managed_fresh_pins_per_instance_known_hosts() {
        let ctx = HostKeyContext {
            known_hosts: Some("/state/vps/known_hosts.d/box1".into()),
            ..Default::default()
        };
        assert_eq!(
            host_key_args(HostKeyClass::ManagedFresh, &ctx),
            vec![
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "UserKnownHostsFile=/state/vps/known_hosts.d/box1",
            ]
        );
    }

    #[test]
    fn managed_fresh_without_file_is_accept_new_global() {
        assert_eq!(
            host_key_args(HostKeyClass::ManagedFresh, &HostKeyContext::default()),
            vec!["-o", "StrictHostKeyChecking=accept-new"]
        );
    }

    #[test]
    fn loopback_without_alias_disables_the_inner_check() {
        assert_eq!(
            host_key_args(HostKeyClass::LoopbackTunneled, &HostKeyContext::default()),
            vec![
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
            ]
        );
    }

    #[test]
    fn loopback_with_alias_pins_by_alias() {
        let ctx = HostKeyContext {
            alias: Some("thegn-iroh-abc".into()),
            ..Default::default()
        };
        assert_eq!(
            host_key_args(HostKeyClass::LoopbackTunneled, &ctx),
            vec![
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "HostKeyAlias=thegn-iroh-abc",
            ]
        );
    }

    #[test]
    fn sandbox_bootstrap_is_accept_new_only() {
        assert_eq!(
            host_key_args(HostKeyClass::SandboxBootstrap, &HostKeyContext::default()),
            vec!["-o", "StrictHostKeyChecking=accept-new"]
        );
        assert_eq!(
            host_key_opts_str(HostKeyClass::SandboxBootstrap, &HostKeyContext::default()),
            "-o StrictHostKeyChecking=accept-new"
        );
    }

    #[test]
    fn only_user_declared_may_forward_the_agent() {
        assert!(HostKeyClass::UserDeclared.forward_agent_allowed(true));
        assert!(!HostKeyClass::UserDeclared.forward_agent_allowed(false));
        for class in [
            HostKeyClass::ManagedFresh,
            HostKeyClass::LoopbackTunneled,
            HostKeyClass::SandboxBootstrap,
        ] {
            assert!(
                !class.forward_agent_allowed(true),
                "{class:?} must force -a off"
            );
        }
    }

    #[test]
    fn every_class_has_nonempty_doctor_strings() {
        for class in HostKeyClass::ALL {
            assert!(!class.label().is_empty());
            assert!(!class.policy_summary().is_empty());
            assert!(!class.justification().is_empty());
        }
    }
}
