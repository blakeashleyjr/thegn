//! Shared selection and presentation helpers for worktree isolation profiles.

use anyhow::Result;
use std::path::Path;
use thegn_core::sandbox_pin::EnvSelection;
use thegn_core::store::WorkspaceStore;

pub(crate) fn selection_for(worktree: &str, repo_root: &Path) -> Result<EnvSelection> {
    let db = thegn_core::db::Db::open()?;
    db.effective_env_selection(worktree, &repo_root.to_string_lossy())
}

pub(crate) fn apply_menu_choice(
    arg: &str,
    worktree: String,
    branch: String,
    refresh_tx: tokio::sync::mpsc::UnboundedSender<crate::hydrate::RefreshKind>,
) -> Result<String> {
    let profile = if arg == "clear" {
        None
    } else {
        Some(thegn_core::sandbox_pin::parse_pin(arg)?)
    };
    if let Some(profile) = profile
        && profile == thegn_core::config::SandboxProfile::Open
    {
        anyhow::bail!("only standard and sealed can be pinned for a worktree");
    }
    let saved_worktree = worktree.clone();
    crate::db_task::persist_critical(move |db| {
        if let Err(error) = db.set_worktree_sandbox_profile(&saved_worktree, profile) {
            tracing::warn!(target: "thegn::sandbox", worktree = %saved_worktree, %error, "could not persist worktree isolation profile");
            crate::db_task::alert(&format!(
                "Could not save isolation profile for {saved_worktree}: {error}"
            ));
            return;
        }
        if let Err(error) = refresh_tx.send(crate::hydrate::RefreshKind::Model) {
            tracing::debug!(target: "thegn::hydrate", %error, "model refresh request failed after isolation profile save");
        }
    });
    let value = profile.map_or_else(
        || "inherit".to_string(),
        |profile| profile.as_str().to_string(),
    );
    Ok(format!(
        "Saving isolation profile for {branch}: {value}. Applies to new panes; restart open panes."
    ))
}

#[cfg(test)]
mod tests {
    use super::selection_for;
    use thegn_core::config::SandboxProfile;
    use thegn_core::store::WorkspaceStore;

    #[test]
    fn sandbox_pin_no_launch_site_drops_the_pin() {
        use std::path::{Path, PathBuf};

        fn source_files(root: &Path, out: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(root).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    source_files(&path, out);
                } else if path.extension().is_some_and(|ext| ext == "rs")
                    && !path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .ends_with("_tests.rs")
                {
                    out.push(path);
                }
            }
        }

        fn matching_close(source: &str, open: usize) -> Option<usize> {
            let mut depth = 1_usize;
            let mut quoted = false;
            let mut escaped = false;
            for (offset, ch) in source[open..].char_indices() {
                if quoted {
                    if escaped {
                        escaped = false;
                    } else if ch == '\\' {
                        escaped = true;
                    } else if ch == '"' {
                        quoted = false;
                    }
                    continue;
                }
                match ch {
                    '"' => quoted = true,
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(open + offset);
                        }
                    }
                    _ => {}
                }
            }
            None
        }

        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        source_files(&manifest.join("src"), &mut files);
        let mut allowed = std::collections::HashMap::new();
        for line in std::fs::read_to_string(manifest.join("../../test/sandbox-pin-ratchet.txt"))
            .unwrap()
            .lines()
            .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
        {
            let (site, reason) = line
                .split_once("  ")
                .expect("call site and reason are separated");
            assert!(
                site.rsplit_once(':')
                    .and_then(|(_, line)| line.parse::<usize>().ok())
                    .is_some(),
                "allowlist entries must identify an exact source line: {site}"
            );
            assert!(
                !reason.trim().is_empty(),
                "every allowlist entry needs a reason: {line}"
            );
            assert!(
                allowed.insert(site.to_string(), false).is_none(),
                "duplicate site: {site}"
            );
        }

        let resolve = regex::Regex::new(r"resolve_env(?:_with)?\s*\(").unwrap();
        let dropped =
            regex::Regex::new(r"EnvSelection::(?:none|named)\s*\(|profile\s*:\s*None").unwrap();
        let mut violations = Vec::new();
        for path in files {
            let source = std::fs::read_to_string(&path).unwrap();
            let text = path
                .strip_prefix(manifest.join("src"))
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            for call in resolve.find_iter(&source) {
                let open = call.end() - 1;
                let Some(close) = matching_close(&source, open + 1) else {
                    continue;
                };
                let call_source = &source[open + 1..close];
                if !dropped.is_match(call_source) {
                    continue;
                }
                let test_module = source[..call.start()].rfind("mod tests");
                let test_attr = source[..call.start()].rfind("#[cfg(test)]");
                if test_module
                    .zip(test_attr)
                    .is_some_and(|(module, attr)| attr < module)
                {
                    continue;
                }
                let line = source[..call.start()]
                    .bytes()
                    .filter(|byte| *byte == b'\n')
                    .count()
                    + 1;
                let site = format!("{text}:{line}");
                if let Some(used) = allowed.get_mut(&site) {
                    *used = true;
                } else {
                    violations.push(site);
                }
            }
        }
        let unused = allowed
            .iter()
            .filter(|(_, used)| !**used)
            .map(|(site, _)| site.clone())
            .collect::<Vec<_>>();
        assert!(
            violations.is_empty(),
            "unclassified pin-dropping resolver calls: {violations:#?}"
        );
        assert!(unused.is_empty(), "stale call-site reasons: {unused:#?}");
    }

    #[test]
    fn sandbox_profile_selection_for_reads_the_stored_pin() {
        let _state = crate::testenv::IsolatedState::new();
        let root = tempfile::tempdir().unwrap();
        let db = thegn_core::db::Db::open().unwrap();
        db.set_worktree_sandbox_profile(
            root.path().to_string_lossy().as_ref(),
            Some(SandboxProfile::Sealed),
        )
        .unwrap();
        let selection = selection_for(root.path().to_string_lossy().as_ref(), root.path()).unwrap();
        assert_eq!(selection.profile, Some(SandboxProfile::Sealed));
    }

    #[test]
    fn sandbox_profile_menu_has_unique_standard_sealed_and_clear_choices() {
        let menu = crate::menu::sandbox_profile_menu(SandboxProfile::Hardened, true);
        let rendered = format!("{menu:?}");
        assert!(rendered.contains("sandbox-profile"));
        assert!(rendered.contains("standard"));
        assert!(!rendered.contains("(default)"));
        assert!(rendered.contains("sealed"));
        assert!(rendered.contains("clear"));
        assert!(rendered.contains("Isolation profile"));
    }

    #[test]
    fn sandbox_profile_wizard_choice_reaches_trusted_resolution_before_registration() {
        let _state = crate::testenv::IsolatedState::new();
        let root = tempfile::tempdir().unwrap();
        let loc = thegn_core::remote::GitLoc::Local(root.path().to_path_buf());
        let selection = EnvSelection::none().with_profile(SandboxProfile::Sealed);
        let trusted = crate::handlers::repo_trust::resolve_env_trusted(
            &thegn_core::config::Config::default(),
            root.path(),
            &loc,
            root.path().to_string_lossy().as_ref(),
            &selection,
        )
        .unwrap();
        assert_eq!(trusted.environment.sandbox.profile, SandboxProfile::Sealed);
    }
}
