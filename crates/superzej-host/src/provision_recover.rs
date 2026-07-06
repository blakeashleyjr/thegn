//! Provisioning helpers split out of the pinned `agent.rs` god-file: the
//! per-step exec login-shell argv builder and the "did this step likely restart
//! the sandbox?" classifier that drives a `wait_ready` recovery between steps.
//!
//! ## Why the recovery exists
//!
//! A pooled sprite runs at the provider's default microVM size. A heavy
//! best-effort step (the Nix install) can exceed that and get OOM-killed
//! (`exit 137` = SIGKILL), which **restarts the whole VM**. Each subsequent
//! step opens a fresh exec connection (`run_exec` → `open_exec`); while the VM
//! is restarting those connects fail (`sprites: exec ws connect`) or time out,
//! so every later step cascades to failure even though nothing is wrong with
//! them. Between a step that signals a probable restart and the next step, we
//! give the sandbox a bounded window to come back (`wait_ready`) so the cascade
//! turns into a single warned-and-recovered blip.

/// Build the `/bin/sh -lc` argv for a provisioning exec step. The provider exec
/// env is non-login (no `$USER`), so the installer's `profile.d` hook is a
/// no-op — each step must put the nix/tool dirs on `PATH` itself for a later
/// step to see what an earlier one installed. CRITICAL: include the
/// daemon/system profile (`/nix/var/nix/profiles/default`) where the
/// Determinate installer (`--init none`) lands — without it every nix-using
/// step fails `nix: not found` after a successful install, leaving a bare
/// shell. `2>&1` folds stderr into the non-tty capture.
pub(crate) fn exec_login_argv(script: &str) -> Vec<String> {
    vec![
        "/bin/sh".to_string(),
        "-lc".to_string(),
        format!(
            // `[ -r F ] && . F`, NOT `. F 2>/dev/null || true`: in dash (the
            // sandbox `/bin/sh`) sourcing a MISSING file is a special-builtin
            // error that exits the shell with status 2 — `|| true` can't catch
            // it — so on a fresh sandbox (no nix yet) it aborted EVERY step.
            "[ -r /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh ] && \
             . /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh; \
             export PATH=\"$HOME/.nix-profile/bin:/nix/var/nix/profiles/default/bin:$HOME/.local/state/nix/profile/bin:$HOME/.local/bin:$PATH\"; {script} 2>&1"
        ),
    ]
}

/// Whether a failed provisioning step likely **restarted the sandbox VM** — so
/// the runner should `wait_ready` before the next step rather than let every
/// remaining step independently exhaust its connect budget against a
/// still-restarting VM. Pure so the signals are unit-tested.
///
/// Signals: an `exit 137` (128+SIGKILL — the OOM-killer), or an error whose
/// text names a lost/timed-out exec connection (`exec ws connect`, `timed
/// out`). Only meaningful for the exec steps; the host-side steps
/// (dotfiles/closure push) don't run in the sandbox.
pub(crate) fn step_signals_sandbox_restart(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("exit 137")
        || e.contains("exec ws connect")
        || e.contains("ws connect")
        || e.contains("timed out")
        || e.contains("never became ready")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_argv_sources_daemon_profile_and_folds_stderr() {
        let argv = exec_login_argv("nix --version");
        assert_eq!(argv[0], "/bin/sh");
        assert_eq!(argv[1], "-lc");
        assert!(
            argv[2].contains("nix-daemon.sh"),
            "sources the daemon profile"
        );
        assert!(argv[2].contains("nix --version"), "embeds the script");
        assert!(
            argv[2].trim_end().ends_with("2>&1"),
            "folds stderr into stdout"
        );
    }

    #[test]
    fn restart_signals_recognized() {
        // OOM/SIGKILL: the Nix-install-killed-the-VM trigger.
        assert!(step_signals_sandbox_restart(
            "Install Nix (exit 137): killed"
        ));
        // The cascade error every later step hits while the VM is down.
        assert!(step_signals_sandbox_restart("sprites: exec ws connect"));
        assert!(step_signals_sandbox_restart(
            "sprites: exec ws connect timed out after 90s (sandbox never became ready)"
        ));
        assert!(step_signals_sandbox_restart("exec timed out after 300s"));
    }

    #[test]
    fn ordinary_failures_do_not_trigger_recovery() {
        // A plain non-zero exit (e.g. a setup script bug) is not a VM restart —
        // no point waiting on readiness; the sandbox is up and the next step
        // should just run.
        assert!(!step_signals_sandbox_restart(
            "Run setup (exit 1): command failed"
        ));
        assert!(!step_signals_sandbox_restart("nix: not found"));
        assert!(!step_signals_sandbox_restart("exit 127"));
    }
}
