//! Compatibility surface for the removed host-side `direnv` warm path.
//!
//! Thegn never evaluates a repository `.envrc` on the host, runs `direnv
//! allow`, writes `.direnv`, or starts a detached warm thread. Repository
//! environment setup is either an explicit user action or target-side work
//! performed inside the selected sandbox/provider.
//!
//! ## Per-pane `direnv` state replay is not captured on the launch path
//!
//! A repo with a slow `.envrc` costs every new pane a full load in that pane's
//! own shell. Skipping it — export once, then replay the exported environment
//! plus `DIRENV_DIR`/`DIRENV_FILE`/`DIRENV_DIFF`/`DIRENV_WATCHES` into later
//! panes so the shell hook finds an already-loaded state — can only be filled by
//! running `direnv export` *on the host*, which is exactly what the sandbox
//! spec forbids ("Never implicitly evaluate repository environments on the
//! host", `openspec/specs/sandbox/spec.md`) and what the THE-418 change
//! removed. `crates/thegn-host/src/agent_tests.rs` pins that no `direnv` or
//! `nix` process is spawned from spec resolution for any `warm_direnv` value, so
//! adding one here breaks a security regression, not just a preference.
//!
//! Any such cache therefore has to be written by an explicit, consented action,
//! mirroring [`crate::devenv`]: `thegn sandbox warm-devshell` evaluates and
//! stores, pane spawn only ever *reads* an already-existing entry and never
//! evaluates. Injecting a replayed environment raises its own questions (which
//! variables a repo may set in a pane, and how `DIRENV_WATCHES` — a
//! base64/deflate JSON watch list — is validated), so wiring that up is a
//! product/trust decision, not a launch-path edit.

use crate::config::WarmDirenv;

/// The old synchronous warm planner is retained only for source compatibility.
/// No configuration value authorizes host warming anymore.
pub const fn warm_now_plan(_mode: WarmDirenv) -> Option<bool> {
    None
}

#[cfg(test)]
mod tests {
    use super::warm_now_plan;
    use crate::config::WarmDirenv;

    #[test]
    fn every_legacy_mode_is_inert() {
        for mode in [WarmDirenv::Auto, WarmDirenv::AllowedOnly, WarmDirenv::Off] {
            assert_eq!(warm_now_plan(mode), None);
            assert!(!mode.host_warming_enabled());
        }
    }
}
