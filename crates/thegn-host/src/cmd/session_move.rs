//! `thegn session move` — migrate one persisted session presentation between
//! profile databases. The operation is intentionally host-local: it never
//! reroots the process, loads target configuration, or becomes a daemon API.

use anyhow::{Context, Result, anyhow, bail};
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

    if profile::instance_running() {
        return fail(
            &mut audit,
            json,
            "the source profile has another interactive instance running",
            false,
        );
    }

    let source_db = if dry_run {
        Db::open_read_only()
            .context("open source profile database read-only")?
            .ok_or_else(|| anyhow!("source profile database does not exist"))?
    } else {
        Db::open().context("open source profile database")?
    };
    let target_db_path = profile_db_path(&target, &profile::default_state_home());
    let target_db = if dry_run {
        Db::open_read_only_at(&target_db_path).with_context(|| {
            format!(
                "open target profile database read-only at {}",
                target_db_path.display()
            )
        })?
    } else {
        Some(Db::open_at(&target_db_path).with_context(|| {
            format!(
                "open target profile database at {}",
                target_db_path.display()
            )
        })?)
    };
    let session = thegn_core::db::session();
    let bundle = source_db.migration_snapshot(&source.name, &target.name, &session, &worktree)?;
    fill_bundle_audit(&mut audit, &bundle);

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

    if !live_ids.is_empty() {
        if !kill {
            return fail(
                &mut audit,
                json,
                "live source sessions found; rerun with --kill",
                false,
            );
        }
        let Some(client) = source_client.as_ref() else {
            return fail(
                &mut audit,
                json,
                "live source sessions found but the source daemon is unavailable",
                false,
            );
        };
        for id in &live_ids {
            client
                .kill(id)
                .await
                .with_context(|| format!("kill source daemon session {id}"))?;
            audit.killed_ids.push(id.clone());
        }
        let after = client
            .sessions()
            .await
            .context("confirm source daemon sessions were killed")?;
        let survivors = live_session_ids(&after, &refs, &worktree);
        if !survivors.is_empty() {
            return fail(
                &mut audit,
                json,
                &format!(
                    "source daemon sessions survived --kill: {}",
                    survivors.into_iter().collect::<Vec<_>>().join(", ")
                ),
                false,
            );
        }
    }

    let target_db = target_db
        .as_ref()
        .expect("real migration always opens a writable target database");
    let imported = target_db
        .import_migration(&plan)
        .map_err(|e| anyhow!("target profile import failed before commit: {e}"))?;
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

    if !target_db.confirm_migration(&plan)? {
        return fail(
            &mut audit,
            json,
            "target import read-back did not match its sanitized fingerprint",
            true,
        );
    }
    audit.target_confirmed = true;

    let cleanup = match source_db.cleanup_migration(&plan.bundle) {
        Ok(cleanup) => cleanup,
        Err(error) => {
            return fail(
                &mut audit,
                json,
                &format!("source cleanup is pending after target confirmation: {error}"),
                true,
            );
        }
    };
    audit.source_deleted = cleanup.source_deleted;

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

/// List a source daemon only when its active DB registry says one is live.
/// A live registry row that cannot be reached is a fail-closed condition: the
/// migration cannot safely prove that a referenced session has stopped.
async fn source_daemon(
    db: &Db,
    referenced: &BTreeSet<String>,
) -> Result<(Option<ControlClient>, Vec<SessionInfo>)> {
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
            let client = ControlClient::new(thegn_svc::control::client::ControlAddr::Unix(
                daemon.endpoint.clone().into(),
            ));
            if client.health().await.is_err() {
                bail!("source daemon is registered but unreachable while referenced sessions exist")
            }
            let sessions = client
                .sessions()
                .await
                .context("list source daemon sessions")?;
            return Ok((Some(client), sessions));
        }
        return Ok((None, Vec::new()));
    }

    let addr = thegn_svc::control::client::discover(db, &scope, now)
        .ok_or_else(|| anyhow!("source daemon is registered but not discoverable"))?;
    let client = ControlClient::new(addr);
    if client.health().await.is_err() {
        bail!("source daemon is registered but unreachable; refusing migration")
    }
    let sessions = client
        .sessions()
        .await
        .context("list source daemon sessions")?;
    // Keep this assertion explicit: if a future registry implementation can
    // return a live row without `discover` seeing it, do not silently proceed.
    if registered.is_empty() {
        bail!("source daemon registry changed during migration preflight")
    }
    Ok((Some(client), sessions))
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
    let client = ControlClient::new(addr);
    if let Err(error) = client.health().await {
        return NotificationAudit {
            status: "unavailable".to_string(),
            warning: Some(format!("target daemon is unreachable: {error}")),
        };
    }
    let note = thegn_svc::control::PushedNote {
        title: "Session migrated".to_string(),
        body: format!("{worktree}: {source_profile} → {target_profile}"),
        urgency: None,
        source: Some("session_migration".to_string()),
    };
    match client.notify_push(&note).await {
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
    use thegn_core::session_migration::{MigrationDispatch, MigrationGroup, MigrationTab};

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
}
