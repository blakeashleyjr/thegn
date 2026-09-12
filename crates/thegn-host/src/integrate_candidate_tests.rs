use super::*;
use crate::integrate::{candidate_branches, fold_active_repo, hold_unenqueued};
use std::collections::HashSet;
use thegn_core::config::{Config, OnLanded};
use thegn_core::db::Db;
use thegn_core::store::{WorkspaceStore, WorktreeAuxStore};

struct Fixture {
    _env: crate::testenv::EnvVarGuard,
    _root: tempfile::TempDir,
    repo: PathBuf,
    queued: PathBuf,
    unqueued: PathBuf,
    db: Db,
    config: Config,
}

#[expect(clippy::disallowed_methods)]
fn git(path: &Path, args: &[&str]) -> String {
    let out = util::git_cmd(path).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

impl Fixture {
    // Positive strict-snapshot steps are Unix-only. Windows runs an actual
    // unsupported-admission/no-mutation assertion instead of ignoring the test.
    // The separate snapshot-disabled test exercises portable compatibility.
    fn supported_or_refused(&self) -> bool {
        if thegn_core::sandbox_backend::host_os() != thegn_core::sandbox_backend::HostOs::Windows {
            return true;
        }
        let queued = Self::snapshot(&self.queued);
        let unqueued = Self::snapshot(&self.unqueued);
        let error = candidate_branches(&self.config.merge_queue, &self.repo, "main")
            .err()
            .expect("Windows strict snapshot must refuse");
        assert!(error.to_string().contains("unsupported on Windows"));
        assert_eq!(Self::snapshot(&self.queued), queued);
        assert_eq!(Self::snapshot(&self.unqueued), unqueued);
        false
    }

    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let config_home = root.path().join("config");
        let template = root.path().join("template");
        std::fs::create_dir(&template).unwrap();
        let global = root.path().join("gitconfig");
        std::fs::write(&global, "").unwrap();
        let env = crate::testenv::EnvVarGuard::set(&[
            ("XDG_STATE_HOME", state.to_str().unwrap()),
            ("XDG_CONFIG_HOME", config_home.to_str().unwrap()),
            ("LOCALAPPDATA", state.to_str().unwrap()),
            ("THEGN_DIR", root.path().to_str().unwrap()),
            ("THEGN_PROFILE", ""),
            ("GIT_CONFIG_GLOBAL", global.to_str().unwrap()),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_COUNT", "0"),
            ("GIT_CONFIG_PARAMETERS", ""),
            ("GIT_TEMPLATE_DIR", template.to_str().unwrap()),
        ]);
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.name", "Private Test"]);
        git(&repo, &["config", "user.email", "private@example.invalid"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        let hooks = root.path().join("hooks");
        std::fs::create_dir(&hooks).unwrap();
        git(
            &repo,
            &["config", "core.hooksPath", hooks.to_str().unwrap()],
        );
        std::fs::write(repo.join("tracked"), "base\n").unwrap();
        git(&repo, &["add", "tracked"]);
        git(&repo, &["commit", "-q", "-m", "base"]);
        let queued = root.path().join("queued");
        let unqueued = root.path().join("unqueued");
        for (path, name) in [(&queued, "queued"), (&unqueued, "unqueued")] {
            git(
                &repo,
                &["worktree", "add", "-q", "-b", name, path.to_str().unwrap()],
            );
            std::fs::write(path.join("tracked"), format!("{name} staged\n")).unwrap();
            git(path, &["add", "tracked"]);
            std::fs::write(path.join("tracked"), format!("{name} unstaged\n")).unwrap();
            std::fs::write(path.join("untracked"), "private data\n").unwrap();
        }
        std::fs::create_dir_all(state.join("thegn")).unwrap();
        let db = Db::open_at(&state.join("thegn/thegn.db")).unwrap();
        let mut config = Config::default();
        config.merge_queue = MergeQueueConfig {
            target_branch: "main".into(),
            snapshot_dirty: true,
            gate_on: false,
            require_enqueue: true,
            organize_folders: false,
            on_landed: OnLanded::Off,
            ..Default::default()
        };
        db.enqueue_merge(queued.to_str().unwrap(), "queued", "main")
            .unwrap();
        Self {
            _env: env,
            _root: root,
            repo,
            queued,
            unqueued,
            db,
            config,
        }
    }

    fn discovered(&self) -> Candidates {
        candidate_branches(&self.config.merge_queue, &self.repo, "main").unwrap()
    }

    fn selected(&self) -> Candidates {
        let mut found = self.discovered();
        hold_unenqueued(
            &mut found,
            &HashSet::from([self.queued.to_string_lossy().into_owned()]),
        );
        found
    }

    fn snapshot(path: &Path) -> (String, Vec<u8>, Vec<u8>, Vec<u8>) {
        let index = git_path(path, "--absolute-git-dir").unwrap().join("index");
        (
            git(path, &["rev-parse", "HEAD"]),
            std::fs::read(index).unwrap(),
            std::fs::read(path.join("tracked")).unwrap(),
            std::fs::read(path.join("untracked")).unwrap(),
        )
    }

    fn snapshot_selected(&self, candidates: &Candidates) -> Result<Vec<Branch>> {
        let observations = crate::integrate::persistence::observe_outcomes(&self.db, candidates)?;
        selected_snapshot_tips(
            &self.config.merge_queue,
            &self.repo,
            candidates,
            Some(&observations),
            true,
        )
    }
}

#[test]
fn discovery_never_snapshots_even_when_dirty_snapshots_are_enabled() {
    let f = Fixture::new();
    if !f.supported_or_refused() {
        return;
    }
    let queued = Fixture::snapshot(&f.queued);
    let unqueued = Fixture::snapshot(&f.unqueued);
    let rows = f.db.list_merge_queue().unwrap();
    let found = f.discovered();
    assert_eq!(found.pending_snapshots.len(), 2);
    assert_eq!(found.branches.len(), 2);
    assert_eq!(Fixture::snapshot(&f.queued), queued);
    assert_eq!(Fixture::snapshot(&f.unqueued), unqueued);
    assert_eq!(f.db.list_merge_queue().unwrap(), rows);
}

#[test]
fn ui_fold_snapshots_only_the_selected_queued_worktree() {
    let f = Fixture::new();
    if !f.supported_or_refused() {
        return;
    }
    let unqueued = Fixture::snapshot(&f.unqueued);
    let queued_head = git(&f.queued, &["rev-parse", "HEAD"]);
    let report = fold_active_repo(&f.config, &f.repo).unwrap();
    assert!(report.advanced);
    assert!(report.request_result().is_ok());
    assert_ne!(git(&f.queued, &["rev-parse", "HEAD"]), queued_head);
    assert_eq!(Fixture::snapshot(&f.unqueued), unqueued);
    assert_eq!(
        std::fs::read(f.repo.join("tracked")).unwrap(),
        b"queued unstaged\n"
    );
    assert_eq!(f.db.list_merge_queue().unwrap()[0].status, "landed");
}

#[test]
fn dirty_lookup_failure_is_not_clean_or_snapshot_permission() {
    let f = Fixture::new();
    let index = git_path(&f.queued, "--absolute-git-dir")
        .unwrap()
        .join("index");
    std::fs::write(&index, "invalid private index").unwrap();
    let before = Fixture::snapshot(&f.queued);
    assert!(candidate_branches(&f.config.merge_queue, &f.repo, "main").is_err());
    assert_eq!(Fixture::snapshot(&f.queued), before);
}

#[test]
fn changed_head_or_branch_refuses_before_any_selected_snapshot() {
    for rename in [false, true] {
        let f = Fixture::new();
        if !f.supported_or_refused() {
            return;
        }
        let selected = f.selected();
        if rename {
            git(&f.queued, &["branch", "-m", "renamed"]);
        } else {
            git(&f.queued, &["commit", "-q", "-m", "external staged commit"]);
        }
        let before = Fixture::snapshot(&f.queued);
        assert!(f.snapshot_selected(&selected).is_err());
        assert_eq!(Fixture::snapshot(&f.queued), before);
    }
}

#[test]
fn remote_or_reassigned_registry_never_retargets_snapshot() {
    for remote in [false, true] {
        let f = Fixture::new();
        if !f.supported_or_refused() {
            return;
        }
        let selected = f.selected();
        f.db.put_worktree(
            "repo/queued",
            f.repo.to_str().unwrap(),
            f.queued.to_str().unwrap(),
            if remote { "queued" } else { "different-branch" },
            None,
            None,
        )
        .unwrap();
        if remote {
            f.db.set_worktree_location(
                f.queued.to_str().unwrap(),
                r#"{"host":"invalid.example","port":22,"path":"/private"}"#,
            )
            .unwrap();
        }
        let before = Fixture::snapshot(&f.queued);
        assert!(f.snapshot_selected(&selected).is_err());
        assert_eq!(Fixture::snapshot(&f.queued), before);
    }
}

#[test]
fn missing_observation_prevents_snapshots() {
    let f = Fixture::new();
    if !f.supported_or_refused() {
        return;
    }
    let before = Fixture::snapshot(&f.queued);
    assert!(
        selected_snapshot_tips(&f.config.merge_queue, &f.repo, &f.selected(), None, true).is_err()
    );
    assert_eq!(Fixture::snapshot(&f.queued), before);
}

#[test]
fn snapshot_disabled_keeps_pinned_tips_without_mutating_dirty_work() {
    let f = Fixture::new();
    let selected = Candidates {
        branches: vec![Branch {
            name: "queued".into(),
            tip: git(&f.queued, &["rev-parse", "HEAD"]),
        }],
        worktrees: std::collections::HashMap::from([(
            "queued".into(),
            f.queued.to_string_lossy().into_owned(),
        )]),
        identities: std::collections::HashMap::new(),
        pending_snapshots: HashSet::new(),
        skipped_dirty: Vec::new(),
    };
    let before = Fixture::snapshot(&f.queued);
    let mut config = f.config.merge_queue.clone();
    config.snapshot_dirty = false;
    let tips = selected_snapshot_tips(&config, &f.repo, &selected, None, true).unwrap();
    assert_eq!(tips[0].tip, selected.branches[0].tip);
    assert_eq!(Fixture::snapshot(&f.queued), before);
    let found = candidate_branches(&config, &f.repo, "main").unwrap();
    assert!(found.identities.is_empty());
    assert!(found.branches.is_empty());
    assert_eq!(found.skipped_dirty.len(), 2);
}

#[test]
fn noncanonical_candidate_paths_are_refused_before_mutation() {
    let f = Fixture::new();
    let alias = f.queued.join("..").join("queued");
    let before = Fixture::snapshot(&f.queued);
    assert!(LocalIdentity::read(&f.repo, &alias, "queued").is_err());
    assert_eq!(Fixture::snapshot(&f.queued), before);
}

#[test]
fn backlink_reader_requires_bounded_regular_registration() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("gitdir");
    std::fs::write(&file, vec![b'a'; 64 * 1024 + 1]).unwrap();
    assert!(read_backlink(&file).is_err());
    assert!(read_backlink(root.path()).is_err());
    std::fs::write(&file, b"/private/worktree/.git\n").unwrap();
    assert_eq!(read_backlink(&file).unwrap(), "/private/worktree/.git\n");
}

#[test]
fn all_selected_candidates_are_preflighted_before_the_first_snapshot() {
    let f = Fixture::new();
    if !f.supported_or_refused() {
        return;
    }
    let selected = f.discovered();
    git(&f.unqueued, &["branch", "-m", "changed"]);
    let queued = Fixture::snapshot(&f.queued);
    assert!(f.snapshot_selected(&selected).is_err());
    assert_eq!(Fixture::snapshot(&f.queued), queued);
}

#[test]
fn changed_common_directory_never_snapshots_the_replacement_repository() {
    let f = Fixture::new();
    if !f.supported_or_refused() {
        return;
    }
    let selected = f.selected();
    let foreign = f._root.path().join("foreign");
    std::fs::create_dir(&foreign).unwrap();
    git(&foreign, &["init", "-q", "-b", "main"]);
    git(&foreign, &["config", "user.name", "private"]);
    git(
        &foreign,
        &["config", "user.email", "private@example.invalid"],
    );
    git(
        &foreign,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "base",
        ],
    );
    let replacement = f._root.path().join("replacement");
    git(
        &foreign,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "queued",
            replacement.to_str().unwrap(),
        ],
    );
    std::fs::write(
        f.queued.join(".git"),
        std::fs::read(replacement.join(".git")).unwrap(),
    )
    .unwrap();
    let foreign_head = git(&replacement, &["rev-parse", "HEAD"]);
    assert!(f.snapshot_selected(&selected).is_err());
    assert_eq!(git(&replacement, &["rev-parse", "HEAD"]), foreign_head);
    assert!(!replacement.join("tracked").exists());
}

#[test]
fn later_snapshot_failure_reports_partial_authorized_snapshots_without_rollback() {
    let f = Fixture::new();
    if !f.supported_or_refused() {
        return;
    }
    let mut selected = f.discovered();
    selected.branches.sort_by(|a, b| a.name.cmp(&b.name));
    let queued_head = git(&f.queued, &["rev-parse", "HEAD"]);
    let unqueued_head = git(&f.unqueued, &["rev-parse", "HEAD"]);
    // An owned lock in the second selected worktree permits read-only status
    // (GIT_OPTIONAL_LOCKS=0), but rejects its required index write. No hook runs.
    let lock = git_path(&f.unqueued, "--absolute-git-dir")
        .unwrap()
        .join("index.lock");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock)
        .unwrap();
    let error = f.snapshot_selected(&selected).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("1 earlier authorized snapshots may remain")
    );
    assert!(error.to_string().contains("no rollback was attempted"));
    assert_ne!(git(&f.queued, &["rev-parse", "HEAD"]), queued_head);
    assert_eq!(git(&f.unqueued, &["rev-parse", "HEAD"]), unqueued_head);
    assert!(lock.exists());
}
