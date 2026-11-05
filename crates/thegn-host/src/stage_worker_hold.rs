//! THE-97 (B): refuse a pipeline stage launch whose composed sandbox could not do the stage's job.

use std::path::{Path, PathBuf};

use thegn_core::sandbox::{Backend, Mount};
use thegn_core::sandbox_worker_ready::{
    CallbackBinary, WorkerFacts, WorkerReadiness, classify_worker, launch_hold_reason,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StageWorkerHold(pub String);

impl std::fmt::Display for StageWorkerHold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StageWorkerHold {}

pub(crate) struct StageHoldFacts<'a> {
    pub backend: Backend,
    pub local: bool,
    pub worktree: &'a Path,
    pub mounts: &'a [Mount],
    pub git_common_dir: Option<&'a Path>,
    pub provider_homes: &'a [PathBuf],
    pub callback_ok: bool,
}

pub(crate) fn stage_git_common_dir(worktree: &Path) -> Option<PathBuf> {
    let common = thegn_core::util::git_common_dir(worktree);
    (!common.starts_with(worktree)).then_some(common)
}

pub(crate) fn stage_worker_hold(
    stage: &str,
    facts: &StageHoldFacts<'_>,
) -> Option<StageWorkerHold> {
    if !facts.local {
        return None;
    }
    let callback = if facts.callback_ok {
        CallbackBinary::Present
    } else {
        CallbackBinary::Missing
    };
    let homes: Vec<Option<&Path>> = if facts.provider_homes.is_empty() {
        vec![None]
    } else {
        facts
            .provider_homes
            .iter()
            .map(|home| Some(home.as_path()))
            .collect()
    };
    for home in homes {
        let readiness = classify_worker(&WorkerFacts {
            linux_host: cfg!(target_os = "linux"),
            backend: facts.backend,
            mounts: facts.mounts,
            worktree: facts.worktree,
            git_common_dir: facts.git_common_dir,
            callback,
            provider_home: home,
            provider_home_exists: home.is_none_or(Path::is_dir),
        });
        if let Some(reason) = launch_hold_reason(readiness) {
            let at = if readiness == WorkerReadiness::ProviderHomeUnavailable {
                home.map(|path| format!(" (credential home {})", path.display()))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            return Some(StageWorkerHold(format!(
                "stage `{stage}` launch refused: stage-worker readiness `{}` for {} (backend `{}`): {reason}{at}; fix the sandbox mounts (see `thegn doctor`); no worker was started",
                readiness.as_str(),
                facts.worktree.display(),
                facts.backend.label()
            )));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use anyhow::Error;
    use thegn_core::sandbox::{Backend, Mount};

    use super::{StageHoldFacts, StageWorkerHold, stage_worker_hold};

    fn rw(dest: &str, ro: bool) -> Mount {
        Mount {
            host: dest.into(),
            dest: dest.into(),
            ro,
            cache: false,
        }
    }

    fn facts<'a>(
        mounts: &'a [Mount],
        common: Option<&'a Path>,
        homes: &'a [PathBuf],
    ) -> StageHoldFacts<'a> {
        StageHoldFacts {
            backend: Backend::Bwrap,
            local: true,
            worktree: Path::new("/work/tree"),
            mounts,
            git_common_dir: common,
            provider_homes: homes,
            callback_ok: true,
        }
    }

    fn linux_hold(stage: &str, facts: &StageHoldFacts<'_>) -> Option<StageWorkerHold> {
        if cfg!(target_os = "linux") {
            stage_worker_hold(stage, facts)
        } else {
            None
        }
    }

    #[test]
    fn stage_worker_hold_passes_a_writable_worktree_and_git_dir() {
        let mounts = [rw("/work/tree", false), rw("/git/common", false)];
        assert!(linux_hold("plan", &facts(&mounts, Some(Path::new("/git/common")), &[])).is_none());
    }

    #[test]
    fn stage_worker_hold_names_a_read_only_git_dir() {
        let mounts = [rw("/work/tree", false), rw("/git/common", true)];
        let hold =
            linux_hold("plan", &facts(&mounts, Some(Path::new("/git/common")), &[])).unwrap();
        for text in [
            "stage `plan` launch refused",
            "cannot-write-git-metadata",
            "bwrap",
            "no worker was started",
        ] {
            assert!(hold.0.contains(text));
        }
    }

    #[test]
    fn stage_worker_hold_names_a_read_only_worktree() {
        let mounts = [rw("/work/tree", true), rw("/git/common", false)];
        let hold =
            linux_hold("plan", &facts(&mounts, Some(Path::new("/git/common")), &[])).unwrap();
        assert!(hold.0.contains("cannot-write-worktree"));
    }

    #[test]
    fn stage_worker_hold_ignores_a_main_checkout_git_dir() {
        let mounts = [rw("/work/tree", false)];
        assert!(linux_hold("plan", &facts(&mounts, None, &[])).is_none());
    }

    #[test]
    fn stage_worker_hold_names_a_stale_callback() {
        let mounts = [rw("/work/tree", false)];
        let mut facts = facts(&mounts, None, &[]);
        facts.callback_ok = false;
        assert!(
            linux_hold("plan", &facts)
                .unwrap()
                .0
                .contains("callback-unavailable")
        );
    }

    #[test]
    fn stage_worker_hold_names_a_missing_provider_home() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("missing-home");
        let mounts = [rw("/work/tree", false), rw(home.to_str().unwrap(), false)];
        let homes = [home.clone()];
        let hold = linux_hold("plan", &facts(&mounts, None, &homes)).unwrap();
        assert!(hold.0.contains("provider-home-unavailable"));
        assert!(hold.0.contains(&home.display().to_string()));
    }

    #[test]
    fn stage_worker_hold_accepts_an_existing_writable_provider_home() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().to_path_buf();
        let mounts = [rw("/work/tree", false), rw(home.to_str().unwrap(), false)];
        let homes = [home];
        assert!(linux_hold("plan", &facts(&mounts, None, &homes)).is_none());
    }

    #[test]
    fn stage_worker_hold_never_holds_an_unsupported_backend() {
        let mounts = [];
        let mut facts = facts(&mounts, None, &[]);
        facts.backend = Backend::Systemd;
        assert!(stage_worker_hold("plan", &facts).is_none());
    }

    #[test]
    fn stage_worker_hold_skips_a_remote_placement() {
        let mounts = [rw("/work/tree", true)];
        let mut facts = facts(&mounts, None, &[]);
        facts.local = false;
        assert!(stage_worker_hold("plan", &facts).is_none());
    }

    #[test]
    fn stage_worker_hold_is_a_typed_error() {
        let error = Error::new(StageWorkerHold("x".into()));
        assert!(error.downcast_ref::<StageWorkerHold>().is_some());
        assert_eq!(error.to_string(), "x");
    }
}
