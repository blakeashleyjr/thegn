//! `thegn session move` — migrate one persisted session presentation between
//! profile databases. The operation is intentionally host-local: it never
//! reroots the process, loads target configuration, or becomes a daemon API.

use anyhow::{Context, Result, anyhow, bail};
use futures::future::BoxFuture;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::outln;
use thegn_core::profile::{self, ProfilePaths};
use thegn_core::session_migration::{MigrationBundle, MigrationCounts, plan_migration};
use thegn_core::store::{ControlStore, SessionMigrationStore};
use thegn_svc::control::SessionInfo;
use thegn_svc::control::client::ControlClient;

use super::session::SessionAction;

const OPAQUE_PAYLOAD_WARNING: &str = "opaque pane commands, scrollback, dispatch reports, and notes are carried unchanged and are not included in this audit";

trait MigrationControl {
    fn health(&self) -> BoxFuture<'_, Result<()>>;
    fn sessions(&self) -> BoxFuture<'_, Result<Vec<SessionInfo>>>;
    fn kill(&self, session: &str) -> BoxFuture<'_, Result<()>>;
    fn notify_push(&self, note: &thegn_svc::control::PushedNote) -> BoxFuture<'_, Result<i64>>;
}

impl MigrationControl for ControlClient {
    fn health(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move { ControlClient::health(self).await })
    }

    fn sessions(&self) -> BoxFuture<'_, Result<Vec<SessionInfo>>> {
        Box::pin(async move { ControlClient::sessions(self).await })
    }

    fn kill(&self, session: &str) -> BoxFuture<'_, Result<()>> {
        let session = session.to_string();
        Box::pin(async move { ControlClient::kill(self, &session).await })
    }

    fn notify_push(&self, note: &thegn_svc::control::PushedNote) -> BoxFuture<'_, Result<i64>> {
        let note = note.clone();
        Box::pin(async move { ControlClient::notify_push(self, &note).await })
    }
}

/// The stable, payload-free audit emitted by a move. Opaque commands,
/// scrollback, reports, notes, and credentials are deliberately absent.
#[derive(Debug, Serialize)]
struct MigrationAudit {
    source_profile: String,
    target_profile: String,
    worktree: String,
    groups: Vec<String>,
    counts: MigrationCounts,
    live_ids: Vec<String>,
    killed_ids: Vec<String>,
    target_dispatch_ids: Vec<i64>,
    dry_run: bool,
    target_committed: bool,
    target_confirmed: bool,
    source_deleted: bool,
    resumed: bool,
    opaque_payload_warning: String,
    notification: NotificationAudit,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct NotificationAudit {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<String>,
}

impl MigrationAudit {
    fn new(source: &str, target: &str, worktree: &str, dry_run: bool) -> Self {
        Self {
            source_profile: source.to_string(),
            target_profile: target.to_string(),
            worktree: worktree.to_string(),
            groups: Vec::new(),
            counts: MigrationCounts::default(),
            live_ids: Vec::new(),
            killed_ids: Vec::new(),
            target_dispatch_ids: Vec::new(),
            dry_run,
            target_committed: false,
            target_confirmed: false,
            source_deleted: false,
            resumed: false,
            opaque_payload_warning: OPAQUE_PAYLOAD_WARNING.to_string(),
            notification: NotificationAudit {
                status: "not_attempted".to_string(),
                warning: None,
            },
            error: None,
        }
    }
}

/// Entry point called before the ordinary session daemon connection is made.
pub async fn run(_cfg: &Config, action: SessionAction) -> Result<()> {
    let SessionAction::Move {
        worktree,
        to_profile,
        kill,
        dry_run,
        json,
    } = action
    else {
        unreachable!("session_move::run called for a non-move action")
    };

    let source = profile::active();
    let target = profile::resolve_active_target(&to_profile)?;
    let mut audit = MigrationAudit::new(&source.name, &target.name, &worktree, dry_run);

    let target_db_path = profile_db_path(&target, &profile::default_state_home());
    let (source_db, target_db) = with_source_instance_guard(&source, || {
        let source_db = if dry_run {
            Db::open_read_only()
                .context("open source profile database read-only")?
                .ok_or_else(|| anyhow!("source profile database does not exist"))?
        } else {
            Db::open().context("open source profile database")?
        };
        let target_db = if dry_run {
            Db::open_read_only_at(&target_db_path).with_context(|| {
                format!(
                    "open target profile database read-only at {}",
                    target_db_path.display()
                )
            })?
        } else {
            Db::open_read_only_wal_at(&target_db_path).with_context(|| {
                format!(
                    "open target profile database for preflight at {}",
                    target_db_path.display()
                )
            })?
        };
        Ok((source_db, target_db))
    })?;
    let session = thegn_core::db::session();
    let bundle = source_db.migration_snapshot(&source.name, &target.name, &session, &worktree)?;
    fill_bundle_audit(&mut audit, &bundle);
    if bundle.worktree.is_none() && bundle.groups.is_empty() && bundle.dispatches.is_empty() {
        return fail(
            &mut audit,
            json,
            "worktree path is not registered and has no session groups or dispatches",
            false,
        );
    }

    let target_state = target_db
        .as_ref()
        .map(|db| db.migration_target_snapshot(&session, &worktree))
        .transpose()?
        .unwrap_or_default();
    let plan = match plan_migration(bundle, target_state) {
        Ok(plan) => plan,
        Err(conflict) => return fail(&mut audit, json, &conflict.to_string(), false),
    };
    let refs = referenced_session_ids(&plan.bundle);
    audit.resumed = plan.resumed;

    let (source_client, listed) = source_daemon(&source_db, &refs).await?;
    let live_ids = live_session_ids(&listed, &refs, &worktree);
    audit.live_ids = live_ids.iter().cloned().collect();

    if dry_run {
        if !audit.live_ids.is_empty() && !kill {
            audit.notification.warning = Some(
                "live source sessions require --kill for a real move; dry-run did not kill them"
                    .to_string(),
            );
        } else if !audit.live_ids.is_empty() {
            audit.notification.warning = Some(
                "--kill would terminate these live source sessions; dry-run did not kill them"
                    .to_string(),
            );
        }
        return report(&audit, json);
    }

    if let Some(message) = live_move_refusal(&live_ids, kill) {
        return fail(&mut audit, json, message, false);
    }
    if !live_ids.is_empty() {
        let Some(client) = source_client.as_deref() else {
            return fail(
                &mut audit,
                json,
                "live source sessions found but the source daemon is unavailable",
                false,
            );
        };
        if let Err(error) = kill_and_relist(client, &live_ids, &refs, &worktree, &mut audit).await {
            return fail(&mut audit, json, &error.to_string(), false);
        }
    }

    // No writable target handle exists until every conflict/liveness check and
    // requested kill has completed. In particular, a refused live move cannot
    // create or migrate the target database.
    drop(target_db);
    let target_db = Db::open_at(&target_db_path).with_context(|| {
        format!(
            "open target profile database at {}",
            target_db_path.display()
        )
    })?;
    if let Err(error) = commit_target_then_cleanup(&source_db, &target_db, &plan, &mut audit) {
        let retryable = audit.target_committed;
        return fail(&mut audit, json, &error.to_string(), retryable);
    }

    audit.notification = notify_target(
        &target_db_path,
        &target_db,
        &source.name,
        &target.name,
        &worktree,
    )
    .await;
    report(&audit, json)
}

/// Check the source compositor's existing profile lock before opening either
/// migration database. The closure is deliberately the only DB-open boundary
/// so the no-race ordering is testable without touching a live state root.
fn with_source_instance_guard<T>(
    source: &ProfilePaths,
    open_databases: impl FnOnce() -> Result<T>,
) -> Result<T> {
    if profile::instance_running_at(source) {
        bail!("the source profile has another interactive instance running");
    }
    open_databases()
}

fn fill_bundle_audit(audit: &mut MigrationAudit, bundle: &MigrationBundle) {
    audit.groups = bundle
        .groups
        .iter()
        .map(|group| group.name.clone())
        .collect();
    audit.counts = MigrationCounts {
        worktrees: usize::from(bundle.worktree.is_some()),
        tab_groups: bundle.groups.len(),
        group_tabs: bundle.groups.iter().map(|group| group.tabs.len()).sum(),
        ui_state: bundle.ui_state.len(),
        dispatches: bundle.dispatches.len(),
        dispatch_notes: bundle.notes.len(),
        attention: 0,
    };
}

fn commit_target_then_cleanup(
    source_db: &dyn SessionMigrationStore,
    target_db: &dyn SessionMigrationStore,
    plan: &thegn_core::session_migration::MigrationPlan,
    audit: &mut MigrationAudit,
) -> Result<()> {
    let imported = target_db
        .import_migration(plan)
        .map_err(|error| anyhow!("target profile import failed before commit: {error}"))?;
    audit.target_committed = true;
    audit.target_dispatch_ids = if imported.dispatch_id_map.is_empty() {
        plan.target
            .dispatches
            .iter()
            .map(|dispatch| dispatch.source_id)
            .collect()
    } else {
        imported.dispatch_id_map.values().copied().collect()
    };
    audit.target_dispatch_ids.sort_unstable();

    if !target_db.confirm_migration(plan)? {
        bail!("target import read-back did not match its sanitized fingerprint");
    }
    audit.target_confirmed = true;

    let cleanup = source_db
        .cleanup_migration(&plan.bundle)
        .map_err(|error| anyhow!("source cleanup is pending after target confirmation: {error}"))?;
    audit.source_deleted = cleanup.source_deleted;
    Ok(())
}

fn referenced_session_ids(bundle: &MigrationBundle) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    for tab in bundle.groups.iter().flat_map(|group| &group.tabs) {
        pane_session_ids(&tab.pane_sessions, &mut ids);
    }
    ids.extend(
        bundle
            .dispatches
            .iter()
            .filter_map(|dispatch| dispatch.session_id.clone()),
    );
    ids
}

fn pane_session_ids(raw: &str, ids: &mut BTreeSet<String>) {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return;
    };
    match value {
        Value::String(id) => {
            ids.insert(id);
        }
        Value::Object(entries) => {
            for entry in entries.values() {
                if let Value::Object(session) = entry
                    && let Some(id) = session.get("session").and_then(Value::as_str)
                    && session
                        .get("provider")
                        .and_then(Value::as_str)
                        .is_none_or(|provider| provider == "daemon")
                {
                    ids.insert(id.to_string());
                }
            }
        }
        _ => {}
    }
}

fn live_session_ids(
    sessions: &[SessionInfo],
    references: &BTreeSet<String>,
    worktree: &str,
) -> BTreeSet<String> {
    sessions
        .iter()
        .filter(|session| {
            session.exited_at_ms.is_none()
                && (session.worktree.as_deref() == Some(worktree)
                    || references.contains(&session.id))
        })
        .map(|session| session.id.clone())
        .collect()
}

fn live_move_refusal(live_ids: &BTreeSet<String>, kill: bool) -> Option<&'static str> {
    (!live_ids.is_empty() && !kill).then_some("live source sessions found; rerun with --kill")
}

async fn kill_and_relist(
    control: &dyn MigrationControl,
    live_ids: &BTreeSet<String>,
    references: &BTreeSet<String>,
    worktree: &str,
    audit: &mut MigrationAudit,
) -> Result<()> {
    for id in live_ids {
        control
            .kill(id)
            .await
            .with_context(|| format!("kill source daemon session {id}"))?;
        audit.killed_ids.push(id.clone());
    }
    let after = control
        .sessions()
        .await
        .context("confirm source daemon sessions were killed")?;
    let survivors = live_session_ids(&after, references, worktree);
    if survivors.is_empty() {
        Ok(())
    } else {
        bail!(
            "source daemon sessions survived --kill: {}",
            survivors.into_iter().collect::<Vec<_>>().join(", ")
        )
    }
}

/// List a source daemon only when its active DB registry says one is live.
/// A live registry row that cannot be reached is a fail-closed condition: the
/// migration cannot safely prove that a referenced session has stopped.
async fn source_daemon(
    db: &Db,
    referenced: &BTreeSet<String>,
) -> Result<(Option<Box<dyn MigrationControl>>, Vec<SessionInfo>)> {
    let scope = crate::daemon::scope_key();
    let now = now_ms();
    let registered = db
        .daemons()?
        .into_iter()
        .filter(|daemon| daemon.scope == scope)
        .collect::<Vec<_>>();
    let live_registered = db.live_daemons(
        &scope,
        now,
        thegn_svc::control::client::DAEMON_HEARTBEAT_TTL_MS,
    )?;
    if live_registered.is_empty() {
        if !referenced.is_empty()
            && let Some(daemon) = registered.iter().max_by_key(|daemon| daemon.heartbeat_at)
        {
            let client = Box::new(ControlClient::new(
                thegn_svc::control::client::ControlAddr::Unix(daemon.endpoint.clone().into()),
            ));
            let sessions = checked_sessions(client.as_ref()).await.with_context(
                || "source daemon is registered but unreachable while referenced sessions exist",
            )?;
            return Ok((Some(client), sessions));
        }
        return Ok((None, Vec::new()));
    }

    let addr = thegn_svc::control::client::discover(db, &scope, now)
        .ok_or_else(|| anyhow!("source daemon is registered but not discoverable"))?;
    let client = Box::new(ControlClient::new(addr));
    let sessions = checked_sessions(client.as_ref())
        .await
        .context("source daemon is registered but unreachable; refusing migration")?;
    // Keep this assertion explicit: if a future registry implementation can
    // return a live row without `discover` seeing it, do not silently proceed.
    if registered.is_empty() {
        bail!("source daemon registry changed during migration preflight")
    }
    Ok((Some(client), sessions))
}

async fn checked_sessions(control: &dyn MigrationControl) -> Result<Vec<SessionInfo>> {
    control.health().await?;
    control
        .sessions()
        .await
        .context("list source daemon sessions")
}

async fn notify_target(
    target_db_path: &Path,
    target_db: &Db,
    source_profile: &str,
    target_profile: &str,
    worktree: &str,
) -> NotificationAudit {
    let scope = target_db_path
        .parent()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    let Some(addr) = thegn_svc::control::client::discover(target_db, &scope, now_ms()) else {
        return NotificationAudit {
            status: "unavailable".to_string(),
            warning: Some("target daemon is not registered".to_string()),
        };
    };
    let client = Box::new(ControlClient::new(addr));
    let note = thegn_svc::control::PushedNote {
        title: "Session migrated".to_string(),
        body: format!("{worktree}: {source_profile} → {target_profile}"),
        urgency: None,
        source: Some("session_migration".to_string()),
    };
    notify_with_control(client.as_ref(), note).await
}

async fn notify_with_control(
    control: &dyn MigrationControl,
    note: thegn_svc::control::PushedNote,
) -> NotificationAudit {
    if let Err(error) = control.health().await {
        return NotificationAudit {
            status: "unavailable".to_string(),
            warning: Some(format!("target daemon is unreachable: {error}")),
        };
    }
    match control.notify_push(&note).await {
        Ok(_) => NotificationAudit {
            status: "sent".to_string(),
            warning: None,
        },
        Err(error) => NotificationAudit {
            status: "failed".to_string(),
            warning: Some(format!("target notification failed: {error}")),
        },
    }
}

fn report(audit: &MigrationAudit, json: bool) -> Result<()> {
    if json {
        return super::emit_json(audit);
    }
    outln!("{}", human_report(audit));
    Ok(())
}

fn human_report(audit: &MigrationAudit) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "session migration{}",
        if audit.dry_run { " (dry-run)" } else { "" }
    ));
    lines.push(format!("  source profile: {}", audit.source_profile));
    lines.push(format!("  target profile: {}", audit.target_profile));
    lines.push(format!("  worktree: {}", audit.worktree));
    lines.push(format!(
        "  groups: {}",
        if audit.groups.is_empty() {
            "(none)".to_string()
        } else {
            audit.groups.join(", ")
        }
    ));
    lines.push(format!(
        "  rows: worktrees={} tab_groups={} group_tabs={} ui_state={} dispatches={} notes={}",
        audit.counts.worktrees,
        audit.counts.tab_groups,
        audit.counts.group_tabs,
        audit.counts.ui_state,
        audit.counts.dispatches,
        audit.counts.dispatch_notes
    ));
    lines.push(format!("  live IDs: {}", display_ids(&audit.live_ids)));
    lines.push(format!("  killed IDs: {}", display_ids(&audit.killed_ids)));
    lines.push(format!("  target committed: {}", audit.target_committed));
    lines.push(format!("  target confirmed: {}", audit.target_confirmed));
    lines.push(format!("  source deleted: {}", audit.source_deleted));
    lines.push(format!("  resumed: {}", audit.resumed));
    lines.push(format!(
        "  opaque payload warning: {}",
        audit.opaque_payload_warning
    ));
    lines.push(format!("  notification: {}", audit.notification.status));
    if let Some(warning) = &audit.notification.warning {
        lines.push(format!("  warning: {warning}"));
    }
    if let Some(error) = &audit.error {
        lines.push(format!("  error: {error}"));
    }
    lines.join("\n")
}

fn display_ids(ids: &[String]) -> String {
    if ids.is_empty() {
        "(none)".to_string()
    } else {
        ids.join(", ")
    }
}

fn fail(audit: &mut MigrationAudit, json: bool, message: &str, retryable: bool) -> Result<()> {
    audit.error = Some(message.to_string());
    report(audit, json)?;
    if retryable {
        Err(anyhow::Error::new(super::Retryable(anyhow!(
            message.to_string()
        ))))
    } else {
        bail!(message.to_string())
    }
}

/// `Db::open_at` takes the concrete SQLite file. Named profiles have a
/// self-contained state root. The default profile retains the legacy XDG path;
/// `default_state_home` must be captured before a named source reroots the
/// process's `XDG_STATE_HOME`.
fn profile_db_path(paths: &ProfilePaths, default_state_home: &Path) -> PathBuf {
    if paths.is_default() {
        default_state_home.join("thegn/thegn.db")
    } else {
        paths.root.join("state/thegn/thegn.db")
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use thegn_core::session_migration::{
        MigrationCleanupResult, MigrationDispatch, MigrationGroup, MigrationImportResult,
        MigrationNote, MigrationPlan, MigrationTab, MigrationTarget, make_bundle,
    };

    #[derive(Clone, Default)]
    struct FakeControl {
        health_error: Option<String>,
        listings: Arc<Mutex<Vec<Vec<SessionInfo>>>>,
        killed: Arc<Mutex<Vec<String>>>,
        notifications: Arc<Mutex<Vec<String>>>,
    }

    impl MigrationControl for FakeControl {
        fn health(&self) -> BoxFuture<'_, Result<()>> {
            let error = self.health_error.clone();
            Box::pin(async move {
                match error {
                    Some(error) => bail!(error),
                    None => Ok(()),
                }
            })
        }

        fn sessions(&self) -> BoxFuture<'_, Result<Vec<SessionInfo>>> {
            let listings = Arc::clone(&self.listings);
            Box::pin(async move { Ok(listings.lock().unwrap().pop().unwrap_or_default()) })
        }

        fn kill(&self, session: &str) -> BoxFuture<'_, Result<()>> {
            let killed = Arc::clone(&self.killed);
            let session = session.to_string();
            Box::pin(async move {
                killed.lock().unwrap().push(session);
                Ok(())
            })
        }

        fn notify_push(&self, note: &thegn_svc::control::PushedNote) -> BoxFuture<'_, Result<i64>> {
            let notifications = Arc::clone(&self.notifications);
            let body = note.body.clone();
            Box::pin(async move {
                notifications.lock().unwrap().push(body);
                Ok(1)
            })
        }
    }

    #[derive(Clone)]
    struct FakeStore {
        events: Arc<Mutex<Vec<&'static str>>>,
        import_error: Option<String>,
        confirm: bool,
        cleanup_error: Option<String>,
    }

    impl FakeStore {
        fn new(events: Arc<Mutex<Vec<&'static str>>>) -> Self {
            Self {
                events,
                import_error: None,
                confirm: true,
                cleanup_error: None,
            }
        }

        fn record(&self, event: &'static str) {
            self.events.lock().unwrap().push(event);
        }
    }

    impl SessionMigrationStore for FakeStore {
        fn migration_snapshot(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<MigrationBundle> {
            bail!("unused fake snapshot")
        }

        fn migration_target_snapshot(&self, _: &str, _: &str) -> Result<MigrationTarget> {
            bail!("unused fake target snapshot")
        }

        fn import_migration(&self, _: &MigrationPlan) -> Result<MigrationImportResult> {
            self.record("target_import");
            if let Some(error) = &self.import_error {
                bail!(error.clone())
            }
            Ok(MigrationImportResult {
                counts: MigrationCounts::default(),
                dispatch_id_map: Default::default(),
                fingerprint: "fingerprint".into(),
            })
        }

        fn confirm_migration(&self, _: &MigrationPlan) -> Result<bool> {
            self.record("target_confirm");
            Ok(self.confirm)
        }

        fn cleanup_migration(&self, _: &MigrationBundle) -> Result<MigrationCleanupResult> {
            self.record("source_cleanup");
            if let Some(error) = &self.cleanup_error {
                bail!(error.clone())
            }
            Ok(MigrationCleanupResult {
                source_deleted: true,
                ..Default::default()
            })
        }
    }

    fn empty_plan() -> MigrationPlan {
        let bundle = make_bundle(
            "source",
            "target",
            "default",
            "/worktree",
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::<MigrationNote>::new(),
            None,
            None,
        );
        MigrationPlan {
            fingerprint: bundle.fingerprint(),
            bundle,
            target: MigrationTarget::default(),
            resumed: false,
        }
    }

    fn live_session(id: &str, worktree: &str) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            worktree: Some(worktree.into()),
            ..Default::default()
        }
    }

    #[test]
    fn live_sessions_refuse_without_kill() {
        let live = BTreeSet::from(["daemon-1".to_string()]);
        assert_eq!(
            live_move_refusal(&live, false),
            Some("live source sessions found; rerun with --kill")
        );
        assert_eq!(live_move_refusal(&live, true), None);
    }

    #[test]
    fn control_seam_kills_and_relists_before_import() {
        let control = FakeControl {
            listings: Arc::new(Mutex::new(vec![
                vec![live_session("daemon-1", "/worktree")],
                Vec::new(),
            ])),
            ..Default::default()
        };
        let refs = BTreeSet::from(["daemon-1".to_string()]);
        let mut audit = MigrationAudit::new("source", "target", "/worktree", false);

        futures::executor::block_on(kill_and_relist(
            &control,
            &BTreeSet::from(["daemon-1".to_string()]),
            &refs,
            "/worktree",
            &mut audit,
        ))
        .unwrap();
        assert_eq!(*control.killed.lock().unwrap(), vec!["daemon-1"]);
        assert_eq!(audit.killed_ids, vec!["daemon-1"]);
    }

    #[test]
    fn unreachable_control_fails_closed() {
        let control = FakeControl {
            health_error: Some("socket unavailable".into()),
            ..Default::default()
        };
        let error = futures::executor::block_on(checked_sessions(&control)).unwrap_err();
        assert!(error.to_string().contains("socket unavailable"));
    }

    #[test]
    fn target_commit_precedes_source_cleanup() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let source = FakeStore::new(Arc::clone(&events));
        let target = FakeStore::new(Arc::clone(&events));
        let mut audit = MigrationAudit::new("source", "target", "/worktree", false);

        commit_target_then_cleanup(&source, &target, &empty_plan(), &mut audit).unwrap();
        assert_eq!(
            *events.lock().unwrap(),
            vec!["target_import", "target_confirm", "source_cleanup"]
        );
        assert!(audit.target_committed);
        assert!(audit.target_confirmed);
        assert!(audit.source_deleted);
    }

    #[test]
    fn readback_failure_is_reported_before_source_cleanup() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let source = FakeStore::new(Arc::clone(&events));
        let target = FakeStore {
            confirm: false,
            ..FakeStore::new(Arc::clone(&events))
        };
        let mut audit = MigrationAudit::new("source", "target", "/worktree", false);

        let error =
            commit_target_then_cleanup(&source, &target, &empty_plan(), &mut audit).unwrap_err();
        assert!(error.to_string().contains("read-back"));
        assert_eq!(
            *events.lock().unwrap(),
            vec!["target_import", "target_confirm"]
        );
        assert!(audit.target_committed);
        assert!(!audit.target_confirmed);
        assert!(!audit.source_deleted);
    }

    #[test]
    fn cleanup_failure_leaves_retryable_target_confirmation() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let source = FakeStore {
            cleanup_error: Some("database busy".into()),
            ..FakeStore::new(Arc::clone(&events))
        };
        let target = FakeStore::new(Arc::clone(&events));
        let mut audit = MigrationAudit::new("source", "target", "/worktree", false);

        let error =
            commit_target_then_cleanup(&source, &target, &empty_plan(), &mut audit).unwrap_err();
        assert!(error.to_string().contains("database busy"));
        assert!(audit.target_confirmed);
        assert!(!audit.source_deleted);
    }

    #[test]
    fn notification_failure_is_a_warning_after_confirmed_move() {
        let control = FakeControl {
            health_error: Some("target daemon unavailable".into()),
            ..Default::default()
        };
        let note = thegn_svc::control::PushedNote {
            title: "Session migrated".into(),
            body: "body".into(),
            urgency: None,
            source: Some("session_migration".into()),
        };

        let notification = futures::executor::block_on(notify_with_control(&control, note));
        assert_eq!(notification.status, "unavailable");
        assert!(notification.warning.unwrap().contains("unavailable"));
    }

    #[test]
    fn default_target_db_uses_the_pre_rerooted_state_home() {
        let custom_state = PathBuf::from("/tmp/thegn-custom-state");
        let target = ProfilePaths {
            name: "default".into(),
            root: PathBuf::from("/tmp/thegn-custom-dir"),
        };

        assert_eq!(
            profile_db_path(&target, &custom_state),
            custom_state.join("thegn/thegn.db")
        );
        assert_ne!(
            profile_db_path(&target, &custom_state),
            thegn_core::util::home().join(".local/state/thegn/thegn.db")
        );
    }

    #[test]
    fn pane_session_ids_only_accept_daemon_sessions() {
        let mut ids = BTreeSet::new();
        pane_session_ids(
            r#"{"0":{"provider":"daemon","session":"daemon-1"},"1":{"provider":"sprites","session":"remote-1"}}"#,
            &mut ids,
        );
        assert_eq!(ids.into_iter().collect::<Vec<_>>(), vec!["daemon-1"]);
    }

    #[test]
    fn audit_does_not_serialize_opaque_payloads() {
        let bundle = MigrationBundle {
            source_profile: "source".into(),
            target_profile: "target".into(),
            session_name: "default".into(),
            worktree_path: "/worktree".into(),
            worktree: None,
            groups: vec![MigrationGroup {
                session_name: "default".into(),
                name: "group".into(),
                kind: "worktree".into(),
                worktree: "/worktree".into(),
                ordinal: 0,
                active_tab: 0,
                tabs: vec![MigrationTab {
                    session_name: "default".into(),
                    group_name: "group".into(),
                    ordinal: 0,
                    title: "1".into(),
                    pane_tree: "SECRET_CMD".into(),
                    focused_pane: 0,
                    pane_cwds: "SECRET_CWD".into(),
                    pane_cmds: "SECRET_COMMAND".into(),
                    pane_sessions: "SECRET_SESSION".into(),
                    scrollback_snapshot: "SECRET_SCROLLBACK".into(),
                }],
            }],
            ui_state: Vec::new(),
            dispatches: vec![MigrationDispatch {
                source_id: 1,
                issue_id: "issue".into(),
                worktree_path: "/worktree".into(),
                agent_name: "agent".into(),
                dispatched_at_ms: 0,
                status: thegn_core::issue::AgentDispatchStatus::Queued,
                stage: None,
                parent_id: None,
                session_id: Some("SECRET_DAEMON".into()),
                artifact_path: Some("SECRET_ARTIFACT".into()),
                note: Some("SECRET_NOTE".into()),
                chunk_path: Some("SECRET_CHUNK".into()),
                report: Some("SECRET_REPORT".into()),
            }],
            notes: Vec::new(),
            pin_state: Some("SECRET_PIN".into()),
            pin_updated_at: None,
        };
        let mut audit = MigrationAudit::new("source", "target", "/worktree", false);
        fill_bundle_audit(&mut audit, &bundle);
        let encoded = serde_json::to_string(&audit).unwrap();
        for secret in [
            "SECRET_CMD",
            "SECRET_CWD",
            "SECRET_COMMAND",
            "SECRET_SESSION",
            "SECRET_SCROLLBACK",
            "SECRET_DAEMON",
            "SECRET_ARTIFACT",
            "SECRET_NOTE",
            "SECRET_CHUNK",
            "SECRET_REPORT",
            "SECRET_PIN",
        ] {
            assert!(!encoded.contains(secret), "audit leaked {secret}");
        }
    }

    #[test]
    fn dry_run_reports_opaque_payload_warning_in_human_and_json_modes() {
        let audit = MigrationAudit::new("source", "target", "/worktree", true);
        let human = human_report(&audit);
        assert!(human.contains(OPAQUE_PAYLOAD_WARNING));

        let json = serde_json::to_value(&audit).unwrap();
        assert_eq!(
            json["opaque_payload_warning"].as_str(),
            Some(OPAQUE_PAYLOAD_WARNING)
        );
        assert!(
            json["opaque_payload_warning"]
                .as_str()
                .unwrap()
                .contains("carried unchanged")
        );
    }

    #[test]
    fn source_owner_blocks_default_and_named_move_before_database_open() {
        for name in ["default", "work"] {
            let root = std::env::temp_dir().join(format!(
                "tg-move-owner-{name}-{}-{}",
                std::process::id(),
                thegn_core::util::now()
            ));
            std::fs::create_dir_all(root.join("run")).unwrap();
            let lock_path = root.join("run/thegn.lock");
            let owner = std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(&lock_path)
                .unwrap();
            owner.try_lock().unwrap();
            let source = ProfilePaths {
                name: name.into(),
                root: root.clone(),
            };
            let mut database_opens = 0;
            let result = with_source_instance_guard(&source, || {
                database_opens += 1;
                Ok::<_, anyhow::Error>(())
            });
            assert!(result.is_err(), "{name} owner must block migration");
            assert_eq!(database_opens, 0, "{name} DBs must not open");
            drop(owner);
            let _ = std::fs::remove_dir_all(&root); // best-effort: test cleanup
        }
    }
}
