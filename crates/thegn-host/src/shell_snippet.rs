//! The interactive-pane login-shell snippet — split out of the pinned `agent.rs`
//! (god-file ratchet). Builds the POSIX `sh` program a container/native pane runs
//! to land the user in their real login shell *with the project toolchain loaded*.

/// The `in_oci` program string for [`crate::agent::shell_inner`]: a POSIX `sh`
/// snippet that (1) primes PATH with the nix profile dirs, (2) loads the project
/// toolchain (direnv `.envrc`, else a pure `devenv.nix`, else the flake
/// devShell), and — crucially — (3) selects and execs the login shell
/// (host-preferred → zsh → bash → sh) **from INSIDE that loaded toolchain**.
///
/// Selecting inside the toolchain is what recreates the host's login shell on a
/// remote/minimal image: the reproduced `zsh` typically lives in the flake
/// devShell (see `flake.nix`), NOT on the bare base `PATH`. Probing before the
/// devShell loaded picked `bash` and then entered the devShell as bash — the
/// host `$SHELL=zsh` preference was silently dropped. So the same first-match
/// selector runs as the `--command` of each toolchain entry, after its env is
/// applied. The host's absolute `$SHELL` path is meaningless in a container, so
/// only its basename seeds the preference; the runtime probe + `/bin/sh`
/// last-resort keep it safe when the image lacks that shell.
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
    // The first-match selector (host-preferred → zsh → bash → sh, else
    // `/bin/sh`), ending in `exec "$tgsh" -l`. It is run as the `--command` of
    // each toolchain entry below so the probe sees the entry's applied env — the
    // reproduced zsh lives in the flake devShell, not the bare base PATH, so
    // selecting before the devShell loaded picked bash and entered the devShell
    // as bash. Handing the whole entry to a tool (`direnv exec` / `devenv shell`
    // / `nix develop --command`) also keeps this POSIX-`sh` wrapper — dash on
    // Debian sprites — free of `eval "$(direnv export bash)"`, which detonated
    // the moment a provisioned sprite's flake devShell evaluated (bash-only
    // quoting in nix stdenv vars ⇒ `export: …: bad variable name` ⇒ exit 2 ⇒
    // pane crash-loop).
    let probe: String = chain
        .iter()
        .map(|s| format!("[ -z \"$tgsh\" ] && command -v {s} >/dev/null 2>&1 && tgsh={s}; "))
        .collect();
    let sel = format!("tgsh=\"\"; {probe}: \"${{tgsh:=/bin/sh}}\"; exec \"$tgsh\" -l");
    // Toolchain entries, most-specific first; each runs the selector `$sel` WITH
    // the project env loaded, so the login shell is chosen from inside it, and
    // `&& exit` closes the pane on a clean shell exit. ANY failure to LOAD the
    // toolchain (build error, missing/old tool) returns non-zero before `$sel`
    // runs and falls through to the next entry — never worse than a bare shell:
    //  - `.envrc` ⇒ `direnv exec . sh -lc "$sel"` (covers `use flake`, `use
    //    devenv`, …): direnv applies the env itself — flavor-proof, no eval.
    //    Build progress (stderr) still shows in the pane.
    //  - else `devenv.nix` with the `devenv` CLI ⇒ `devenv shell -- sh -lc "$sel"`.
    //  - else a flake ⇒ `nix develop .#$THEGN_DEVSHELL --command sh -lc "$sel"`
    //    (`sandbox` in-sandbox, else `default`), guarded on `IN_NIX_SHELL` so an
    //    already-active devShell isn't re-entered.
    // `$sel` carries a `"` (`exec "$tgsh" -l`), so it is single-quoted for the
    // `-lc` argument; the selector itself uses no single quotes.
    let devshell = "if command -v direnv >/dev/null 2>&1 && [ -e .envrc ]; then \
             direnv allow . 2>/dev/null; direnv exec . sh -lc \"$sel\" && exit; \
         fi; \
         if [ -e devenv.nix ] && command -v devenv >/dev/null 2>&1; then \
             devenv shell -- sh -lc \"$sel\" && exit; \
         fi; \
         if [ -z \"$IN_NIX_SHELL\" ] && [ -e flake.nix ] && command -v nix >/dev/null 2>&1; then \
             nix develop \".#${THEGN_DEVSHELL:-default}\" --command sh -lc \"$sel\" && exit; \
         fi; ";
    format!("{path_export}sel='{sel}'; {devshell}exec sh -lc \"$sel\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oci_snippet_selects_shell_inside_the_loaded_toolchain() {
        let s = oci_login_snippet();
        // The selector is defined ONCE into `$sel` and re-runs the first-match
        // probe (host-preferred → zsh → bash → sh, else /bin/sh) then execs the
        // login shell — so selection can happen INSIDE each toolchain entry.
        assert!(
            s.contains("sel='tgsh=\"\";")
                && s.contains("[ -z \"$tgsh\" ] && command -v zsh")
                && s.contains("${tgsh:=/bin/sh}"),
            "selector defines the first-match probe into $sel: {s}"
        );
        assert!(s.contains("command -v bash"), "bash in the chain: {s}");
        // The selector — not the toolchain wrapper — is what execs the shell.
        assert!(
            s.contains("exec \"$tgsh\" -l'"),
            "the $sel selector execs the probed shell: {s}"
        );
        // Ends by running the selector in the base env when no toolchain loaded.
        assert!(
            s.ends_with("exec sh -lc \"$sel\""),
            "base-env fallback runs the selector: {s}"
        );
        // Each toolchain entry runs the SELECTOR as its `--command`, so the shell
        // is chosen from inside the applied env (where the flake devShell's zsh
        // lives). direnv still applies the env ITSELF (`direnv exec`) — never an
        // `eval "$(direnv export bash)"` in this POSIX wrapper: dash (Debian
        // sprites' /bin/sh) chokes on bash-flavored export dumps the moment the
        // flake devShell evaluates (`export: …: bad variable name` → exit 2 →
        // pane crash-loop).
        assert!(
            s.contains("[ -e .envrc ]") && s.contains("direnv exec . sh -lc \"$sel\" && exit"),
            "direnv entry runs the selector: {s}"
        );
        assert!(
            !s.contains("eval \"$("),
            "no shell-flavored eval in the wrapper: {s}"
        );
        // Pure-devenv fallback, guarded so any failure falls through.
        assert!(
            s.contains("[ -e devenv.nix ] && command -v devenv")
                && s.contains("devenv shell -- sh -lc \"$sel\" && exit"),
            "devenv fallback runs the selector: {s}"
        );
        // Direct flake-devShell fallback when there's no .envrc: `nix develop
        // --command`, guarded on IN_NIX_SHELL so it no-ops when already active.
        assert!(
            s.contains("[ -z \"$IN_NIX_SHELL\" ]")
                && s.contains(
                    "nix develop \".#${THEGN_DEVSHELL:-default}\" --command sh -lc \"$sel\""
                ),
            "flake nix-develop fallback runs the selector: {s}"
        );
        // Entry order: direnv → devenv → flake → bare selector.
        let pos = |needle: &str| s.find(needle).unwrap();
        assert!(pos(".envrc") < pos("devenv.nix"));
        assert!(pos("devenv.nix") < pos("flake.nix"));
        assert!(pos("flake.nix") < pos("exec sh -lc \"$sel\""));
    }
}
