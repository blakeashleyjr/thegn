//! Catalog action adapter for automation plans.

use std::sync::Arc;

use anyhow::{Context, Result};
use thegn_core::automation::{AutomationEvent, AutomationOrigin, PlannedAction};
use thegn_core::config::Config;
use thegn_svc::control::client::{ControlAddr, ControlClient};
use thegn_svc::control::{
    AgentLaunch, AutomationOrigin as WireOrigin, OpenSpec, PushedNote, ToolRunRequest,
};

pub async fn execute(
    cfg: Arc<Config>,
    event: AutomationEvent,
    action: PlannedAction,
    run_id: i64,
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
            client()?
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
            client()?.merge_add(worktree).await?;
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
            client()?
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
            let opened = client()?
                .tools_run(&ToolRunRequest {
                    name,
                    worktree: event.worktree.clone(),
                    automation_origin: Some(wire_origin),
                })
                .await?;
            let outcome = client()?
                .wait(&opened.id, serde_json::json!({"kind": "exited"}), None)
                .await?;
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

fn client() -> Result<ControlClient> {
    let db = thegn_core::db::Db::open()?;
    let addr = thegn_svc::control::client::discover(
        &db,
        &crate::daemon::scope_key(),
        thegn_core::util::now().saturating_mul(1_000),
    )
    .context("no live daemon for automation action")?;
    match addr {
        ControlAddr::Unix(_) | ControlAddr::Tcp { .. } => Ok(ControlClient::new(addr)),
    }
}
