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

/// Sanitize a subprocess-derived message for display on the loading screen:
/// strip ANSI/OSC escape sequences and other control bytes (provisioning output
/// is full of them — they corrupt width math and have triggered renderer
/// `capacity overflow`s), collapse runs of whitespace/newlines to single spaces,
/// and clamp to a sane length. Pure + unit-tested.
pub(crate) fn sanitize_detail(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(256));
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // CSI (`ESC[ … final`) / OSC (`ESC] … BEL/ST`) / other ESC seq: skip
            // the introducer and run to the terminating byte.
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    for d in chars.by_ref() {
                        if ('@'..='~').contains(&d) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    for d in chars.by_ref() {
                        if d == '\u{7}' || d == '\u{1b}' {
                            break;
                        }
                    }
                }
                _ => {
                    chars.next();
                }
            }
            continue;
        }
        // Control chars (incl. newlines/tabs) + spaces → a single space; collapse
        // runs so multi-line subprocess output reads as one tidy line.
        if c.is_control() || c == ' ' {
            if !out.ends_with(' ') {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
        if out.chars().count() >= 200 {
            out.push('…');
            break;
        }
    }
    out.trim().to_string()
}

/// The most informative line of a subprocess stderr blob: the first line whose
/// trimmed form starts with `Error:`/`error:` (anyhow/CLI convention — the
/// bridge's `Error: machine0-ssh: could not resolve VM …` otherwise drowns
/// under `thegn migrate:` warnings and a RUST_BACKTRACE), else the last
/// non-empty line, sanitized via [`sanitize_detail`]. Empty blob ⇒ `""`.
pub(crate) fn stderr_gist(stderr: &str) -> String {
    let lines = || stderr.lines().map(str::trim).filter(|l| !l.is_empty());
    let pick = lines()
        .find(|l| l.starts_with("Error:") || l.starts_with("error:"))
        .or_else(|| lines().next_back());
    pick.map(sanitize_detail).unwrap_or_default()
}

/// Which provisioning steps are ESSENTIAL — a failure aborts creation — vs
/// best-effort (warn + continue; the shell still opens and the step resolves
/// lazily in the pane). Essentials: the worktree dir, git auth, the clone.
/// Everything else (nix install, devShell/direnv warm, personal tools, dotfiles,
/// the home-parity closure, checkpoint) is best-effort, so one flaky `nix
/// develop` / unreachable cache can't kill an otherwise-usable sandbox.
pub(crate) fn step_is_fatal(step_id: &str) -> bool {
    matches!(step_id, "workspace" | "git_auth" | "clone")
}

/// Which best-effort steps are pure PRE-WARMS whose only effect is to build the
/// dev shell ahead of time — the pane rebuilds it lazily on entry, so a failure
/// is invisible to the user. Their common failure on a pooled provider microVM is
/// an OOM-kill (`exit 137`, which restarts the VM) or a timeout on a heavy Nix
/// build; painting that as a red `Failed` splash row is alarming and misleading
/// (nothing is actually broken). So these surface as a completed step with a "will
/// finish in the shell" hint instead. Distinct from the other best-effort steps
/// (dotfiles, tools, agent configs), whose failure the user DOES want to see —
/// their effect isn't reproduced lazily in the pane.
pub(crate) fn step_is_warm_only(step_id: &str) -> bool {
    matches!(step_id, "devshell" | "cache_push" | "direnv_allow")
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
    fn stderr_gist_picks_the_error_line_over_noise() {
        // The incident shape: migrate warning first, the real error buried,
        // then a useless backtrace.
        let blob = "thegn migrate: both the legacy dir and ~/.thegn exist — preferring the new path\n\
                    Error: machine0-ssh: could not resolve VM \"thegn-x\" (…); provision it first\n\
                    Stack backtrace:\n   0: <unknown>\n";
        assert!(stderr_gist(blob).starts_with("Error: machine0-ssh:"));
        // No Error: line → last non-empty line.
        assert_eq!(
            stderr_gist("warming up\nclone failed badly\n\n"),
            "clone failed badly"
        );
        // Empty → empty.
        assert_eq!(stderr_gist("  \n\n"), "");
        // ANSI stripped + long lines capped (sanitize_detail semantics).
        let long = format!("error: {}", "x".repeat(500));
        let s = stderr_gist(&long);
        assert!(s.chars().count() <= 201);
        assert!(!stderr_gist("error: \u{1b}[31mred\u{1b}[0m").contains('\u{1b}'));
    }

    #[test]
    fn sanitize_detail_strips_ansi_control_and_collapses_whitespace() {
        let raw = "Build dev shell (exit 2): \u{1b}[1m\u{1b}[32merror:\u{1b}[0m foo\n\n  bar\tbaz";
        let s = sanitize_detail(raw);
        assert!(!s.contains('\u{1b}'), "no escape bytes: {s:?}");
        assert!(
            !s.contains('\n') && !s.contains('\t'),
            "no raw control: {s:?}"
        );
        assert_eq!(s, "Build dev shell (exit 2): error: foo bar baz");
        assert_eq!(sanitize_detail("a\u{1b}]0;title\u{7}b"), "ab");
        assert!(sanitize_detail(&"x".repeat(500)).chars().count() <= 201);
    }

    #[test]
    fn warm_only_and_fatal_step_classifiers_partition_the_pipeline() {
        // Essentials abort creation; pure pre-warms de-alarm to a hint; the rest
        // keep the visible best-effort failure. The three sets must not overlap.
        for id in ["workspace", "git_auth", "clone"] {
            assert!(step_is_fatal(id), "{id} is fatal");
            assert!(!step_is_warm_only(id), "{id} is not a warm-only");
        }
        for id in ["devshell", "cache_push", "direnv_allow"] {
            assert!(step_is_warm_only(id), "{id} is a warm-only pre-warm");
            assert!(!step_is_fatal(id), "{id} is not fatal");
        }
        // A best-effort-but-visible step (dotfiles/tools) is neither — its failure
        // still surfaces because the pane doesn't reproduce it lazily.
        for id in ["dotfiles", "tools", "agents"] {
            assert!(!step_is_fatal(id) && !step_is_warm_only(id), "{id}");
        }
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
