//! User initiated DB work whose results are applied by the loop.

use std::path::Path;
use thegn_core::store::{AccountStore, WorkspaceStore};

#[derive(Debug, Clone)]
pub(crate) enum PendingDbResult {
    AccountAdded {
        seq: u64,
        pane: u32,
        provider: String,
        name: String,
        dir: String,
    },
    ApplyLayout {
        seq: u64,
        tab: String,
        name: String,
        spec: Option<crate::layout_spec::LayoutSpec>,
    },
    Template {
        generation: u64,
        tab: String,
        fallback_commands: Vec<String>,
        spec: Option<crate::layout_spec::LayoutSpec>,
    },
    Dropped {
        seq: u64,
    },
}

#[derive(Debug, Default)]
pub(crate) struct RequestFence {
    next: u64,
    latest_layout: u64,
}

impl RequestFence {
    pub(crate) fn next(&mut self) -> u64 {
        self.next = self.next.wrapping_add(1);
        self.next
    }

    pub(crate) fn begin_layout(&mut self) -> u64 {
        let seq = self.next();
        self.latest_layout = seq;
        seq
    }

    pub(crate) fn accepts_layout(&self, seq: u64) -> bool {
        self.latest_layout == seq
    }
}

#[derive(Debug, Clone)]
pub(crate) enum ApplyLayoutPlan {
    Stale,
    ChangedTab,
    Missing,
    Apply(crate::layout_spec::LayoutSpec),
}

pub(crate) fn plan_apply_layout(
    is_latest: bool,
    active_tab: Option<&str>,
    requested_tab: &str,
    spec: Option<crate::layout_spec::LayoutSpec>,
) -> ApplyLayoutPlan {
    if !is_latest {
        ApplyLayoutPlan::Stale
    } else if active_tab != Some(requested_tab) {
        ApplyLayoutPlan::ChangedTab
    } else if let Some(spec) = spec {
        ApplyLayoutPlan::Apply(spec)
    } else {
        ApplyLayoutPlan::Missing
    }
}

pub(crate) fn account_gone_pane_status(provider: &str, name: &str) -> String {
    format!("Account {provider}:{name} registered; focused pane closed, run the login yourself")
}

pub(crate) fn dropped_layout_status() -> &'static str {
    "Apply layout: database busy, try again"
}

pub(crate) fn template_spec(
    spec: Option<crate::layout_spec::LayoutSpec>,
    fallback_commands: &[String],
) -> Option<crate::layout_spec::LayoutSpec> {
    spec.or_else(|| {
        (!fallback_commands.is_empty())
            .then(|| crate::layout_spec::LayoutSpec::even_split(fallback_commands))
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TemplateTabPlan {
    pub(crate) target: usize,
    pub(crate) restore: Option<usize>,
}

pub(crate) fn template_tab_plan(active: usize, target: usize) -> TemplateTabPlan {
    TemplateTabPlan {
        target,
        restore: (active != target).then_some(active),
    }
}

fn spec_from_db(db: &thegn_core::db::Db, name: &str) -> Option<crate::layout_spec::LayoutSpec> {
    db.get_layout(name)
        .ok()
        .flatten()
        .and_then(|json| crate::layout_spec::LayoutSpec::from_json(&json).ok())
}

fn send_result(
    tx: &tokio::sync::mpsc::UnboundedSender<PendingDbResult>,
    waker: &termwiz::terminal::TerminalWaker,
    result: PendingDbResult,
) {
    if let Err(error) = tx.send(result) {
        tracing::debug!(target: "thegn::db", %error, "pending DB result receiver is gone");
    }
    if let Err(error) = waker.wake() {
        tracing::debug!(target: "thegn::db", %error, "pending DB result wake failed");
    }
}

pub(crate) fn queue_apply_layout(
    seq: u64,
    tab: String,
    name: String,
    tx: &tokio::sync::mpsc::UnboundedSender<PendingDbResult>,
    waker: &termwiz::terminal::TerminalWaker,
) {
    let tx = tx.clone();
    let wake = waker.clone();
    let fallback_tx = tx.clone();
    let fallback_wake = wake.clone();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    crate::db_task::persist(move |db| {
        send_result(
            &tx,
            &wake,
            PendingDbResult::ApplyLayout {
                seq,
                tab,
                name: name.clone(),
                spec: spec_from_db(db, &name),
            },
        );
        if done_tx.send(()).is_err() {
            tracing::debug!(target: "thegn::db", "layout completion receiver is gone");
        }
    });
    tokio::spawn(async move {
        if done_rx.await.is_err() {
            send_result(
                &fallback_tx,
                &fallback_wake,
                PendingDbResult::Dropped { seq },
            );
        }
    });
}

pub(crate) fn queue_template(
    generation: u64,
    tab: String,
    layout_name: Option<String>,
    fallback_commands: Vec<String>,
    tx: &tokio::sync::mpsc::UnboundedSender<PendingDbResult>,
    waker: &termwiz::terminal::TerminalWaker,
) {
    if let Some(name) = layout_name.filter(|name| !name.trim().is_empty()) {
        let job_tx = tx.clone();
        let job_wake = waker.clone();
        let fallback_tx = tx.clone();
        let fallback_wake = waker.clone();
        let job_tab = tab.clone();
        let job_commands = fallback_commands.clone();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        crate::db_task::persist(move |db| {
            send_result(
                &job_tx,
                &job_wake,
                PendingDbResult::Template {
                    generation,
                    tab: job_tab,
                    fallback_commands: job_commands,
                    spec: spec_from_db(db, &name),
                },
            );
            if done_tx.send(()).is_err() {
                tracing::debug!(target: "thegn::db", "template completion receiver is gone");
            }
        });
        tokio::spawn(async move {
            if done_rx.await.is_err() {
                send_result(
                    &fallback_tx,
                    &fallback_wake,
                    PendingDbResult::Template {
                        generation,
                        tab,
                        fallback_commands,
                        spec: None,
                    },
                );
            }
        });
    } else {
        send_result(
            tx,
            waker,
            PendingDbResult::Template {
                generation,
                tab,
                fallback_commands,
                spec: None,
            },
        );
    }
}

pub(crate) fn queue_account_add(
    seq: u64,
    pane: u32,
    provider: String,
    tx: &tokio::sync::mpsc::UnboundedSender<PendingDbResult>,
    waker: &termwiz::terminal::TerminalWaker,
) {
    let tx = tx.clone();
    let wake = waker.clone();
    crate::db_task::persist_critical(move |db| {
        let count = match db.list_accounts(&provider) {
            Ok(accounts) => accounts.len(),
            Err(error) => {
                tracing::warn!(target: "thegn::db", %error, "account list failed before account add");
                return;
            }
        };
        let name = format!("acct{}", count + 1);
        if thegn_core::account::provider(&provider).is_none() {
            return;
        }
        let dir = thegn_core::account::managed_dir(&provider, &name);
        if let Err(error) = std::fs::create_dir_all(&dir) {
            tracing::warn!(target: "thegn::db", %error, "managed account directory creation failed");
            return;
        }
        let dir_string = dir.to_string_lossy().into_owned();
        if let Err(error) =
            db.put_account(&provider, &name, &dir_string, true, thegn_core::util::now())
        {
            tracing::warn!(target: "thegn::db", %error, "account registration failed");
            return;
        }
        send_result(
            &tx,
            &wake,
            PendingDbResult::AccountAdded {
                seq,
                pane,
                provider,
                name,
                dir: dir_string,
            },
        );
    });
}

pub(crate) fn persist_account_binding(worktree: String, provider: String, name: String) {
    crate::db_task::persist_critical(move |db| {
        if let Err(error) = set_account_binding(db, &worktree, &provider, &name) {
            tracing::warn!(target: "thegn::db", %error, "account binding persist failed");
        }
    });
}

fn set_account_binding(
    db: &thegn_core::db::Db,
    worktree: &str,
    provider: &str,
    name: &str,
) -> anyhow::Result<()> {
    let repo_root = match db.repo_root_for(worktree) {
        Ok(root) => root.filter(|root| !root.is_empty()),
        Err(error) => {
            tracing::debug!(target: "thegn::db", %error, "account binding workspace lookup failed; using global scope");
            None
        }
    };
    let slug = repo_root.and_then(|root| {
        let base = Path::new(&root).file_name()?.to_string_lossy().into_owned();
        match db.slug_for_repo(&root, &base) {
            Ok(slug) => Some(slug),
            Err(error) => {
                tracing::debug!(target: "thegn::db", %error, "account binding slug lookup failed; using global scope");
                None
            }
        }
    });
    let bind = if slug.is_some() {
        thegn_core::account::Bind::Workspace
    } else {
        thegn_core::account::Bind::Global
    };
    thegn_core::account::set_active(db, bind, worktree, slug.as_deref(), provider, name)
}

pub(crate) fn persist_bundle_binding(worktree: String, name: Option<String>) {
    crate::db_task::persist_critical(move |db| {
        let slug = super::bundle_scope_slug(db, &worktree);
        let bind = if slug.is_some() {
            thegn_core::bundle::Bind::Workspace
        } else {
            thegn_core::bundle::Bind::Global
        };
        let result = match name {
            Some(name) => {
                thegn_core::bundle::set_active(db, bind, &worktree, slug.as_deref(), &name)
            }
            None => thegn_core::bundle::clear_active(db, bind, &worktree, slug.as_deref()),
        };
        if let Err(error) = result {
            tracing::warn!(target: "thegn::db", %error, "bundle binding persist failed");
        }
    });
}

pub(crate) fn persist_identity_binding(worktree: String, name: Option<String>) {
    crate::db_task::persist_critical(move |db| {
        let slug = super::bundle_scope_slug(db, &worktree);
        let bind = if slug.is_some() {
            thegn_core::bundle::Bind::Workspace
        } else {
            thegn_core::bundle::Bind::Global
        };
        let result = match name {
            Some(name) => {
                thegn_core::identity::set_active(db, bind, &worktree, slug.as_deref(), &name)
            }
            None => thegn_core::identity::clear_active(db, bind, &worktree, slug.as_deref()),
        };
        if let Err(error) = result {
            tracing::warn!(target: "thegn::db", %error, "identity binding persist failed");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_spec() -> crate::layout_spec::LayoutSpec {
        crate::layout_spec::LayoutSpec::even_split(&["one".to_string(), "two".to_string()])
    }

    #[test]
    fn apply_layout_result_for_superseded_seq_is_dropped() {
        let mut fence = RequestFence::default();
        let old = fence.begin_layout();
        let latest = fence.begin_layout();
        assert!(!fence.accepts_layout(old));
        assert!(matches!(
            plan_apply_layout(
                fence.accepts_layout(old),
                Some("repo/main"),
                "repo/main",
                Some(sample_spec())
            ),
            ApplyLayoutPlan::Stale
        ));
        assert!(fence.accepts_layout(latest));
    }

    #[test]
    fn apply_layout_result_for_changed_tab_is_refused() {
        assert!(matches!(
            plan_apply_layout(true, Some("repo/other"), "repo/main", Some(sample_spec())),
            ApplyLayoutPlan::ChangedTab
        ));
    }

    #[test]
    fn template_layout_result_targets_its_own_tab_and_restores_previous() {
        let plan = template_tab_plan(2, 5);
        assert_eq!(plan.target, 5);
        assert_eq!(plan.restore, Some(2));
        assert_eq!(template_tab_plan(5, 5).restore, None);
    }

    #[test]
    fn template_missing_row_falls_back_to_even_split() {
        let commands = vec!["one".to_string(), "two".to_string()];
        assert!(template_spec(None, &commands).is_some());
        assert!(template_spec(None, &[]).is_none());
    }

    #[test]
    fn account_added_to_gone_pane_reports_manual_login() {
        assert_eq!(
            account_gone_pane_status("claude", "acct3"),
            "Account claude:acct3 registered; focused pane closed, run the login yourself"
        );
    }

    #[test]
    fn dropped_layout_job_reports_busy() {
        assert_eq!(
            dropped_layout_status(),
            "Apply layout: database busy, try again"
        );
    }

    #[test]
    fn account_binding_job_helper_sets_workspace_scope() {
        let dir = tempfile::tempdir().unwrap();
        let db = thegn_core::db::Db::open_at(&dir.path().join("thegn.db")).unwrap();
        db.put_workspace("/repo", "repo", "repo").unwrap();
        db.slug_for_repo("/repo", "repo").unwrap();
        db.put_worktree("repo/main", "/repo", "/repo/main", "main", None, None)
            .unwrap();
        set_account_binding(&db, "/repo/main", "claude", "acct1").unwrap();
        assert_eq!(
            thegn_core::account::active_name(
                &thegn_core::config::Config::default(),
                &db,
                "/repo/main",
                Some("repo"),
                "claude"
            ),
            Some("acct1".to_string())
        );
    }
}
