//! The interactive-pane login-shell snippet — split out of the pinned `agent.rs`
//! (god-file ratchet). Builds the POSIX `sh` program a container/native pane runs
//! to land the user in their real login shell *with the project toolchain loaded*.

/// The `in_oci` program string for [`crate::agent::shell_inner`]: a POSIX `sh`
/// snippet that (1) primes PATH with the nix profile dirs, (2) loads the project
/// toolchain into this shell (direnv `.envrc`, else a pure `devenv.nix`), then (3)
/// execs the first available login shell (host-preferred → zsh → bash → sh).
///
/// The host's absolute `$SHELL` path is meaningless in a container, and even the
/// basename fails if the image lacks that shell — so we probe at runtime and fall
/// back to `/bin/sh`.
pub(crate) fn oci_login_snippet() -> String {
    // Preference order: honour the host shell name if it's a known shell, then try
    // zsh/bash/fish/sh. The outer `/bin/sh -lc` already gives a POSIX context.
    let host_shell = std::env::var("SHELL").unwrap_or_default();
    let preferred = std::path::Path::new(&host_shell)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let mut chain: Vec<&str> = Vec::new();
    if matches!(preferred, "zsh" | "bash" | "fish" | "dash" | "ksh" | "mksh") {
        chain.push(preferred);
    }
    for s in &["zsh", "bash", "sh"] {
        if !chain.contains(s) {
            chain.push(s);
        }
    }
    // Emit: for s in <chain>; do command -v "$s" && exec "$s" -l; done
    let checks: String = chain
        .iter()
        .map(|s| format!("command -v {s} >/dev/null 2>&1 && exec {s} -l; "))
        .collect();
    // Put the nix profile dirs on PATH FIRST so a `nix profile install`ed shell
    // (zsh from nixpkgs) is found by `command -v` — covers single-user
    // (`~/.nix-profile`), daemon/system (Determinate `--init none`,
    // `/nix/var/nix/profiles/default`), the Determinate per-user profile
    // (`~/.local/state/nix/profile/bin`), and `~/.local/bin`. Without these the
    // checks miss the installed zsh/starship and drop to `/bin/sh`.
    let path_export = "export PATH=\"$HOME/.nix-profile/bin:/nix/var/nix/profiles/default/bin:\
         $HOME/.local/state/nix/profile/bin:$HOME/.local/bin:$PATH\"; ";
    // The login-shell probe chain, PATH-primed. Reused both bare and, for a
    // pure-devenv repo, RE-run inside `devenv shell` so the pane's real shell
    // lands inside the devenv toolchain. No single quotes inside, so it embeds
    // safely in the `sh -lc '…'` below.
    let inner = format!("{path_export}{checks}exec /bin/sh -l");
    // Load the project toolchain into THIS shell before exec'ing the login shell,
    // so the pane enters it even where the direnv rc-HOOK can't install (machine0's
    // read-only home-manager `~/.zshrc`). Three cases, most-specific first; each
    // runs after `cd` (see `open_spec`):
    //  - `.envrc` present ⇒ direnv (covers `use flake`, `use devenv`, …): eval its
    //    exports (stdout) into this shell; build progress (stderr) shows.
    //  - else a `devenv.nix` with the `devenv` CLI but NO direnv integration ⇒
    //    enter `devenv shell` directly, re-running the probe chain inside it so the
    //    user's shell keeps the devenv PATH. `&& exit` closes the pane on a clean
    //    exit; ANY failure (missing/old devenv, build error) falls through to the
    //    bare chain below — never worse than no devenv entry.
    //  - else nothing: the bare chain runs.
    let devshell = format!(
        "if command -v direnv >/dev/null 2>&1 && [ -e .envrc ]; then \
             direnv allow . 2>/dev/null; eval \"$(direnv export bash)\" || true; \
         elif [ -e devenv.nix ] && command -v devenv >/dev/null 2>&1; then \
             devenv shell -- /bin/sh -lc '{inner}' && exit; \
         fi; "
    );
    format!("{path_export}{devshell}{checks}exec /bin/sh -l")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oci_snippet_probes_chain_devshell_and_devenv_fallback() {
        let s = oci_login_snippet();
        assert!(s.contains("command -v"), "probes for shell availability");
        assert!(s.ends_with("exec /bin/sh -l"), "ends with /bin/sh fallback");
        assert!(s.contains("bash"), "bash in the chain");
        // direnv env loaded before the login shell execs.
        assert!(s.contains("[ -e .envrc ]") && s.contains("direnv export bash"));
        assert!(s.find("direnv export bash").unwrap() < s.find("exec /bin/sh -l").unwrap());
        // Pure-devenv fallback, guarded so any failure falls through.
        assert!(
            s.contains("elif [ -e devenv.nix ] && command -v devenv")
                && s.contains("devenv shell -- /bin/sh -lc")
                && s.contains("&& exit"),
            "devenv fallback: {s}"
        );
    }
}
