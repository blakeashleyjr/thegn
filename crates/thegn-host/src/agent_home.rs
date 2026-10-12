//! Personal home-config resolution for sandbox provisioning: which setup
//! commands and dotfiles ride into a fresh env (`[sandbox.home]`). Extracted
//! from `agent.rs` (kept flat); re-exported from
//! `crate::agent` so call sites are unchanged.

use std::path::Path;

use crate::agent::default_dotfiles;

/// The final personal setup commands: the inline `[sandbox.home] setup` list,
/// then `setup_script` resolved — if it names an existing HOST file, its contents
/// are inlined (so no upload is needed); otherwise it's treated as an in-sandbox
/// path and run with `sh`. Bring-your-own escape hatch (agent CLIs, internal
/// tooling, anything not a package).
pub(crate) fn resolve_setup(home: &thegn_core::config::HomeConfig) -> Vec<String> {
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
            Err(_) => cmds.push(format!("sh {}", thegn_core::util::sh_quote(script))),
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
    home: &thegn_core::config::HomeConfig,
    env_name: &str,
) -> (Vec<String>, Vec<String>) {
    use thegn_core::config::ShellStrategy;
    use thegn_core::envplan::{PitfallKind, scan_dotfile, store_roots_in};

    let candidates = if home.dotfiles.is_empty() {
        default_dotfiles()
    } else {
        home.dotfiles.clone()
    };
    let mut dotfiles = Vec::new();
    let mut roots: Vec<String> = Vec::new();
    for name in candidates {
        if !is_safe_dotfile_name(&name) {
            thegn_core::msg::warn(&format!(
                "[sandbox.home] skipping unsafe dotfile path {name:?}"
            ));
            continue;
        }
        let source = home_dir.join(&name);
        // Dotfiles are deliberately not followed through symlinks. Only a
        // regular file directly under HOME can be selected for staging.
        match std::fs::symlink_metadata(&source) {
            Ok(metadata) if metadata.file_type().is_file() => {}
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {} // Keep it selected; the staging read will report the real failure.
        }
        let contents = std::fs::read_to_string(&source).ok(); // best-effort: scan failure leaves upload selected; staging reports read errors
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
                        thegn_core::msg::warn(&format!(
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

/// Dotfile configuration names represent one item directly under HOME, never
/// a path expression. Reject separators and control characters before any
/// filesystem join so configuration cannot escape the selected home.
fn is_safe_dotfile_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.chars().any(char::is_control)
        && Path::new(name).components().count() == 1
        && Path::new(name)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_dotfiles_must_be_single_safe_components() {
        use thegn_core::config::{HomeConfig, ShellStrategy};

        let home_dir = tempfile::tempdir().unwrap();
        std::fs::write(home_dir.path().join(".gitconfig"), "[user]\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            home_dir.path().join(".gitconfig"),
            home_dir.path().join(".zshrc"),
        )
        .unwrap();
        let config = HomeConfig {
            strategy: ShellStrategy::HostParity,
            dotfiles: vec![
                ".gitconfig".into(),
                "../outside".into(),
                "/tmp/absolute".into(),
                "nested/.zshrc".into(),
                "..".into(),
                "bad\nname".into(),
                "nested\\file".into(),
                ".zshrc".into(),
            ],
            ..HomeConfig::default()
        };

        let (files, roots) = resolve_personal_dotfiles(home_dir.path(), &config, "test");
        assert_eq!(files, vec![".gitconfig"]);
        assert!(roots.is_empty());
    }
}
