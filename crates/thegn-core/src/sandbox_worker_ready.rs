//! Pure classification of whether a stage worker's composed sandbox is ready.
//!
//! This module performs no I/O. The caller supplies the facts it probed about
//! the host, sandbox mounts, callback binary, and provider home.

use std::path::{Component, Path};

use crate::sandbox::{Backend, Mount};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallbackBinary {
    Present,
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerReadiness {
    Ready,
    ContainmentAbsent,
    UnsupportedPlatform,
    UnsupportedBackend,
    CannotWriteWorktree,
    CannotWriteGitMetadata,
    CallbackUnavailable,
    ProviderHomeUnavailable,
}

impl WorkerReadiness {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::ContainmentAbsent => "containment-absent",
            Self::UnsupportedPlatform => "unsupported-platform",
            Self::UnsupportedBackend => "unsupported-backend",
            Self::CannotWriteWorktree => "cannot-write-worktree",
            Self::CannotWriteGitMetadata => "cannot-write-git-metadata",
            Self::CallbackUnavailable => "callback-unavailable",
            Self::ProviderHomeUnavailable => "provider-home-unavailable",
        }
    }

    pub fn is_ready(self) -> bool {
        self == Self::Ready
    }
}

pub struct WorkerFacts<'a> {
    pub linux_host: bool,
    pub backend: Backend,
    pub mounts: &'a [Mount],
    pub worktree: &'a Path,
    pub git_common_dir: Option<&'a Path>,
    pub callback: CallbackBinary,
    pub provider_home: Option<&'a Path>,
    pub provider_home_exists: bool,
}

pub fn classify_worker(facts: &WorkerFacts<'_>) -> WorkerReadiness {
    if !facts.linux_host {
        return WorkerReadiness::UnsupportedPlatform;
    }
    if facts.backend == Backend::None {
        return WorkerReadiness::ContainmentAbsent;
    }
    if !matches!(
        facts.backend,
        Backend::Bwrap | Backend::Podman | Backend::PodmanRootful | Backend::Docker | Backend::Smol
    ) {
        return WorkerReadiness::UnsupportedBackend;
    }
    if !writable(facts.mounts, facts.worktree) {
        return WorkerReadiness::CannotWriteWorktree;
    }
    if facts
        .git_common_dir
        .is_some_and(|git_common_dir| !writable(facts.mounts, git_common_dir))
    {
        return WorkerReadiness::CannotWriteGitMetadata;
    }
    if facts.callback == CallbackBinary::Missing {
        return WorkerReadiness::CallbackUnavailable;
    }
    if facts.provider_home.is_some_and(|provider_home| {
        !facts.provider_home_exists || !writable(facts.mounts, provider_home)
    }) {
        return WorkerReadiness::ProviderHomeUnavailable;
    }
    WorkerReadiness::Ready
}

/// Absolute credential-home directories an agent's expanded env relocates.
/// Values are `~`-expanded, kept in env order, and deduplicated. Relative or
/// unset values do not relocate a home. This mirrors the host launch's
/// `provider_home_mounts` selection rule.
pub fn relocated_provider_homes(
    expanded_env: &[(String, String)],
    home_vars: &[&str],
) -> Vec<std::path::PathBuf> {
    let mut homes = Vec::new();
    for (key, value) in expanded_env {
        if home_vars.contains(&key.as_str()) && Path::new(value).is_absolute() {
            let home = std::path::PathBuf::from(crate::util::expand_tilde(value));
            if !homes.contains(&home) {
                homes.push(home);
            }
        }
    }
    homes
}

/// Add the path-preserving read-write mount used by a launch for a relocated
/// credential home. Doctor's remaining check is whether the home exists as a
/// directory; this mount mirrors the launch carve and makes the path writable.
pub fn with_provider_home_mount(mounts: &[Mount], home: &Path) -> Vec<Mount> {
    let text = home.to_string_lossy().into_owned();
    let mut all = mounts.to_vec();
    all.push(Mount {
        host: text.clone(),
        dest: text,
        ro: false,
        cache: false,
    });
    all
}

fn writable(mounts: &[Mount], path: &Path) -> bool {
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
        || mounts.iter().any(|mount| {
            Path::new(&mount.dest)
                .components()
                .any(|component| matches!(component, Component::ParentDir))
        })
    {
        return false;
    }

    mounts
        .iter()
        .enumerate()
        .filter(|(_, mount)| path.starts_with(Path::new(&mount.dest)))
        .max_by_key(|(index, mount)| (Path::new(&mount.dest).components().count(), *index))
        .is_some_and(|(_, mount)| !mount.ro)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::sandbox::{Backend, Mount};

    use super::{
        CallbackBinary, WorkerFacts, WorkerReadiness, classify_worker, relocated_provider_homes,
        with_provider_home_mount, writable,
    };

    fn m(dest: &str, ro: bool) -> Mount {
        Mount {
            host: dest.to_owned(),
            dest: dest.to_owned(),
            ro,
            cache: false,
        }
    }

    fn facts<'a>(mounts: &'a [Mount], worktree: &'a Path) -> WorkerFacts<'a> {
        WorkerFacts {
            linux_host: true,
            backend: Backend::Bwrap,
            mounts,
            worktree,
            git_common_dir: None,
            callback: CallbackBinary::Present,
            provider_home: None,
            provider_home_exists: true,
        }
    }

    #[test]
    fn classify_ready_for_a_linked_worktree_with_rw_worktree_and_git_dir() {
        let mounts = [m("/work/tree", false), m("/git/common", false)];
        let worktree = Path::new("/work/tree");
        let facts = WorkerFacts {
            git_common_dir: Some(Path::new("/git/common")),
            ..facts(&mounts, worktree)
        };

        assert_eq!(classify_worker(&facts), WorkerReadiness::Ready);
    }

    #[test]
    fn classify_reports_unsupported_platform_before_anything_else() {
        let mounts = [];
        let facts = WorkerFacts {
            linux_host: false,
            backend: Backend::None,
            ..facts(&mounts, Path::new("/missing"))
        };

        assert_eq!(
            classify_worker(&facts),
            WorkerReadiness::UnsupportedPlatform
        );
    }

    #[test]
    fn classify_reports_containment_absent_for_backend_none() {
        let mounts = [];
        let facts = WorkerFacts {
            backend: Backend::None,
            callback: CallbackBinary::Missing,
            provider_home: Some(Path::new("/provider/home")),
            provider_home_exists: false,
            ..facts(&mounts, Path::new("/missing"))
        };

        assert_eq!(classify_worker(&facts), WorkerReadiness::ContainmentAbsent);
    }

    #[test]
    fn classify_reports_unsupported_backend_for_systemd_and_apple() {
        let mounts = [];
        for backend in [Backend::Systemd, Backend::Apple] {
            let facts = WorkerFacts {
                backend,
                ..facts(&mounts, Path::new("/work/tree"))
            };

            assert_eq!(classify_worker(&facts), WorkerReadiness::UnsupportedBackend);
            assert!(!classify_worker(&facts).is_ready());
        }
    }

    #[test]
    fn classify_bwrap_and_oci_are_both_evaluated_by_mounts() {
        let mounts = [m("/work/tree", false)];
        for backend in [Backend::Bwrap, Backend::Podman] {
            let facts = WorkerFacts {
                backend,
                ..facts(&mounts, Path::new("/work/tree"))
            };

            assert_eq!(classify_worker(&facts), WorkerReadiness::Ready);
        }
    }

    #[test]
    fn classify_cannot_write_worktree_when_mount_is_ro_or_missing() {
        let worktree = Path::new("/work/tree");
        for mounts in [vec![m("/work/tree", true)], vec![m("/work/other", false)]] {
            assert_eq!(
                classify_worker(&facts(&mounts, worktree)),
                WorkerReadiness::CannotWriteWorktree
            );
        }

        let prefix_mount = [m("/work/tree", false)];
        assert!(!writable(&prefix_mount, Path::new("/work/tree2/subdir")));

        let outside_worktree = facts(&prefix_mount, Path::new("/work/tree2"));
        assert_eq!(
            classify_worker(&outside_worktree),
            WorkerReadiness::CannotWriteWorktree
        );
    }

    #[test]
    fn writable_rejects_parent_dir_components_in_paths_and_mount_destinations() {
        let mounts = [m("/work/tree", false)];
        assert!(!writable(&mounts, Path::new("/work/tree/../secret")));

        let parent_dir_mount = [m("/work/tree/..", false)];
        assert!(!writable(&parent_dir_mount, Path::new("/work/secret")));

        let mixed_mounts = [m("/work/tree", false), m("/outside/../secret", false)];
        assert!(!writable(&mixed_mounts, Path::new("/work/tree")));
    }

    #[test]
    fn classify_cannot_write_git_metadata_when_the_common_dir_is_ro_or_unmounted() {
        let worktree = Path::new("/work/tree");
        for mounts in [
            vec![m("/work/tree", false), m("/git/common", true)],
            vec![m("/work/tree", false)],
        ] {
            let facts = WorkerFacts {
                git_common_dir: Some(Path::new("/git/common")),
                ..facts(&mounts, worktree)
            };

            assert_eq!(
                classify_worker(&facts),
                WorkerReadiness::CannotWriteGitMetadata
            );
        }
    }

    #[test]
    fn classify_config_pin_does_not_make_git_metadata_unwritable() {
        let mounts = [
            m("/work/tree", false),
            m("/git/common", false),
            m("/git/common/config", true),
        ];
        let facts = WorkerFacts {
            git_common_dir: Some(Path::new("/git/common")),
            ..facts(&mounts, Path::new("/work/tree"))
        };

        assert_eq!(classify_worker(&facts), WorkerReadiness::Ready);
        assert!(!writable(&mounts, Path::new("/git/common/config")));
    }

    #[test]
    fn classify_most_specific_mount_wins_regardless_of_order() {
        let child_before_parent = [m("/work/tree/cache", true), m("/work/tree", false)];
        assert!(!writable(
            &child_before_parent,
            Path::new("/work/tree/cache")
        ));
        assert!(writable(&child_before_parent, Path::new("/work/tree/src")));

        let parent_before_child = [m("/work/tree", false), m("/work/tree/cache", true)];
        assert!(!writable(
            &parent_before_child,
            Path::new("/work/tree/cache")
        ));
    }

    #[test]
    fn classify_callback_missing_is_unavailable() {
        let mounts = [m("/work/tree", false)];
        let facts = WorkerFacts {
            callback: CallbackBinary::Missing,
            ..facts(&mounts, Path::new("/work/tree"))
        };

        assert_eq!(
            classify_worker(&facts),
            WorkerReadiness::CallbackUnavailable
        );
    }

    #[test]
    fn classify_provider_home_missing_or_unwritable() {
        let worktree = Path::new("/work/tree");
        let provider_home = Path::new("/provider/home");
        let mounts = [m("/work/tree", false), m("/provider/home", false)];
        let missing = WorkerFacts {
            provider_home: Some(provider_home),
            provider_home_exists: false,
            ..facts(&mounts, worktree)
        };
        let unwritable_mounts = [m("/work/tree", false), m("/provider/home", true)];
        let unwritable = WorkerFacts {
            provider_home: Some(provider_home),
            ..facts(&unwritable_mounts, worktree)
        };

        assert_eq!(
            classify_worker(&missing),
            WorkerReadiness::ProviderHomeUnavailable
        );
        assert_eq!(
            classify_worker(&unwritable),
            WorkerReadiness::ProviderHomeUnavailable
        );
    }

    #[test]
    fn relocated_provider_homes_keeps_absolute_home_vars_in_order_without_duplicates() {
        let env = vec![
            ("CODEX_HOME".to_owned(), "/h/codex".to_owned()),
            ("OTHER".to_owned(), "/x".to_owned()),
            ("CLAUDE_CONFIG_DIR".to_owned(), "/h/claude".to_owned()),
            ("CODEX_HOME".to_owned(), "/h/codex".to_owned()),
        ];

        assert_eq!(
            relocated_provider_homes(&env, &["CODEX_HOME", "CLAUDE_CONFIG_DIR"]),
            vec![
                std::path::PathBuf::from("/h/codex"),
                std::path::PathBuf::from("/h/claude"),
            ]
        );
    }

    #[test]
    fn relocated_provider_homes_ignores_relative_unset_and_non_home_keys() {
        let env = vec![
            ("CODEX_HOME".to_owned(), ".codex".to_owned()),
            ("PATH".to_owned(), "/usr/bin".to_owned()),
        ];

        assert!(relocated_provider_homes(&env, &["CODEX_HOME"]).is_empty());
        assert!(relocated_provider_homes(&[], &["CODEX_HOME"]).is_empty());
    }

    #[test]
    fn with_provider_home_mount_appends_a_path_preserving_rw_mount_and_makes_the_home_writable() {
        let mounts = [m("/work/tree", false)];
        let home = Path::new("/provider/home");
        let carved = with_provider_home_mount(&mounts, home);

        assert_eq!(carved.len(), 2);
        assert_eq!(carved[1].host, "/provider/home");
        assert_eq!(carved[1].dest, "/provider/home");
        assert!(!carved[1].ro);

        let provider_home = WorkerFacts {
            provider_home: Some(home),
            provider_home_exists: true,
            ..facts(&mounts, Path::new("/work/tree"))
        };
        let carved_provider_home = WorkerFacts {
            mounts: &carved,
            ..provider_home
        };
        assert_eq!(
            classify_worker(&carved_provider_home),
            WorkerReadiness::Ready
        );
        assert_eq!(
            classify_worker(&WorkerFacts {
                provider_home: Some(home),
                provider_home_exists: true,
                ..facts(&mounts, Path::new("/work/tree"))
            }),
            WorkerReadiness::ProviderHomeUnavailable
        );
    }

    #[test]
    fn classify_check_order_is_stable() {
        let mounts = [m("/work/tree", true)];
        let first_failure = WorkerFacts {
            git_common_dir: Some(Path::new("/git/common")),
            callback: CallbackBinary::Missing,
            provider_home: Some(Path::new("/provider/home")),
            provider_home_exists: false,
            ..facts(&mounts, Path::new("/work/tree"))
        };

        assert_eq!(
            classify_worker(&first_failure),
            WorkerReadiness::CannotWriteWorktree
        );

        let writable_worktree = [m("/work/tree", false)];
        let metadata_before_callback = WorkerFacts {
            git_common_dir: Some(Path::new("/git/common")),
            callback: CallbackBinary::Missing,
            ..facts(&writable_worktree, Path::new("/work/tree"))
        };
        assert_eq!(
            classify_worker(&metadata_before_callback),
            WorkerReadiness::CannotWriteGitMetadata
        );

        let callback_before_provider_home = WorkerFacts {
            callback: CallbackBinary::Missing,
            provider_home: Some(Path::new("/provider/home")),
            provider_home_exists: false,
            ..facts(&writable_worktree, Path::new("/work/tree"))
        };
        assert_eq!(
            classify_worker(&callback_before_provider_home),
            WorkerReadiness::CallbackUnavailable
        );

        let backend_none_before_unsupported_backend = WorkerFacts {
            backend: Backend::None,
            callback: CallbackBinary::Missing,
            ..facts(&[], Path::new("/work/tree"))
        };
        assert_eq!(
            classify_worker(&backend_none_before_unsupported_backend),
            WorkerReadiness::ContainmentAbsent
        );
    }

    #[test]
    fn readiness_as_str_is_unique_and_only_ready_is_ready() {
        let states = [
            (WorkerReadiness::Ready, "ready"),
            (WorkerReadiness::ContainmentAbsent, "containment-absent"),
            (WorkerReadiness::UnsupportedPlatform, "unsupported-platform"),
            (WorkerReadiness::UnsupportedBackend, "unsupported-backend"),
            (
                WorkerReadiness::CannotWriteWorktree,
                "cannot-write-worktree",
            ),
            (
                WorkerReadiness::CannotWriteGitMetadata,
                "cannot-write-git-metadata",
            ),
            (WorkerReadiness::CallbackUnavailable, "callback-unavailable"),
            (
                WorkerReadiness::ProviderHomeUnavailable,
                "provider-home-unavailable",
            ),
        ];
        let labels = states.map(|(state, _)| state.as_str());

        for (index, (state, expected_label)) in states.iter().enumerate() {
            let label = labels[index];
            assert!(
                !labels[..index].contains(&label),
                "duplicate readiness label"
            );
            assert_eq!(state.as_str(), *expected_label);
            assert_eq!(state.is_ready(), *state == WorkerReadiness::Ready);
        }
    }
}
