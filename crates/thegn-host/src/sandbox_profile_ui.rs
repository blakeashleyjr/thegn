//! Shared selection and presentation helpers for worktree isolation profiles.

use anyhow::Result;
use std::path::Path;
use thegn_core::sandbox_pin::EnvSelection;
use thegn_core::store::WorkspaceStore;

pub(crate) fn selection_for(worktree: &str, repo_root: &Path) -> Result<EnvSelection> {
    let db = thegn_core::db::Db::open()?;
    db.effective_env_selection(worktree, &repo_root.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::selection_for;
    use thegn_core::config::SandboxProfile;
    use thegn_core::store::WorkspaceStore;

    #[test]
    fn sandbox_pin_no_new_launch_site_drops_the_pin() {
        let pattern = regex::Regex::new(r"EnvSelection::named\(|profile:\s*None").unwrap();
        thegn_core::test_support::ratchet::file_ratchet(
            env!("CARGO_MANIFEST_DIR"),
            "sandbox-pin-ratchet.txt",
            &["sandbox_profile_ui.rs"],
            |_, body| pattern.is_match(body),
            "Worktree launch and resolver paths must carry the persisted sandbox profile pin. \
             Migrate each call site to effective_env_selection; the allowlist records only \
             pre-existing source files whose selections are ambient-only or out of this lane's \
             ownership range.",
        );
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
        let menu = crate::menu::sandbox_profile_menu(
            SandboxProfile::Hardened,
            true,
            SandboxProfile::Hardened,
        );
        let rendered = format!("{menu:?}");
        assert!(rendered.contains("sandbox-profile"));
        assert!(rendered.contains("standard (default)"));
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
