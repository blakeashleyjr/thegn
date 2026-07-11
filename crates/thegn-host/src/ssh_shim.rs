//! ssh-config ownership shim for bwrap sandboxes.

use thegn_core::sandbox;

/// Unprivileged bwrap maps the nix store to `nobody` (the userns overflow
/// uid), so ssh rejects the store-resident `~/.ssh/config` ("Bad owner or
/// permissions") and ssh-based git fails in the sandbox. Point sandboxed git
/// at a user-owned, include-flattened copy materialized on the host (visible
/// via the rw `$HOME` bind). Bwrap only, and only when `$HOME` (or `/`) is
/// mounted so the copy is reachable. Shared by the pane launch path and the
/// embedded `agent` tab's tool sandbox. See [`sandbox::prepare_ssh_config`].
pub(crate) fn apply(spec: &mut sandbox::SandboxSpec) {
    if spec.backend != sandbox::Backend::Bwrap || spec.env_overrides.contains_key("GIT_SSH_COMMAND")
    {
        return;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let home_mounted =
        !home.is_empty() && spec.mounts.iter().any(|m| m.dest == home || m.dest == "/");
    if home_mounted && let Some(path) = sandbox::prepare_ssh_config() {
        spec.env_overrides
            .insert("GIT_SSH_COMMAND".to_string(), format!("ssh -F {path}"));
    }
}
