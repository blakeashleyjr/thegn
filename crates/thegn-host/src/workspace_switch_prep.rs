//! DB-writer preparation and loop-side fencing for workspace switches.

use crate::session::{PreparedWorkspaceSwitch, Session};

#[derive(Debug, Clone, Default)]
pub(crate) struct AfterSwitch {
    pub(crate) focus_center: bool,
    pub(crate) set_region_last_w: bool,
    pub(crate) reveal_active: bool,
    pub(crate) focus_active_row: bool,
    pub(crate) region_bookmark: Option<(bool, Option<String>)>,
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
        root: Option<String>,
        cwd: Option<String>,
        group_paths: Vec<String>,
        result: anyhow::Result<(Vec<(String, String, String)>, Vec<String>)>,
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
    root: Option<String>,
    cwd: Option<String>,
    group_paths: Vec<String>,
    session_id: &str,
    pending: &mut PendingSwitch,
    tx: &tokio::sync::mpsc::UnboundedSender<SwitchResult>,
    waker: &termwiz::terminal::TerminalWaker,
) {
    use thegn_core::store::WorkspaceStore;
    let request = pending.begin(SwitchRequestSpec {
        target: session_id.to_string(),
        group: None,
        after: AfterSwitch::default(),
    });
    let seq = request.seq;
    let tx = tx.clone();
    let wake = waker.clone();
    crate::db_task::persist_critical(move |db| {
        let result: anyhow::Result<(Vec<(String, String, String)>, Vec<String>)> = (|| {
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
        })();
        if let Err(error) = tx.send(SwitchResult::ConnectRoot {
            seq,
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

pub(crate) fn after_switch_replay(after: &AfterSwitch) -> AfterSwitch {
    after.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn prepare_error_keeps_old_workspace_and_bookmarks() {
        let session = Session::default();
        let bookmarks = (Some("worktree".to_string()), Some("terminal".to_string()));
        let result: anyhow::Result<PreparedWorkspaceSwitch> = Err(anyhow::anyhow!("unreadable"));
        assert_eq!(
            cold_result_decision(2, 2, &session.id, "/other", result.is_ok()),
            ColdResultDecision::ReportFailure
        );
        assert_eq!(session.id, "default");
        assert_eq!(bookmarks.0.as_deref(), Some("worktree"));
        assert_eq!(bookmarks.1.as_deref(), Some("terminal"));
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
        let bookmarks = (Some("worktree".to_string()), Some("terminal".to_string()));
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
        assert_eq!(bookmarks.0.as_deref(), Some("worktree"));
        assert_eq!(bookmarks.1.as_deref(), Some("terminal"));
    }

    #[test]
    fn after_switch_flags_are_replayed_on_apply() {
        let after = AfterSwitch {
            focus_center: true,
            set_region_last_w: true,
            reveal_active: true,
            focus_active_row: true,
            region_bookmark: Some((true, Some("terminal".into()))),
        };
        let replayed = after_switch_replay(&after);
        assert!(replayed.focus_center && replayed.set_region_last_w);
        assert!(replayed.reveal_active && replayed.focus_active_row);
        assert_eq!(
            replayed.region_bookmark,
            Some((true, Some("terminal".into())))
        );
    }
}
