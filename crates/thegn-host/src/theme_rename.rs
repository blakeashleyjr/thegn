use std::path::Path;

use sha2::{Digest, Sha256};

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RenameOutcome {
    pub(crate) old_name: String,
    pub(crate) new_name: String,
    pub(crate) old_removed: bool,
}

pub(crate) fn rename_theme(
    dir: &Path,
    old: &str,
    new: &str,
    active: Option<&str>,
) -> Result<RenameOutcome, String> {
    let old_slug = thegn_core::util::slugify(old);
    let new_slug = thegn_core::util::slugify(new);
    if thegn_core::theme::PRESETS
        .iter()
        .any(|preset| thegn_core::util::slugify(preset) == old_slug)
    {
        return Err(format!(
            "theme `{old}` is a built-in preset and cannot be renamed (nothing was written)"
        ));
    }

    let (themes, _warnings, hashes) = crate::theme_store::scan_dir_hashed(dir, active);
    let Some(found) = themes
        .iter()
        .find(|theme| thegn_core::util::slugify(&theme.meta.name) == old_slug)
    else {
        return Err(format!(
            "unknown user theme `{old}`; run `thegn theme list` (nothing was written)"
        ));
    };

    if active.map(thegn_core::util::slugify) == Some(old_slug.clone()) {
        return Err(format!(
            "theme `{old}` is the active theme; select another with `thegn theme set <name>` first (nothing was written)"
        ));
    }
    if new_slug == old_slug {
        return Err(format!(
            "`{new}` has the same storage identity as `{old}`; nothing to rename (nothing was written)"
        ));
    }

    let mut renamed = found.clone();
    renamed.meta.name = new.to_owned();
    crate::theme_store::write_theme(dir, &renamed).map_err(with_nothing_written_note)?;

    let expected = hashes.get(&old_slug).copied();
    let old_path = dir.join(format!("{old_slug}.toml"));
    let old_removed = expected.is_some_and(|hash| remove_if_unchanged(&old_path, hash));
    if !old_removed {
        tracing::debug!(
            target: "thegn::theme",
            path = %old_path.display(),
            "old theme file changed or could not be removed after rename"
        );
    }

    Ok(RenameOutcome {
        old_name: found.meta.name.clone(),
        new_name: renamed.meta.name,
        old_removed,
    })
}

fn with_nothing_written_note(error: String) -> String {
    if error.contains("nothing was written") {
        error
    } else {
        format!("{error} (nothing was written)")
    }
}

fn remove_if_unchanged(path: &Path, expected: [u8; 32]) -> bool {
    let Ok(bytes) = crate::theme_store::read_bounded(path) else {
        return false;
    };
    let actual: [u8; 32] = Sha256::digest(&bytes).into();
    if actual != expected {
        return false;
    }
    std::fs::remove_file(path).is_ok()
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};
    use thegn_core::theme_user::UserTheme;

    use super::{remove_if_unchanged, rename_theme, with_nothing_written_note};

    fn private_temp_dir() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        thegn_core::fsperm::ensure_private_dir(temp.path()).unwrap();
        temp
    }

    fn theme(name: &str) -> UserTheme {
        UserTheme::from_palette(name, &thegn_core::config::Config::default().palette())
    }

    fn save(dir: &std::path::Path, name: &str) {
        crate::theme_store::write_theme(dir, &theme(name)).unwrap();
    }

    #[test]
    fn rename_moves_the_theme_to_the_new_slug() {
        let temp = private_temp_dir();
        let dir = temp.path();
        let original = theme("Old Name");
        crate::theme_store::write_theme(dir, &original).unwrap();

        let outcome = rename_theme(dir, "Old Name", "New Name", Some("prism")).unwrap();

        assert!(outcome.old_removed);
        assert_eq!(outcome.old_name, "Old Name");
        assert_eq!(outcome.new_name, "New Name");
        assert!(!dir.join("old-name.toml").exists());
        assert!(dir.join("new-name.toml").exists());
        let (themes, _) = crate::theme_store::scan_dir(dir, None);
        assert_eq!(themes.len(), 1);
        assert_eq!(themes[0].meta.name, "New Name");
        assert_eq!(themes[0].palette().unwrap(), original.palette().unwrap());
    }

    #[test]
    fn rename_refuses_the_active_theme() {
        for active in ["Old Name", "old-name"] {
            let temp = private_temp_dir();
            let dir = temp.path();
            save(dir, "Old Name");
            let old_bytes = std::fs::read(dir.join("old-name.toml")).unwrap();

            let error = rename_theme(dir, "Old Name", "New Name", Some(active)).unwrap_err();

            assert!(error.contains("active theme"));
            assert!(error.ends_with("(nothing was written)"));
            assert_eq!(std::fs::read(dir.join("old-name.toml")).unwrap(), old_bytes);
            assert!(!dir.join("new-name.toml").exists());
        }
    }

    #[test]
    fn rename_refuses_a_builtin_old_or_new_name() {
        let temp = private_temp_dir();
        let dir = temp.path();
        let builtin = thegn_core::theme::PRESETS[0];
        let old_error = rename_theme(dir, builtin, "New Name", None).unwrap_err();
        assert!(old_error.contains("built-in preset"));
        assert!(old_error.ends_with("(nothing was written)"));

        save(dir, "Old Name");
        let old_bytes = std::fs::read(dir.join("old-name.toml")).unwrap();
        let new_error = rename_theme(dir, "Old Name", builtin, None).unwrap_err();
        assert!(new_error.contains("reserved by a built-in preset"));
        assert!(new_error.ends_with("(nothing was written)"));
        assert_eq!(std::fs::read(dir.join("old-name.toml")).unwrap(), old_bytes);
        assert!(
            !dir.join(format!("{}.toml", thegn_core::util::slugify(builtin)))
                .exists()
        );
    }

    #[test]
    fn rename_refuses_unknown_old_theme() {
        let temp = private_temp_dir();
        let error = rename_theme(temp.path(), "Missing", "New Name", None).unwrap_err();
        assert!(error.contains("unknown user theme `Missing`"));
        assert!(error.ends_with("(nothing was written)"));
        assert!(!temp.path().join("new-name.toml").exists());
    }

    #[test]
    fn rename_refuses_a_name_another_theme_claims() {
        let temp = private_temp_dir();
        let dir = temp.path();
        save(dir, "Old Name");
        save(dir, "Taken");
        let old_bytes = std::fs::read(dir.join("old-name.toml")).unwrap();
        let taken_bytes = std::fs::read(dir.join("taken.toml")).unwrap();

        let error = rename_theme(dir, "Old Name", "Taken", None).unwrap_err();

        assert!(error.contains("already exists on disk"), "{error}");
        assert!(error.ends_with("(nothing was written)"));
        assert_eq!(std::fs::read(dir.join("old-name.toml")).unwrap(), old_bytes);
        assert_eq!(std::fs::read(dir.join("taken.toml")).unwrap(), taken_bytes);
    }

    #[test]
    fn rename_refuses_a_same_slug_new_name() {
        let temp = private_temp_dir();
        save(temp.path(), "Old Name");
        let old_bytes = std::fs::read(temp.path().join("old-name.toml")).unwrap();

        let error = rename_theme(temp.path(), "Old Name", "old name", None).unwrap_err();

        assert!(error.contains("same storage identity"));
        assert!(error.ends_with("(nothing was written)"));
        assert_eq!(
            std::fs::read(temp.path().join("old-name.toml")).unwrap(),
            old_bytes
        );
        assert!(!temp.path().join("old name.toml").exists());
    }

    #[test]
    fn rename_refuses_an_invalid_new_name() {
        for new in ["", "!!!"] {
            let temp = private_temp_dir();
            save(temp.path(), "Old Name");
            let old_bytes = std::fs::read(temp.path().join("old-name.toml")).unwrap();

            let error = rename_theme(temp.path(), "Old Name", new, None).unwrap_err();

            assert!(error.ends_with("(nothing was written)"));
            assert_eq!(
                std::fs::read(temp.path().join("old-name.toml")).unwrap(),
                old_bytes
            );
            assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
        }
    }

    #[test]
    fn write_error_with_existing_nothing_written_note_is_not_duplicated() {
        let error = "cannot verify theme identities in /themes: theme directory could not be read: denied; nothing was written".to_owned();

        let wrapped = with_nothing_written_note(error.clone());

        assert_eq!(wrapped, error);
        assert_eq!(wrapped.matches("nothing was written").count(), 1);
    }

    #[test]
    fn rename_leaves_a_changed_old_file_in_place() {
        let temp = private_temp_dir();
        let path = temp.path().join("old-name.toml");
        std::fs::write(&path, b"original").unwrap();
        let expected: [u8; 32] = Sha256::digest(b"original").into();
        assert!(remove_if_unchanged(&path, expected));
        assert!(!path.exists());

        std::fs::write(&path, b"original").unwrap();
        let expected: [u8; 32] = Sha256::digest(b"original").into();
        std::fs::write(&path, b"changed").unwrap();
        assert!(!remove_if_unchanged(&path, expected));
        assert_eq!(std::fs::read(&path).unwrap(), b"changed");
        assert!(!remove_if_unchanged(
            &temp.path().join("missing.toml"),
            expected
        ));
    }
}
