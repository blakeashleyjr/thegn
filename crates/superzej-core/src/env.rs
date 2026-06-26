//! Named **execution environments** — the resolved product of `[env.<name>]`
//! config selected per workspace/repo/worktree.
//!
//! An [`Environment`] bundles the three orthogonal axes that used to be tangled
//! in a single `[sandbox]` block:
//!
//! - **placement** ([`Placement`]) — *where* processes run (local / ssh / k8s /
//!   provider). The exec primitive.
//! - **isolation** ([`SandboxConfig`]) — *how* they're sandboxed (podman / bwrap
//!   / none + hardening). Reuses the existing sandbox machinery.
//! - **data** ([`DataMode`]) — *where the worktree files live* (in the env, on
//!   the host with remote exec, or sshfs-mounted).
//!
//! [`Config::resolve_env`](crate::config::Config::resolve_env) layers the named
//! env onto the base `[sandbox]` and returns one of these. The default env (no
//! `[env.*]` selected) reproduces today's behavior exactly.

use crate::config::{DataMode, SandboxConfig};
use crate::placement::Placement;

/// A fully-resolved execution environment for a worktree.
#[derive(Debug, Clone)]
pub struct Environment {
    /// The selected env name (`"default"` for the implicit `[sandbox]` env).
    pub name: String,
    /// Where the worktree's processes run.
    pub placement: Placement,
    /// How they're isolated (the base `[sandbox]` with the env overlay applied).
    pub sandbox: SandboxConfig,
    /// Where the worktree's files physically live.
    pub data: DataMode,
}

impl Environment {
    /// Whether this environment runs anywhere other than the local host.
    pub fn is_remote(&self) -> bool {
        !self.placement.is_local()
    }

    /// A short status label, e.g. `default · local`, `company-k8s · k8s:ns/pod`.
    pub fn label(&self) -> String {
        format!("{} · {}", self.name, self.placement.label())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SandboxConfig;

    #[test]
    fn label_and_is_remote() {
        let local = Environment {
            name: "default".into(),
            placement: Placement::Local,
            sandbox: SandboxConfig::default(),
            data: DataMode::InEnv,
        };
        assert_eq!(local.label(), "default · local");
        assert!(!local.is_remote());

        let k8s = Environment {
            name: "company-k8s".into(),
            placement: Placement::K8s(crate::placement::K8sPlacement {
                kubectl: "kubectl".into(),
                context: None,
                namespace: Some("ns".into()),
                pod: "p".into(),
                container: None,
                pod_template: None,
                image: None,
            }),
            sandbox: SandboxConfig::default(),
            data: DataMode::InEnv,
        };
        assert_eq!(k8s.label(), "company-k8s · k8s:ns/p");
        assert!(k8s.is_remote());
    }
}
