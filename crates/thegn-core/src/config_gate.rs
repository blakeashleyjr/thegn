//! `[merge_queue] gate_sandbox` (THE-2200): whether the merge gate's
//! commands may run inside the repo's sealed sandbox (`sealed`). `off` is
//! retained for compatibility but refuses candidate code execution. Kept out
//! of the ratcheted `config.rs`; the field lives on `MergeQueueConfig` and is
//! re-exported from `config`.
//!
//! A sealed runner is required for trusted gates. The default `off` setting
//! fails closed. The setting is read from global config, a trusted profile, or
//! the trusted `[workspace.<slug>.merge_queue]` layer; that per-repo layer may
//! only raise `off` to `sealed` ([`effective_gate_sandbox`]).
//! Repo-authored `.thegn.*` files have no `merge_queue` table at all.

use crate::config::{config_enum, config_warn};

config_enum! {
    /// `[merge_queue] gate_sandbox` — where the gate's commands run.
    ///
    /// - `Off` (default): refuse candidate code execution; host gates are not trusted.
    /// - `Sealed`: `gate_setup_command`, `gate_command` and
    ///   `regenerate_command` run inside the repo's sealed sandbox (network
    ///   `none`, no credentials, no control sockets).
    pub enum GateSandbox: "gate sandbox" {
        Off = "off",
        Sealed = "sealed",
    } default = Off;
}

/// The gate sandbox after a trusted per-repo overlay: the overlay may raise
/// `off` to `sealed` but never lower `sealed` to `off`.
pub fn effective_gate_sandbox(global: GateSandbox, overlay: Option<GateSandbox>) -> GateSandbox {
    match (global, overlay) {
        (GateSandbox::Sealed, _) | (_, Some(GateSandbox::Sealed)) => GateSandbox::Sealed,
        _ => GateSandbox::Off,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_off_and_parses_both_spellings() {
        assert_eq!(GateSandbox::default(), GateSandbox::Off);
        let cfg = crate::config::Config::default();
        assert_eq!(cfg.merge_queue.gate_sandbox, GateSandbox::Off);
        let cfg: crate::config::Config =
            toml::from_str("[merge_queue]\ngate_sandbox = \"sealed\"\n").unwrap();
        assert_eq!(cfg.merge_queue.gate_sandbox, GateSandbox::Sealed);
        let cfg: crate::config::Config =
            toml::from_str("[merge_queue]\ngate_sandbox = \"off\"\n").unwrap();
        assert_eq!(cfg.merge_queue.gate_sandbox, GateSandbox::Off);
    }

    #[test]
    fn a_per_repo_overlay_never_lowers_the_gate_sandbox() {
        use GateSandbox::{Off, Sealed};
        assert_eq!(effective_gate_sandbox(Off, None), Off);
        assert_eq!(effective_gate_sandbox(Off, Some(Off)), Off);
        assert_eq!(effective_gate_sandbox(Off, Some(Sealed)), Sealed);
        assert_eq!(effective_gate_sandbox(Sealed, None), Sealed);
        assert_eq!(effective_gate_sandbox(Sealed, Some(Sealed)), Sealed);
        assert_eq!(
            effective_gate_sandbox(Sealed, Some(Off)),
            Sealed,
            "a repo overlay may not lower a sealed gate"
        );
    }

    #[test]
    fn the_workspace_merge_queue_overlay_only_raises() {
        use crate::config::{MergeQueueConfig, MergeQueueOverlay};
        let mut sealed = MergeQueueConfig {
            gate_sandbox: GateSandbox::Sealed,
            ..MergeQueueConfig::default()
        };
        MergeQueueOverlay {
            gate_sandbox: Some(GateSandbox::Off),
            ..MergeQueueOverlay::default()
        }
        .apply(&mut sealed);
        assert_eq!(sealed.gate_sandbox, GateSandbox::Sealed);

        let mut off = MergeQueueConfig::default();
        let raise = MergeQueueOverlay {
            gate_sandbox: Some(GateSandbox::Sealed),
            ..MergeQueueOverlay::default()
        };
        assert!(!raise.is_empty());
        raise.apply(&mut off);
        assert_eq!(off.gate_sandbox, GateSandbox::Sealed);
    }

    #[test]
    fn gate_sandbox_is_a_security_relevant_path() {
        // `merge_queue` is a SECURITY_RELEVANT_ROOTS entry, so a typo such as
        // `gate_sandbx` refuses instead of silently leaving the gate on the host.
        assert!(crate::config_validate::is_security_relevant_path(
            "merge_queue.gate_sandbox"
        ));
        assert!(crate::config_validate::is_security_relevant_path(
            "project.x.merge_queue.gate_sandbox"
        ));
    }

    #[test]
    fn a_repo_root_overlay_cannot_set_the_gate_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".thegn.toml"),
            "[merge_queue]\ngate_sandbox = \"off\"\n",
        )
        .unwrap();
        let cfg = crate::config::Config {
            merge_queue: crate::config::MergeQueueConfig {
                gate_sandbox: GateSandbox::Sealed,
                ..crate::config::MergeQueueConfig::default()
            },
            ..crate::config::Config::default()
        };
        assert_eq!(
            cfg.repo_merge_queue(dir.path()).gate_sandbox,
            GateSandbox::Sealed
        );
    }
}
