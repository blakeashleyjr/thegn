//! User initiated DB work whose results are applied by the loop.

use std::path::Path;
use thegn_core::store::{AccountStore, WorkspaceStore};

#[derive(Debug, Clone)]
pub(crate) enum PendingDbResult {
    AccountAdded {
        pane: u32,
        provider: String,
        name: String,
        dir: String,
    },
    AccountAddFailed {
        provider: String,
        error: String,
    },
    ApplyLayout {
        seq: u64,
        tab: String,
        name: String,
        spec: Option<crate::layout_spec::LayoutSpec>,
    },
    Preset {
        seq: u64,
        group: String,
        worktree: String,
        preset: thegn_core::config::Preset,
        spec: Option<crate::layout_spec::LayoutSpec>,
    },
    Template {
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
    fn next(&mut self) -> u64 {
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

#[derive(Debug, Clone)]
pub(crate) enum PresetDrainPlan {
    Stale,
    ChangedWorktree,
    Apply(crate::layout_spec::LayoutSpec),
    FallBackToCommands,
    NothingToLaunch,
}

pub(crate) fn plan_preset_drain(
    accepts: bool,
    active_worktree: Option<&str>,
    worktree: &str,
    spec: Option<crate::layout_spec::LayoutSpec>,
    has_commands: bool,
) -> PresetDrainPlan {
    if !accepts {
        PresetDrainPlan::Stale
    } else if active_worktree != Some(worktree) {
        PresetDrainPlan::ChangedWorktree
    } else if let Some(spec) = spec {
        PresetDrainPlan::Apply(spec)
    } else if has_commands {
        PresetDrainPlan::FallBackToCommands
    } else {
        PresetDrainPlan::NothingToLaunch
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccountAddDrainStatus {
    LoginQueued,
    PaneNotReading,
    PaneClosed,
}

pub(crate) fn account_add_drain_status(pane_exists: bool, write_ok: bool) -> AccountAddDrainStatus {
    match (pane_exists, write_ok) {
        (false, _) => AccountAddDrainStatus::PaneClosed,
        (true, true) => AccountAddDrainStatus::LoginQueued,
        (true, false) => AccountAddDrainStatus::PaneNotReading,
    }
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

#[derive(Debug, Clone)]
pub(crate) struct TemplateDrainPlan {
    pub(crate) target: usize,
    pub(crate) restore: Option<usize>,
    pub(crate) spec: Option<crate::layout_spec::LayoutSpec>,
}

pub(crate) fn template_tab_plan(active: usize, target: usize) -> TemplateTabPlan {
    TemplateTabPlan {
        target,
        restore: (active != target).then_some(active),
    }
}

pub(crate) fn plan_template_drain(
    active: usize,
    target: usize,
    spec: Option<crate::layout_spec::LayoutSpec>,
    fallback_commands: &[String],
) -> TemplateDrainPlan {
    let tabs = template_tab_plan(active, target);
    TemplateDrainPlan {
        target: tabs.target,
        restore: tabs.restore,
        spec: template_spec(spec, fallback_commands),
    }
}

fn spec_from_db(db: &thegn_core::db::Db, name: &str) -> Option<crate::layout_spec::LayoutSpec> {
    db.get_layout(name)
        .ok()
        .flatten()
        .and_then(|json| crate::layout_spec::LayoutSpec::from_json(&json).ok())
}

fn preset_result_from_db(
    db: &thegn_core::db::Db,
    seq: u64,
    group: String,
    worktree: String,
    preset: thegn_core::config::Preset,
) -> PendingDbResult {
    let spec = preset.layout_name().and_then(|name| spec_from_db(db, name));
    PendingDbResult::Preset {
        seq,
        group,
        worktree,
        preset,
        spec,
    }
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

async fn send_dropped_result(
    done_rx: tokio::sync::oneshot::Receiver<()>,
    seq: u64,
    tx: &tokio::sync::mpsc::UnboundedSender<PendingDbResult>,
) -> bool {
    if done_rx.await.is_ok() {
        return false;
    }
    if let Err(error) = tx.send(PendingDbResult::Dropped { seq }) {
        tracing::debug!(target: "thegn::db", %error, "pending DB result receiver is gone");
        return false;
    }
    true
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
        if send_dropped_result(done_rx, seq, &fallback_tx).await
            && let Err(error) = fallback_wake.wake()
        {
            tracing::debug!(target: "thegn::db", %error, "pending DB result wake failed");
        }
    });
}

pub(crate) fn queue_preset_layout(
    seq: u64,
    group: String,
    worktree: String,
    preset: thegn_core::config::Preset,
    tx: &tokio::sync::mpsc::UnboundedSender<PendingDbResult>,
    waker: &termwiz::terminal::TerminalWaker,
) {
    let job_tx = tx.clone();
    let job_wake = waker.clone();
    let fallback_tx = tx.clone();
    let fallback_wake = waker.clone();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    crate::db_task::persist(move |db| {
        send_result(
            &job_tx,
            &job_wake,
            preset_result_from_db(db, seq, group, worktree, preset),
        );
        if done_tx.send(()).is_err() {
            tracing::debug!(target: "thegn::db", "preset completion receiver is gone");
        }
    });
    tokio::spawn(async move {
        if send_dropped_result(done_rx, seq, &fallback_tx).await
            && let Err(error) = fallback_wake.wake()
        {
            tracing::debug!(target: "thegn::db", %error, "pending DB result wake failed");
        }
    });
}

pub(crate) fn queue_template(
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
                tab,
                fallback_commands,
                spec: None,
            },
        );
    }
}

pub(crate) fn queue_account_add(
    pane: u32,
    provider: String,
    tx: &tokio::sync::mpsc::UnboundedSender<PendingDbResult>,
    waker: &termwiz::terminal::TerminalWaker,
) {
    let tx = tx.clone();
    let wake = waker.clone();
    crate::db_task::persist_critical(move |db| {
        let result = add_account(db, &provider);
        let message = match result {
            Ok((name, dir)) => PendingDbResult::AccountAdded {
                pane,
                provider,
                name,
                dir,
            },
            Err(error) => {
                tracing::warn!(target: "thegn::db", %error, "account add failed");
                PendingDbResult::AccountAddFailed {
                    provider,
                    error: error.to_string(),
                }
            }
        };
        send_result(&tx, &wake, message);
    });
}

fn add_account(db: &thegn_core::db::Db, provider: &str) -> anyhow::Result<(String, String)> {
    if thegn_core::account::provider(provider).is_none() {
        anyhow::bail!("unknown account provider \"{provider}\"");
    }
    let count = db.list_accounts(provider)?.len();
    let name = format!("acct{}", count + 1);
    let dir = thegn_core::account::managed_dir(provider, &name);
    std::fs::create_dir_all(&dir)?;
    let dir_string = dir.to_string_lossy().into_owned();
    db.put_account(provider, &name, &dir_string, true, thegn_core::util::now())?;
    Ok((name, dir_string))
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
    fn preset_drain_plan_covers_fence_worktree_layout_and_fallback() {
        let worktree = "/repo/main";
        let spec = sample_spec();
        let cases = [
            (false, Some(worktree), Some(spec.clone()), true, "stale"),
            (
                true,
                Some("/repo/other"),
                Some(spec.clone()),
                true,
                "changed",
            ),
            (true, Some(worktree), Some(spec.clone()), false, "apply"),
            (true, Some(worktree), None, true, "commands"),
            (true, Some(worktree), None, false, "nothing"),
        ];
        for (accepts, active, candidate, commands, expected) in cases {
            let actual = plan_preset_drain(accepts, active, worktree, candidate, commands);
            match expected {
                "stale" => assert!(matches!(actual, PresetDrainPlan::Stale)),
                "changed" => assert!(matches!(actual, PresetDrainPlan::ChangedWorktree)),
                "apply" => assert!(matches!(actual, PresetDrainPlan::Apply(_))),
                "commands" => assert!(matches!(actual, PresetDrainPlan::FallBackToCommands)),
                "nothing" => assert!(matches!(actual, PresetDrainPlan::NothingToLaunch)),
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn preset_result_reads_saved_layout_and_keeps_request_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let db = thegn_core::db::Db::open_at(&dir.path().join("thegn.db")).unwrap();
        let expected = sample_spec();
        db.put_layout("saved", &expected.to_json().unwrap())
            .unwrap();
        let preset = thegn_core::config::Preset {
            name: "test".into(),
            layout: Some("saved".into()),
            ..Default::default()
        };
        assert!(matches!(
            preset_result_from_db(&db, 17, "repo/main".into(), "/repo/main".into(), preset),
            PendingDbResult::Preset {
                seq: 17,
                spec: Some(spec),
                ..
            } if spec == expected
        ));
    }

    #[test]
    fn template_drain_plan_applies_to_its_tab_and_restores_previous() {
        let commands = vec!["one".to_string(), "two".to_string()];
        let plan = plan_template_drain(2, 5, None, &commands);
        assert_eq!(plan.target, 5);
        assert_eq!(plan.restore, Some(2));
        assert!(plan.spec.is_some());
        let same_tab = plan_template_drain(5, 5, None, &commands);
        assert_eq!(same_tab.restore, None);
    }

    #[test]
    fn dropped_layout_fallback_sends_a_dropped_result() {
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        drop(done_tx);
        let (result_tx, mut result_rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(async { send_dropped_result(done_rx, 7, &result_tx).await });
        assert!(matches!(
            result_rx.try_recv(),
            Ok(PendingDbResult::Dropped { seq: 7 })
        ));
    }

    #[test]
    fn template_missing_row_falls_back_to_even_split() {
        let commands = vec!["one".to_string(), "two".to_string()];
        assert!(template_spec(None, &commands).is_some());
        assert!(template_spec(None, &[]).is_none());
    }

    #[test]
    fn account_add_drain_distinguishes_closed_and_unwritable_panes() {
        assert_eq!(
            account_add_drain_status(false, false),
            AccountAddDrainStatus::PaneClosed
        );
        assert_eq!(
            account_add_drain_status(true, false),
            AccountAddDrainStatus::PaneNotReading
        );
        assert_eq!(
            account_add_drain_status(true, true),
            AccountAddDrainStatus::LoginQueued
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
        assert_eq!(
            thegn_core::account::active_name(
                &thegn_core::config::Config::default(),
                &db,
                "/elsewhere",
                None,
                "claude"
            ),
            None
        );
    }

    #[test]
    fn account_add_helper_rejects_unknown_provider_with_db_open_at() {
        let dir = tempfile::tempdir().unwrap();
        let db = thegn_core::db::Db::open_at(&dir.path().join("thegn.db")).unwrap();
        let error = add_account(&db, "not-a-provider").unwrap_err();
        assert!(error.to_string().contains("unknown account provider"));
    }
}
