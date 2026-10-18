//! User initiated DB work whose results are applied by the loop.
//!
//! Critical jobs whose database cannot be opened remain retained for retry, so their pending UI status
//! stays until the database returns; this is intentional because those writes have not been lost.

use std::path::Path;
use thegn_core::store::{AccountStore, WorkspaceStore};

struct ResultOnDrop<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> ResultOnDrop<F> {
    fn new(notify_failure: F) -> Self {
        Self(Some(notify_failure))
    }

    fn disarm(&mut self) {
        self.0.take();
    }
}

impl<F: FnOnce()> Drop for ResultOnDrop<F> {
    fn drop(&mut self) {
        if let Some(notify_failure) = self.0.take() {
            notify_failure();
        }
    }
}

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
    WriteFailed {
        status: String,
    },
    PoolTarget {
        status: String,
    },
    HostAdded {
        name: String,
        defs: Vec<(String, thegn_core::host_config::HostConfig)>,
        then: HostAddThen,
    },
    HostAddFailed {
        status: String,
    },
}

#[derive(Debug, Clone)]
pub(crate) enum HostAddThen {
    Wizard { repo_root: std::path::PathBuf },
    Panel,
}

pub(crate) struct HostAddRequest {
    pub(crate) name: String,
    pub(crate) hc: thegn_core::host_config::HostConfig,
    pub(crate) cfg_has_name: bool,
    pub(crate) then: HostAddThen,
    pub(crate) now: i64,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PresetApplyOutcome {
    Applied,
    NothingSpawned,
}

pub(crate) fn plan_preset_apply(spawned: bool) -> PresetApplyOutcome {
    if spawned {
        PresetApplyOutcome::Applied
    } else {
        PresetApplyOutcome::NothingSpawned
    }
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

fn report_write_result(
    result: Result<(), String>,
    on_err: impl FnOnce(String) -> String,
    notify: impl FnOnce(PendingDbResult),
) {
    if let Err(error) = result {
        notify(PendingDbResult::WriteFailed {
            status: on_err(error),
        });
    }
}

pub(crate) fn queue_reported_write(
    tx: &tokio::sync::mpsc::UnboundedSender<PendingDbResult>,
    waker: &termwiz::terminal::TerminalWaker,
    job: impl FnOnce(&thegn_core::db::Db) -> Result<(), String> + Send + 'static,
    on_err: impl FnOnce(String) -> String + Send + 'static,
) {
    let tx = tx.clone();
    let waker = waker.clone();
    crate::db_task::persist_critical(move |db| {
        report_write_result(job(db), on_err, |result| send_result(&tx, &waker, result));
    });
}

pub(crate) fn queue_pool_target_adjust(
    tx: &tokio::sync::mpsc::UnboundedSender<PendingDbResult>,
    waker: &termwiz::terminal::TerminalWaker,
    cfg: thegn_core::config::Config,
    wt: String,
    delta: i64,
) {
    crate::db_task::persist_critical(pool_target_job(tx, waker, cfg, wt, delta));
}

fn pool_target_job(
    tx: &tokio::sync::mpsc::UnboundedSender<PendingDbResult>,
    waker: &termwiz::terminal::TerminalWaker,
    cfg: thegn_core::config::Config,
    wt: String,
    delta: i64,
) -> impl FnOnce(&thegn_core::db::Db) + Send + 'static {
    let tx = tx.clone();
    let waker = waker.clone();
    let failure_tx = tx.clone();
    let failure_waker = waker.clone();
    let mut result_on_drop = ResultOnDrop::new(move || {
        send_result(
            &failure_tx,
            &failure_waker,
            PendingDbResult::PoolTarget {
                status: "pool target: not saved (database writer unavailable)".into(),
            },
        );
    });
    move |db| {
        run_pool_target_adjust(db, &cfg, &wt, delta, |result| {
            send_result(&tx, &waker, result);
            result_on_drop.disarm();
        });
    }
}

fn run_pool_target_adjust(
    db: &thegn_core::db::Db,
    cfg: &thegn_core::config::Config,
    wt: &str,
    delta: i64,
    notify: impl FnOnce(PendingDbResult),
) {
    let status = crate::handlers::provision::adjust_pool_target(db, cfg, wt, delta);
    notify(PendingDbResult::PoolTarget { status });
}

pub(crate) fn queue_host_add(
    tx: &tokio::sync::mpsc::UnboundedSender<PendingDbResult>,
    waker: &termwiz::terminal::TerminalWaker,
    request: HostAddRequest,
) {
    crate::db_task::persist_critical(host_add_job(tx, waker, request));
}

fn host_add_job(
    tx: &tokio::sync::mpsc::UnboundedSender<PendingDbResult>,
    waker: &termwiz::terminal::TerminalWaker,
    request: HostAddRequest,
) -> impl FnOnce(&thegn_core::db::Db) + Send + 'static {
    let tx = tx.clone();
    let waker = waker.clone();
    let failure_tx = tx.clone();
    let failure_waker = waker.clone();
    let mut result_on_drop = ResultOnDrop::new(move || {
        send_result(
            &failure_tx,
            &failure_waker,
            PendingDbResult::HostAddFailed {
                status: "add host: not saved (database writer unavailable)".into(),
            },
        );
    });
    move |db| {
        let result = crate::handlers::wizard::store_host_def(
            db,
            &request.name,
            &request.hc,
            request.cfg_has_name,
            request.now,
        );
        run_host_add(request, result, |result| {
            send_result(&tx, &waker, result);
            result_on_drop.disarm();
        });
    }
}

fn run_host_add(
    request: HostAddRequest,
    result: Result<Vec<(String, thegn_core::host_config::HostConfig)>, String>,
    notify: impl FnOnce(PendingDbResult),
) {
    match result {
        Ok(defs) => notify(PendingDbResult::HostAdded {
            name: request.name,
            defs,
            then: request.then,
        }),
        Err(error) => notify(PendingDbResult::HostAddFailed {
            status: format!("add host: {error}"),
        }),
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
    fn reported_write_sends_exactly_one_failure_and_nothing_on_success() {
        let mut results = Vec::new();
        report_write_result(
            Err("locked".into()),
            |error| format!("failed: {error}"),
            |r| results.push(r),
        );
        report_write_result(
            Ok(()),
            |error| format!("failed: {error}"),
            |r| results.push(r),
        );
        assert!(matches!(
            results.as_slice(),
            [PendingDbResult::WriteFailed { status }] if status == "failed: locked"
        ));
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
    fn preset_apply_plan_only_marks_spawned_layouts_as_applied() {
        assert_eq!(plan_preset_apply(true), PresetApplyOutcome::Applied);
        assert_eq!(plan_preset_apply(false), PresetApplyOutcome::NothingSpawned);
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

    #[test]
    fn pool_target_result_is_sent_on_success_and_failure() {
        use thegn_core::store::WorkspaceStore as _;

        let db = thegn_core::db::Db::open_memory().unwrap();
        db.put_worktree(
            "repo/main",
            "/repo",
            "/remote/wt",
            "main",
            Some(r#"{"control_prefix":["provider"],"path":"/remote/wt"}"#),
            None,
        )
        .unwrap();
        let cfg = thegn_core::config::Config::default();
        let mut results = Vec::new();
        run_pool_target_adjust(&db, &cfg, "/remote/wt", 1, |result| results.push(result));
        assert!(matches!(
            results.as_slice(),
            [PendingDbResult::PoolTarget { status }]
                if status == "warm pool target: 1 spare(s)"
        ));

        let db = thegn_core::db::Db::open_memory().unwrap();
        let mut results = Vec::new();
        run_pool_target_adjust(&db, &cfg, "/local/wt", 1, |result| results.push(result));
        assert!(matches!(
            results.as_slice(),
            [PendingDbResult::PoolTarget { status }]
                if status == "warm pool: only for provider (remote) workspaces"
        ));
    }

    #[test]
    fn store_host_def_persists_and_preserves_the_config_duplicate_rule() {
        use thegn_core::store::HostStore as _;

        let (name, hc) =
            thegn_core::host_config::parse_host_target("alice@host.example", None).unwrap();
        let db = thegn_core::db::Db::open_memory().unwrap();
        let defs = crate::handlers::wizard::store_host_def(&db, &name, &hc, false, 123).unwrap();
        assert!(
            defs.iter()
                .any(|(stored, config)| stored == &name && config.ssh.host == hc.ssh.host)
        );

        let (config_name, config_hc) =
            thegn_core::host_config::parse_host_target("bob@config-only.example", None).unwrap();
        let duplicate =
            crate::handlers::wizard::store_host_def(&db, &config_name, &config_hc, true, 124);
        assert!(matches!(duplicate, Err(error) if error.contains("already exists in config.toml")));
        assert!(
            db.host_defs()
                .unwrap()
                .iter()
                .any(|(stored, _)| stored == &name)
        );
        assert!(
            !db.host_defs()
                .unwrap()
                .iter()
                .any(|(stored, _)| stored == &config_name)
        );

        let readded = crate::handlers::wizard::store_host_def(&db, &name, &hc, true, 125).unwrap();
        assert!(
            readded
                .iter()
                .any(|(stored, config)| stored == &name && config.ssh.host == hc.ssh.host)
        );
    }

    #[test]
    fn host_add_result_is_sent_once_on_success_and_failure() {
        let (name, hc) =
            thegn_core::host_config::parse_host_target("alice@host.example", None).unwrap();
        let request = |name: String, hc: thegn_core::host_config::HostConfig| HostAddRequest {
            name,
            hc,
            cfg_has_name: false,
            then: HostAddThen::Panel,
            now: 123,
        };
        let mut results = Vec::new();
        run_host_add(
            request(name.clone(), hc.clone()),
            Ok(vec![(name.clone(), hc.clone())]),
            |result| results.push(result),
        );
        assert!(matches!(
            results.as_slice(),
            [PendingDbResult::HostAdded { name: added, defs, then: HostAddThen::Panel }]
                if added == &name
                    && matches!(defs.as_slice(), [(stored, config)]
                        if stored == &name && config.ssh.host == hc.ssh.host)
        ));

        results.clear();
        run_host_add(request(name, hc), Err("write failed".into()), |result| {
            results.push(result)
        });
        assert!(matches!(
            results.as_slice(),
            [PendingDbResult::HostAddFailed { status }]
                if status == "add host: write failed"
        ));
    }

    #[test]
    fn host_add_submit_does_not_block_on_a_held_write_lock() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }

        let state = crate::testenv::IsolatedState::new();
        drop(thegn_core::db::Db::open().unwrap());
        let lock = rusqlite::Connection::open(state.state_home().join("thegn/thegn.db")).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (waker, _master, _terminal) =
            crate::platform::sound_process::test_support::test_waker();
        let started = std::time::Instant::now();
        let status = crate::handlers::host::queue_host_add_from_input(
            "alice@host.example",
            &thegn_core::config::Config::default(),
            &tx,
            &waker,
        );
        assert_eq!(status, "add host: saving…");
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));

        lock.execute_batch("ROLLBACK").unwrap();
        drop(lock);
        let (barrier_tx, barrier_rx) = std::sync::mpsc::channel();
        crate::db_task::persist_critical(move |_| {
            if barrier_tx.send(()).is_err() {
                tracing::debug!(target: "thegn::host", "host-add barrier receiver closed");
            }
        });
        barrier_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        assert!(matches!(
            rx.try_recv(),
            Ok(PendingDbResult::HostAdded { name, defs, then: HostAddThen::Panel })
                if name == "host-example" && defs.iter().any(|(stored, _)| stored == "host-example")
        ));
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn dropped_host_add_job_reports_unavailable_once_and_running_job_reports_added_once() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (waker, _master, _terminal) =
            crate::platform::sound_process::test_support::test_waker();
        let (name, hc) =
            thegn_core::host_config::parse_host_target("alice@host.example", None).unwrap();
        let request = HostAddRequest {
            name: name.clone(),
            hc: hc.clone(),
            cfg_has_name: false,
            then: HostAddThen::Panel,
            now: 123,
        };

        drop(host_add_job(&tx, &waker, request));
        assert!(matches!(
            rx.try_recv(),
            Ok(PendingDbResult::HostAddFailed { status })
                if status == "add host: not saved (database writer unavailable)"
        ));
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));

        let db = thegn_core::db::Db::open_memory().unwrap();
        host_add_job(
            &tx,
            &waker,
            HostAddRequest {
                name: name.clone(),
                hc,
                cfg_has_name: false,
                then: HostAddThen::Panel,
                now: 123,
            },
        )(&db);
        assert!(matches!(
            rx.try_recv(),
            Ok(PendingDbResult::HostAdded { name: added, .. }) if added == name
        ));
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn dropped_pool_target_job_reports_unavailable_once_and_running_job_reports_once() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (waker, _master, _terminal) =
            crate::platform::sound_process::test_support::test_waker();

        drop(pool_target_job(
            &tx,
            &waker,
            thegn_core::config::Config::default(),
            "/local/wt".into(),
            1,
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(PendingDbResult::PoolTarget { status })
                if status == "pool target: not saved (database writer unavailable)"
        ));
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));

        let db = thegn_core::db::Db::open_memory().unwrap();
        pool_target_job(
            &tx,
            &waker,
            thegn_core::config::Config::default(),
            "/local/wt".into(),
            1,
        )(&db);
        assert!(matches!(
            rx.try_recv(),
            Ok(PendingDbResult::PoolTarget { status })
                if status == "warm pool: only for provider (remote) workspaces"
        ));
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn closed_writer_drops_host_add_job_and_guard_reports_failure() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (waker, _master, _terminal) =
            crate::platform::sound_process::test_support::test_waker();
        let (name, hc) =
            thegn_core::host_config::parse_host_target("alice@host.example", None).unwrap();

        assert!(!crate::db_task::persist_critical_to_closed_writer_for_test(
            host_add_job(
                &tx,
                &waker,
                HostAddRequest {
                    name,
                    hc,
                    cfg_has_name: false,
                    then: HostAddThen::Panel,
                    now: 123,
                },
            ),
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(PendingDbResult::HostAddFailed { status })
                if status == "add host: not saved (database writer unavailable)"
        ));
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }
}
