//! DB-writer preparation and loop-side fencing for workspace switches.

use crate::session::{PreparedWorkspaceSwitch, Session};

type WorktreeRegistryRow = (String, String, String);
type ConnectRegistry = (Vec<WorktreeRegistryRow>, Vec<String>);

#[derive(Debug, Clone, Default)]
pub(crate) struct AfterSwitch {
    pub(crate) focus_center: bool,
    pub(crate) set_region_last_w: bool,
    pub(crate) reveal_active: bool,
    pub(crate) focus_active_row: bool,
    pub(crate) region_bookmark: Option<(bool, Option<String>)>,
    pub(crate) open_merge_queue: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct SwitchRequestSpec {
    pub(crate) target: String,
    pub(crate) group: Option<String>,
    pub(crate) after: AfterSwitch,
}

#[derive(Debug, Clone)]
pub(crate) struct SwitchRequest {
    pub(crate) seq: u64,
    pub(crate) target: String,
    pub(crate) group: Option<String>,
    pub(crate) after: AfterSwitch,
    pub(crate) requested_at: std::time::Instant,
}

#[derive(Debug, Default)]
pub(crate) struct PendingSwitch {
    seq: u64,
    request: Option<SwitchRequest>,
    spinner: Option<(u64, String)>,
}

impl PendingSwitch {
    pub(crate) fn begin(&mut self, spec: SwitchRequestSpec) -> SwitchRequest {
        self.seq = self.seq.wrapping_add(1);
        let request = SwitchRequest {
            seq: self.seq,
            target: spec.target,
            group: spec.group,
            after: spec.after,
            requested_at: std::time::Instant::now(),
        };
        self.request = Some(request.clone());
        request
    }

    pub(crate) fn seq(&self) -> u64 {
        self.seq
    }

    pub(crate) fn current(&self, seq: u64) -> bool {
        self.request
            .as_ref()
            .is_some_and(|request| request.seq == seq)
    }

    pub(crate) fn request_target(&self) -> Option<&str> {
        self.request.as_ref().map(|request| request.target.as_str())
    }

    pub(crate) fn set_spinner(&mut self, seq: u64, status: String) {
        self.spinner = Some((seq, status));
    }

    pub(crate) fn clear_spinner_if_matches(&mut self, seq: u64, status: &mut String) {
        if let Some((spinner_seq, spinner)) = &self.spinner
            && *spinner_seq == seq
            && status == spinner
        {
            status.clear();
            self.spinner = None;
        }
    }

    pub(crate) fn clear(&mut self, seq: u64) {
        if self.current(seq) {
            self.request = None;
        }
    }

    pub(crate) fn supersede(&mut self) {
        self.seq = self.seq.wrapping_add(1);
        self.request = None;
    }
}

#[derive(Debug)]
pub(crate) enum SwitchResult {
    Cold {
        request: SwitchRequest,
        result: anyhow::Result<PreparedWorkspaceSwitch>,
    },
    Adopted {
        seq: u64,
        session_id: String,
        rows: Vec<crate::session::WorktreeGroup>,
        land_on: Option<String>,
    },
    ConnectRoot {
        seq: u64,
        session_id: String,
        root: Option<std::path::PathBuf>,
        cwd: Option<std::path::PathBuf>,
        group_paths: Vec<String>,
        result: anyhow::Result<ConnectRegistry>,
    },
}

pub(crate) fn request_workspace_switch(
    spec: SwitchRequestSpec,
    pending: &mut PendingSwitch,
    tx: &tokio::sync::mpsc::UnboundedSender<SwitchResult>,
    waker: &termwiz::terminal::TerminalWaker,
    cfg: &thegn_core::config::Config,
) -> SwitchRequest {
    let request = pending.begin(spec);
    let job_request = request.clone();
    let job_tx = tx.clone();
    let wake = waker.clone();
    let cfg = cfg.clone();
    crate::db_task::persist_critical(move |db| {
        let result = Session::prepare_workspace_switch(&job_request.target, db, &cfg);
        if let Err(error) = job_tx.send(SwitchResult::Cold {
            request: job_request,
            result,
        }) {
            tracing::debug!(target: "thegn::workspace_switch", %error, "workspace switch result receiver is gone");
        }
        if let Err(error) = wake.wake() {
            tracing::debug!(target: "thegn::workspace_switch", %error, "workspace switch result wake failed");
        }
    });
    request
}

pub(crate) fn request_registered_adoption(
    session: &Session,
    land_on: Option<String>,
    pending: &mut PendingSwitch,
    tx: &tokio::sync::mpsc::UnboundedSender<SwitchResult>,
    waker: &termwiz::terminal::TerminalWaker,
    cfg: &thegn_core::config::Config,
) {
    let session_id = session.id.clone();
    let existing = session.worktrees.clone();
    let request = pending.begin(SwitchRequestSpec {
        target: session_id.clone(),
        group: land_on.clone(),
        after: AfterSwitch::default(),
    });
    let seq = request.seq;
    let tx = tx.clone();
    let wake = waker.clone();
    let cfg = cfg.clone();
    crate::db_task::persist_critical(move |db| {
        let rows = Session::prepare_registered_adoption(&session_id, &existing, db, &cfg);
        if let Err(error) = tx.send(SwitchResult::Adopted {
            seq,
            session_id,
            rows,
            land_on,
        }) {
            tracing::debug!(target: "thegn::workspace_switch", %error, "worktree adoption result receiver is gone");
        }
        if let Err(error) = wake.wake() {
            tracing::debug!(target: "thegn::workspace_switch", %error, "worktree adoption result wake failed");
        }
    });
}

pub(crate) fn request_connect_root(
    cwd: Option<std::path::PathBuf>,
    group_paths: Vec<String>,
    session_id: &str,
    pending: &mut PendingSwitch,
    tx: &tokio::sync::mpsc::UnboundedSender<SwitchResult>,
    waker: &termwiz::terminal::TerminalWaker,
) {
    let request = pending.begin(SwitchRequestSpec {
        target: session_id.to_string(),
        group: None,
        after: AfterSwitch::default(),
    });
    let seq = request.seq;
    let session_id = session_id.to_string();
    let tx = tx.clone();
    let wake = waker.clone();
    tokio::spawn(async move {
        let root_cwd = cwd.clone();
        let root = match tokio::task::spawn_blocking(move || resolve_connect_root(root_cwd)).await {
            Ok(root) => root,
            Err(error) => {
                tracing::debug!(target: "thegn::workspace_switch", %error, "connect-root root lookup worker failed");
                None
            }
        };
        crate::db_task::persist_critical(move |db| {
            let result = read_connect_registry(db);
            if let Err(error) = tx.send(SwitchResult::ConnectRoot {
                seq,
                session_id,
                root,
                cwd,
                group_paths,
                result,
            }) {
                tracing::debug!(target: "thegn::workspace_switch", %error, "connect-root result receiver is gone");
            }
            if let Err(error) = wake.wake() {
                tracing::debug!(target: "thegn::workspace_switch", %error, "connect-root result wake failed");
            }
        });
    });
}

fn resolve_connect_root(cwd: Option<std::path::PathBuf>) -> Option<std::path::PathBuf> {
    cwd.as_deref()
        .and_then(thegn_core::repo::worktree_root_for_cwd)
}

fn read_connect_registry(db: &thegn_core::db::Db) -> anyhow::Result<ConnectRegistry> {
    use thegn_core::store::WorkspaceStore;
    let worktrees = db
        .worktrees()?
        .into_iter()
        .map(|row| (row.worktree, row.repo_root, row.tab_name))
        .collect();
    let workspaces = db
        .workspaces()?
        .into_iter()
        .map(|row| row.repo_path)
        .collect();
    Ok((worktrees, workspaces))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColdResultDecision {
    Drop,
    ReportFailure,
    Apply,
}

pub(crate) fn cold_result_decision(
    seq: u64,
    current_seq: u64,
    current_session: &str,
    target: &str,
    succeeded: bool,
) -> ColdResultDecision {
    if seq != current_seq || current_session == target {
        ColdResultDecision::Drop
    } else if succeeded {
        ColdResultDecision::Apply
    } else {
        ColdResultDecision::ReportFailure
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AfterSwitchEffects {
    pub(crate) region_last_w: Option<Option<String>>,
    pub(crate) region_last_t: Option<Option<String>>,
    pub(crate) focus_center: bool,
    pub(crate) reveal_active: bool,
    pub(crate) focus_active_row: bool,
    pub(crate) open_merge_queue: bool,
}

pub(crate) fn after_switch_effects(after: &AfterSwitch, session: &Session) -> AfterSwitchEffects {
    let active = session
        .worktrees
        .get(session.active)
        .map(|g| g.name.clone());
    let (mut region_last_w, mut region_last_t) = (None, None);
    if let Some((was_terminal, previous)) = &after.region_bookmark {
        if *was_terminal {
            region_last_t = Some(previous.clone());
        } else {
            region_last_w = Some(previous.clone());
        }
    }
    if after.set_region_last_w {
        region_last_w = Some(active);
    }
    AfterSwitchEffects {
        region_last_w,
        region_last_t,
        focus_center: after.focus_center,
        reveal_active: after.reveal_active,
        focus_active_row: after.focus_active_row,
        open_merge_queue: after.open_merge_queue,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdoptionDecision {
    Ignore,
    Land,
    ReportStale,
}

pub(crate) fn adoption_decision(current: bool, found: bool) -> AdoptionDecision {
    match (current, found) {
        (false, _) => AdoptionDecision::Ignore,
        (true, true) => AdoptionDecision::Land,
        (true, false) => AdoptionDecision::ReportStale,
    }
}

pub(crate) fn should_replace_jump_status(activation_pending: bool) -> bool {
    !activation_pending
}

pub(crate) fn resolve_connect_tab_index(
    session_matches: bool,
    requested_index: usize,
    requested_paths: &[String],
    current_paths: &[String],
) -> Option<usize> {
    if !session_matches {
        return None;
    }
    let target = requested_paths.get(requested_index)?;
    current_paths
        .iter()
        .position(|path| path.as_str() == target.as_str())
}

pub(crate) fn seed_deferred_preset(
    applied_seq: Option<u64>,
    deferred: &mut Option<(u64, thegn_core::store::IntentRow)>,
) -> Option<thegn_core::store::IntentRow> {
    match (applied_seq, deferred.as_ref()) {
        (Some(applied), Some((seq, _))) if applied == *seq => deferred.take().map(|(_, row)| row),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "creates an isolated Git fixture for root resolution"
    )]
    fn connect_root_resolves_canonical_git_root_from_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let initialized = thegn_core::util::git_cmd(&repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["-c", "commit.gpgsign=false", "init", "--quiet"])
            .output()
            .unwrap();
        assert!(initialized.status.success());
        let nested = repo.join("nested");
        std::fs::create_dir(&nested).unwrap();
        assert_eq!(
            resolve_connect_root(Some(nested)),
            Some(repo.canonicalize().unwrap())
        );
    }

    #[test]
    fn superseded_cold_result_is_dropped() {
        let mut pending = PendingSwitch::default();
        let first = pending.begin(SwitchRequestSpec {
            target: "/one".into(),
            group: None,
            after: AfterSwitch::default(),
        });
        let second = pending.begin(SwitchRequestSpec {
            target: "/two".into(),
            group: None,
            after: AfterSwitch::default(),
        });
        assert!(!pending.current(first.seq));
        assert!(pending.current(second.seq));
        assert_eq!(
            cold_result_decision(first.seq, pending.seq(), "/old", "/one", true),
            ColdResultDecision::Drop
        );
    }

    #[test]
    fn result_for_now_active_target_is_dropped() {
        assert_eq!(
            cold_result_decision(7, 7, "/target", "/target", true),
            ColdResultDecision::Drop
        );
    }

    #[test]
    fn prepare_error_is_reported_for_a_different_current_workspace() {
        let result: anyhow::Result<PreparedWorkspaceSwitch> = Err(anyhow::anyhow!("unreadable"));
        assert_eq!(
            cold_result_decision(2, 2, "/current", "/other", result.is_ok()),
            ColdResultDecision::ReportFailure
        );
    }

    #[test]
    fn cold_request_changes_nothing_until_apply() {
        let mut session = Session::default();
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let mut panes = crate::panes::Panes::new(tx);
        let mut pool = crate::run::WorkspacePool::default();
        let active = session.active;
        let id = session.id.clone();
        let groups = session.worktrees.clone();
        let (mut need_relayout, mut clear) = (false, false);
        assert_eq!(
            crate::run::switch_workspace(
                "/target",
                None,
                &mut session,
                &mut panes,
                &mut pool,
                &mut need_relayout,
                &mut clear,
            ),
            crate::run::SwitchStart::NeedsCold
        );
        assert_eq!(session.id, id);
        assert_eq!(session.active, active);
        assert_eq!(session.worktrees, groups);
        assert!(!need_relayout && !clear);
    }

    #[test]
    fn after_switch_effects_apply_bookmarks_and_flags_to_the_landed_session() {
        let after = AfterSwitch {
            focus_center: true,
            set_region_last_w: true,
            reveal_active: true,
            focus_active_row: true,
            region_bookmark: Some((true, Some("terminal".into()))),
            open_merge_queue: true,
        };
        let session = Session {
            id: "/repo".into(),
            worktrees: vec![crate::session::WorktreeGroup::new(
                "repo/main",
                crate::session::GroupKind::Home,
                "/repo",
            )],
            active: 0,
        };
        let effects = after_switch_effects(&after, &session);
        assert_eq!(effects.region_last_t, Some(Some("terminal".into())));
        assert_eq!(effects.region_last_w, Some(Some("repo/main".into())));
        assert!(effects.focus_center && effects.reveal_active && effects.focus_active_row);
        assert!(effects.open_merge_queue);
    }

    #[test]
    fn adopted_landing_reports_only_a_current_missing_group() {
        assert_eq!(adoption_decision(false, true), AdoptionDecision::Ignore);
        assert_eq!(adoption_decision(false, false), AdoptionDecision::Ignore);
        assert_eq!(adoption_decision(true, true), AdoptionDecision::Land);
        assert_eq!(
            adoption_decision(true, false),
            AdoptionDecision::ReportStale
        );
    }

    #[test]
    fn deferred_preset_survives_unrelated_iterations_and_releases_on_match() {
        let intent = thegn_core::store::IntentRow {
            id: 1,
            kind: "preset".into(),
            payload: String::new(),
            created_at: 0,
        };
        let mut deferred = Some((8, intent));
        assert!(seed_deferred_preset(Some(7), &mut deferred).is_none());
        assert!(deferred.is_some());
        assert!(seed_deferred_preset(None, &mut deferred).is_none());
        let released = seed_deferred_preset(Some(8), &mut deferred);
        assert!(released.is_some());
        assert!(deferred.is_none());
    }

    #[test]
    fn stale_cold_result_clears_only_its_own_spinner() {
        let mut pending = PendingSwitch::default();
        let first = pending.begin(SwitchRequestSpec {
            target: "/one".into(),
            group: None,
            after: AfterSwitch::default(),
        });
        pending.set_spinner(first.seq, "⠋ Opening one…".into());
        let second = pending.begin(SwitchRequestSpec {
            target: "/two".into(),
            group: None,
            after: AfterSwitch::default(),
        });
        pending.set_spinner(second.seq, "⠙ Opening two…".into());
        let mut status = "⠙ Opening two…".to_string();
        pending.clear_spinner_if_matches(first.seq, &mut status);
        assert_eq!(status, "⠙ Opening two…");
        pending.clear_spinner_if_matches(second.seq, &mut status);
        assert!(status.is_empty());
    }

    #[test]
    fn jump_status_is_suppressed_until_pending_activation_lands() {
        assert!(!should_replace_jump_status(true));
        assert!(should_replace_jump_status(false));
    }

    #[test]
    fn connect_root_tab_is_resolved_against_the_current_session_paths() {
        let requested = vec!["/a".to_string(), "/b".to_string()];
        let current = vec!["/b".to_string(), "/a".to_string()];
        assert_eq!(
            resolve_connect_tab_index(true, 1, &requested, &current),
            Some(0)
        );
        assert_eq!(
            resolve_connect_tab_index(false, 1, &requested, &current),
            None
        );
        assert_eq!(
            resolve_connect_tab_index(true, 1, &requested, &["/a".into()]),
            None
        );
    }
}
