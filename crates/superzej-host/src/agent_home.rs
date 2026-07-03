//! Personal home-config resolution for sandbox provisioning: which setup
//! commands and dotfiles ride into a fresh env (`[sandbox.home]`). Extracted
//! from `agent.rs` (pinned by the file-size ratchet); re-exported from
//! `crate::agent` so call sites are unchanged.

use std::path::Path;

use crate::agent::default_dotfiles;

/// The final personal setup commands: the inline `[sandbox.home] setup` list,
/// then `setup_script` resolved — if it names an existing HOST file, its contents
/// are inlined (so no upload is needed); otherwise it's treated as an in-sandbox
/// path and run with `sh`. Bring-your-own escape hatch (agent CLIs, internal
/// tooling, anything not a package).
pub(crate) fn resolve_setup(home: &superzej_core::config::HomeConfig) -> Vec<String> {
    let mut cmds = home.setup.clone();
    let script = home.setup_script.trim();
    if !script.is_empty() {
        let host_path = if let Some(rest) = script.strip_prefix("~/") {
            std::env::var("HOME")
                .map(|h| format!("{h}/{rest}"))
                .unwrap_or_else(|_| script.to_string())
        } else {
            script.to_string()
        };
        match std::fs::read_to_string(&host_path) {
            Ok(body) => cmds.push(body),
            Err(_) => cmds.push(format!("sh {}", superzej_core::util::sh_quote(script))),
        }
    }
    cmds
}

/// Resolve which host dotfiles to upload under the env's `ShellStrategy`, and
/// (for host-parity) the host `/nix/store` roots they reference.
///
/// - `Clean`: upload nothing (the plan drops the dotfiles step too).
/// - `Portable`/`ToolParity` with `portable_dotfiles_only` (the default): read each
///   candidate on the host and **skip** any that hard-codes absent store paths,
///   warning which file + why. Portable files (`.gitconfig`, …) still upload.
/// - `HostParity`: upload everything unfiltered and collect the store roots so the
///   provisioner can reproduce their closure before the upload.
///
/// `home_dir` is the host `$HOME` (a param so it's unit-testable with a fixture).
pub(crate) fn resolve_personal_dotfiles(
    home_dir: &Path,
    home: &superzej_core::config::HomeConfig,
    env_name: &str,
) -> (Vec<String>, Vec<String>) {
    use superzej_core::config::ShellStrategy;
    use superzej_core::envplan::{PitfallKind, scan_dotfile, store_roots_in};

    let candidates = if home.dotfiles.is_empty() {
        default_dotfiles()
    } else {
        home.dotfiles.clone()
    };
    let mut dotfiles = Vec::new();
    let mut roots: Vec<String> = Vec::new();
    for name in candidates {
        let contents = std::fs::read_to_string(home_dir.join(&name)).ok();
        match home.strategy {
            ShellStrategy::Clean => {} // nothing personal under clean
            ShellStrategy::HostParity => {
                if let Some(c) = &contents {
                    for r in store_roots_in(c) {
                        if !roots.contains(&r) {
                            roots.push(r);
                        }
                    }
                }
                dotfiles.push(name);
            }
            ShellStrategy::Portable | ShellStrategy::ToolParity => {
                if home.portable_dotfiles_only
                    && let Some(c) = &contents
                {
                    let absent: Vec<String> = scan_dotfile(&name, c, &home.tools)
                        .into_iter()
                        .filter(|p| p.kind == PitfallKind::AbsentStorePath)
                        .map(|p| p.detail)
                        .collect();
                    if !absent.is_empty() {
                        superzej_core::msg::warn(&format!(
                            "[sandbox.home] {name} references {} path(s) absent in env {env_name:?} \
                             (e.g. {}); skipping its upload (strategy=portable). Set \
                             strategy=\"host-parity\" to reproduce the closure, or make the rc \
                             portable (init tools by command name).",
                            absent.len(),
                            absent[0],
                        ));
                        continue;
                    }
                }
                dotfiles.push(name);
            }
        }
    }
    (dotfiles, roots)
}
