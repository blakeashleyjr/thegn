//! Catalog action adapter for automation plans.

use std::sync::Arc;

use anyhow::{Context, Result};
use thegn_core::automation::{AutomationEvent, AutomationOrigin, PlannedAction};
use thegn_core::config::Config;
use thegn_svc::control::client::{ControlAddr, ControlClient};
use thegn_svc::control::{
    AgentLaunch, AutomationOrigin as WireOrigin, OpenSpec, PushedNote, ToolRunRequest,
};

#[derive(Debug)]
pub struct ActionTimedOut(pub String);

impl std::fmt::Display for ActionTimedOut {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ActionTimedOut {}

pub async fn execute(
    cfg: Arc<Config>,
    event: AutomationEvent,
    action: PlannedAction,
    run_id: i64,
    timeout_secs: u64,
) -> Result<()> {
    anyhow::ensure!(
        thegn_core::capability::lookup(&action.cap).is_some(),
        "automation action {} is absent from the capability catalog",
        action.cap
    );
    let origin = AutomationOrigin {
        root_event_id: event
            .origin
            .as_ref()
            .map_or_else(|| event.id.clone(), |origin| origin.root_event_id.clone()),
        rule_id: action.rule_id.clone(),
        run_id: run_id.to_string(),
    };
    let wire_origin = WireOrigin {
        root_event_id: origin.root_event_id.clone(),
        rule_id: origin.rule_id.clone(),
        run_id: origin.run_id.clone(),
    };
    match action.cap.as_str() {
        "sessions.open" => {
            let agent = required(&action, "agent")?;
            let prompt = action.params.get("prompt").cloned().unwrap_or_default();
            anyhow::ensure!(
                cfg.agent_command(&agent).is_some(),
                "configured agent {agent:?} not found"
            );
            client()
                .await?
                .open(&OpenSpec {
                    rows: 24,
                    cols: 80,
                    worktree: event.worktree.clone(),
                    agent: Some(AgentLaunch {
                        agent,
                        prompt: prompt.clone(),
                        headless: Some(!prompt.is_empty()),
                        bind_worktree: false,
                        resume: None,
                        continue_last: false,
                        stage: None,
                        fork: false,
                        native_session_id: None,
                    }),
                    automation_origin: Some(wire_origin),
                    ..Default::default()
                })
                .await?;
            Ok(())
        }
        "merge.add" => {
            let worktree = event
                .worktree
                .as_deref()
                .context("merge.add requires event.worktree")?;
            let origin_json = serde_json::to_string(&origin)?;
            let worktree_owned = worktree.to_string();
            tokio::task::spawn_blocking(move || -> Result<()> {
                use thegn_core::store::WorkspaceStore;
                let db = thegn_core::db::Db::open()?;
                db.set_ui_state("automation_merge_origin", &worktree_owned, &origin_json)?;
                Ok(())
            })
            .await??;
            client().await?.merge_add(worktree).await?;
            Ok(())
        }
        "notify.push" => {
            let body = required(&action, "body")?;
            let title = action
                .params
                .get("title")
                .cloned()
                .unwrap_or_else(|| "Automation".into());
            let urgency = action.params.get("urgency").cloned();
            client()
                .await?
                .notify_push(&PushedNote {
                    title,
                    body,
                    urgency,
                    source: Some(format!("automation:{}", action.rule_id)),
                    automation_origin: Some(wire_origin),
                })
                .await?;
            Ok(())
        }
        "tools.run" => {
            let name = required(&action, "name")?;
            anyhow::ensure!(
                cfg.tool_command(&name).is_some(),
                "configured tool {name:?} not found"
            );
            let client = client().await?;
            let opened = client
                .tools_run(&ToolRunRequest {
                    name,
                    worktree: event.worktree.clone(),
                    automation_origin: Some(wire_origin),
                })
                .await?;
            let outcome = match tokio::time::timeout(
                std::time::Duration::from_secs(timeout_secs),
                client.wait(&opened.id, serde_json::json!({"kind": "exited"}), None),
            )
            .await
            {
                Ok(outcome) => outcome?,
                Err(_) => {
                    let _ = client.kill(&opened.id).await;
                    return Err(anyhow::Error::new(ActionTimedOut(format!(
                        "tools.run deadline {timeout_secs}s; session {} terminated",
                        opened.id
                    ))));
                }
            };
            anyhow::ensure!(
                outcome
                    .get("exit_code")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0)
                    == 0,
                "configured tool exited unsuccessfully"
            );
            Ok(())
        }
        other => anyhow::bail!("unsupported automation action {other}"),
    }
}

fn required(action: &PlannedAction, name: &str) -> Result<String> {
    action
        .params
        .get(name)
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .with_context(|| format!("{}.{} is required", action.cap, name))
}

async fn client() -> Result<ControlClient> {
    let addr = tokio::task::spawn_blocking(|| -> Result<ControlAddr> {
        let db = thegn_core::db::Db::open()?;
        thegn_svc::control::client::discover(
            &db,
            &crate::daemon::scope_key(),
            thegn_core::util::now().saturating_mul(1_000),
        )
        .context("no live daemon for automation action")
    })
    .await??;
    match addr {
        ControlAddr::Unix(_) | ControlAddr::Tcp { .. } => Ok(ControlClient::new(addr)),
    }
}
