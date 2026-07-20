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
    // Put the nix profile dirs on PATH FIRST so a `nix profile install`ed shell
    // (zsh from nixpkgs) is found by `command -v` — covers single-user
    // (`~/.nix-profile`), daemon/system (Determinate `--init none`,
    // `/nix/var/nix/profiles/default`), the Determinate per-user profile
    // (`~/.local/state/nix/profile/bin`), and `~/.local/bin`. Without these the
    // checks miss the installed zsh/starship and drop to `/bin/sh`.
    let path_export = "export PATH=\"$HOME/.nix-profile/bin:/nix/var/nix/profiles/default/bin:\
         $HOME/.local/state/nix/profile/bin:$HOME/.local/bin:$PATH\"; ";
    // Probe the login shell ONCE, first-match (host-preferred → zsh → bash →
    // sh), into `$tgsh` — the toolchain entries below hand the ENTIRE entry to a
    // tool (`direnv exec` / `devenv shell` / `nix develop --command`) instead of
    // eval'ing shell-flavored export dumps in THIS wrapper. This wrapper is
    // POSIX `/bin/sh` — dash on Debian sprites — and `eval "$(direnv export
    // bash)"` there detonated the moment a provisioned sprite's flake devShell
    // actually evaluated (bash-only quoting in nix stdenv vars ⇒ `export: …:
    // bad variable name` ⇒ exit 2 ⇒ the pane crash-loop).
    let probe: String = chain
        .iter()
        .map(|s| format!("[ -z \"$tgsh\" ] && command -v {s} >/dev/null 2>&1 && tgsh={s}; "))
        .collect();
    let probe = format!("tgsh=\"\"; {probe}: \"${{tgsh:=/bin/sh}}\"; ");
    // Toolchain entries, most-specific first; each hands the pane to `$tgsh -l`
    // WITH the project env loaded, and `&& exit` closes the pane on a clean
    // shell exit. ANY failure (build error, missing/old tool) falls through to
    // the next entry — never worse than a bare shell:
    //  - `.envrc` ⇒ `direnv exec . $tgsh -l` (covers `use flake`, `use devenv`,
    //    …): direnv applies the env itself — flavor-proof, no eval. Build
    //    progress (stderr) still shows in the pane.
    //  - else `devenv.nix` with the `devenv` CLI ⇒ enter `devenv shell` directly.
    //  - else a flake ⇒ `nix develop .#$THEGN_DEVSHELL --command $tgsh -l`
    //    (`sandbox` in-sandbox, else `default`), guarded on `IN_NIX_SHELL` so an
    //    already-active devShell isn't re-entered.
    let devshell = "if command -v direnv >/dev/null 2>&1 && [ -e .envrc ]; then \
             direnv allow . 2>/dev/null; direnv exec . \"$tgsh\" -l && exit; \
         fi; \
         if [ -e devenv.nix ] && command -v devenv >/dev/null 2>&1; then \
             devenv shell -- \"$tgsh\" -l && exit; \
         fi; \
         if [ -z \"$IN_NIX_SHELL\" ] && [ -e flake.nix ] && command -v nix >/dev/null 2>&1; then \
             nix develop \".#${THEGN_DEVSHELL:-default}\" --command \"$tgsh\" -l && exit; \
         fi; ";
    format!("{path_export}{probe}{devshell}exec \"$tgsh\" -l")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oci_snippet_probes_chain_devshell_and_devenv_fallback() {
        let s = oci_login_snippet();
        // First-match probe into $tgsh, /bin/sh as the last resort.
        assert!(
            s.contains("[ -z \"$tgsh\" ] && command -v zsh") && s.contains("${tgsh:=/bin/sh}"),
            "first-match shell probe: {s}"
        );
        assert!(s.contains("bash"), "bash in the chain");
        assert!(s.ends_with("exec \"$tgsh\" -l"), "ends with the probed shell");
        // direnv hands the pane to the shell ITSELF (`direnv exec`) — never an
        // `eval "$(direnv export bash)"` in this POSIX wrapper: dash (Debian
        // sprites' /bin/sh) chokes on bash-flavored export dumps the moment the
        // flake devShell evaluates (`export: …: bad variable name` → exit 2 →
        // pane crash-loop).
        assert!(
            s.contains("[ -e .envrc ]") && s.contains("direnv exec . \"$tgsh\" -l && exit"),
            "direnv entry via direnv exec: {s}"
        );
        assert!(!s.contains("eval \"$("), "no shell-flavored eval in the wrapper: {s}");
        // Pure-devenv fallback, guarded so any failure falls through.
        assert!(
            s.contains("[ -e devenv.nix ] && command -v devenv")
                && s.contains("devenv shell -- \"$tgsh\" -l && exit"),
            "devenv fallback: {s}"
        );
        // Direct flake-devShell fallback when there's no .envrc: `nix develop
        // --command`, guarded on IN_NIX_SHELL so it no-ops when already active.
        assert!(
            s.contains("[ -z \"$IN_NIX_SHELL\" ]")
                && s.contains("nix develop \".#${THEGN_DEVSHELL:-default}\" --command \"$tgsh\" -l"),
            "flake nix-develop fallback: {s}"
        );
        // Entry order: direnv → devenv → flake → bare exec.
        let pos = |needle: &str| s.find(needle).unwrap();
        assert!(pos(".envrc") < pos("devenv.nix"));
        assert!(pos("devenv.nix") < pos("flake.nix"));
        assert!(pos("flake.nix") < pos("exec \"$tgsh\" -l"));
    }
}
