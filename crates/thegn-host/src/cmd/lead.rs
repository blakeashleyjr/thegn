//! `thegn lead pause|resume|status` (b155 6f): the Lead kill switch.
//!
//! `pause` posts `automations.halt` (Admin), which sets the pause flag and,
//! unless `--keep-sessions`, kills every automation-started and Lead-opened
//! session, cancels executor validations, and reports what stopped. `resume`
//! posts `automations.resume` (Admin; the Lead cannot); killed sessions are
//! not restarted. `status` prints the flag, budget usage vs limits, pending
//! approvals and live automation lanes.
//!
//! Both mutating verbs talk to the daemon the way `thegn automations` does
//! (`automation_executor::client`, the CLI's implicit-admin connection).
//! `status` reads the flag, budgets and lanes from the local DB and asks the
//! daemon only for pending approvals, so it still answers with the daemon
//! down.

use anyhow::{Context, Result};
use serde::Serialize;
use thegn_core::automation_budget::{BudgetUsage, Budgets};
use thegn_core::config::Config;

#[derive(Debug, Clone, clap::Subcommand)]
pub enum Action {
    /// Pause every automation rule and kill automation-started sessions.
    Pause {
        /// Pause only; leave running automation and Lead-opened sessions alone.
        #[arg(long)]
        keep_sessions: bool,
    },
    /// Resume paused automations (operator only).
    Resume,
    /// Show the pause flag, budget usage, pending approvals and live lanes.
    Status {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
}

pub fn run(cfg: &Config, a: Action) -> Result<()> {
    match a {
        Action::Pause { keep_sessions } => pause(keep_sessions),
        Action::Resume => resume(),
        Action::Status { json } => status(cfg, json),
    }
}

fn call_daemon(path: &'static str, body: serde_json::Value) -> Result<serde_json::Value> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        crate::automation_executor::client()
            .await?
            .call_raw("POST", path, Some(body))
            .await
    })
}

fn pause(keep_sessions: bool) -> Result<()> {
    let reply = call_daemon(
        "/v1/automations/halt",
        serde_json::json!({ "keep_sessions": keep_sessions }),
    )
    .context("automations.halt")?;
    for line in format_halt(&reply) {
        thegn_core::outln!("{line}");
    }
    Ok(())
}

/// What `pause` prints from the `automations.halt` reply.
fn format_halt(reply: &serde_json::Value) -> Vec<String> {
    let mut out = vec!["automations paused".to_string()];
    let kept = reply
        .get("kept_sessions")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let killed: Vec<&str> = reply
        .get("killed")
        .and_then(serde_json::Value::as_array)
        .map(|ids| ids.iter().filter_map(serde_json::Value::as_str).collect())
        .unwrap_or_default();
    if kept {
        out.push("sessions left running (--keep-sessions)".into());
    } else if killed.is_empty() {
        out.push("no automation or Lead-opened sessions were running".into());
    } else {
        out.push(format!(
            "killed {} session{}: {}",
            killed.len(),
            if killed.len() == 1 { "" } else { "s" },
            killed.join(", ")
        ));
    }
    let cancelled = reply
        .get("validations_cancelled")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    out.push(format!("cancelled {cancelled} executor validation(s)"));
    if let Some(note) = reply
        .get("headless_lead_turn")
        .and_then(serde_json::Value::as_str)
    {
        out.push(format!("note: {note}"));
    }
    out.push("resume with `thegn lead resume` (the Lead cannot resume)".into());
    out
}

fn resume() -> Result<()> {
    call_daemon("/v1/automations/resume", serde_json::json!({})).context("automations.resume")?;
    thegn_core::outln!("automations resumed");
    thegn_core::outln!("sessions killed by `thegn lead pause` are not restarted");
    Ok(())
}

/// One live automation lane row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct LaneView {
    id: i64,
    issue_id: String,
    stage: Option<String>,
    status: String,
    rule: String,
    autonomy: Option<String>,
}

/// One pending approval request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
struct PendingView {
    issue_id: String,
    stage: String,
    commit_sha: String,
    #[serde(default)]
    message: String,
}

/// Everything `status` prints. A section that could not be read carries its
/// error instead of a value, so a partial answer never looks like "none".
#[derive(Debug, Clone, PartialEq, Eq)]
struct StatusView {
    paused: Result<bool, String>,
    limits: Budgets,
    usage: Result<BudgetUsage, String>,
    pending_approvals: Result<Vec<PendingView>, String>,
    lanes: Result<Vec<LaneView>, String>,
}

/// The JSON shape of `status --json`: each section is either its value or
/// `{"error": ...}`.
fn status_json(v: &StatusView) -> serde_json::Value {
    fn part<T: Serialize>(r: &Result<T, String>) -> serde_json::Value {
        match r {
            Ok(v) => serde_json::to_value(v).unwrap_or(serde_json::Value::Null),
            Err(e) => serde_json::json!({ "error": e }),
        }
    }
    serde_json::json!({
        "paused": part(&v.paused),
        "budget": {
            "max_concurrent_lanes": v.limits.max_concurrent_lanes,
            "max_spawns_per_day": v.limits.max_spawns_per_day,
            "usage": match &v.usage {
                Ok(u) => serde_json::json!({
                    "active_lanes": u.active_lanes,
                    "spawns_24h": u.spawns_24h,
                }),
                Err(e) => serde_json::json!({ "error": e }),
            },
        },
        "pending_approvals": part(&v.pending_approvals),
        "lanes": part(&v.lanes),
    })
}

/// The human `status` text.
fn format_status(v: &StatusView) -> Vec<String> {
    let mut out = Vec::new();
    out.push(match &v.paused {
        Ok(true) => "automations: PAUSED (resume with `thegn lead resume`)".into(),
        Ok(false) => "automations: running".into(),
        Err(e) => format!("automations: unknown ({e})"),
    });
    match &v.usage {
        Ok(u) => {
            out.push(format!(
                "lanes:  {} of {} active ([automations] max_concurrent_lanes)",
                u.active_lanes, v.limits.max_concurrent_lanes
            ));
            out.push(format!(
                "spawns: {} of {} in the last 24 h ([automations] max_spawns_per_day)",
                u.spawns_24h, v.limits.max_spawns_per_day
            ));
        }
        Err(e) => out.push(format!(
            "budget: unreadable ({e}); automation spawns are refused"
        )),
    }
    match &v.pending_approvals {
        Ok(p) if p.is_empty() => out.push("pending approvals: none".into()),
        Ok(p) => {
            out.push(format!("pending approvals: {}", p.len()));
            for a in p {
                let sha: String = a.commit_sha.chars().take(7).collect();
                out.push(format!(
                    "  {} {} {}  {}",
                    a.issue_id,
                    a.stage,
                    sha,
                    a.message.replace(['\n', '\r'], " ")
                ));
            }
        }
        Err(e) => out.push(format!("pending approvals: unavailable ({e})")),
    }
    match &v.lanes {
        Ok(l) if l.is_empty() => out.push("live automation lanes: none".into()),
        Ok(l) => {
            out.push(format!("live automation lanes: {}", l.len()));
            for lane in l {
                out.push(format!(
                    "  #{} {} {} {} rule={} autonomy={}",
                    lane.id,
                    lane.issue_id,
                    lane.stage.as_deref().unwrap_or("-"),
                    lane.status,
                    lane.rule,
                    lane.autonomy.as_deref().unwrap_or("-")
                ));
            }
        }
        Err(e) => out.push(format!("live automation lanes: unavailable ({e})")),
    }
    out
}

/// Active dispatch rows that carry an automation rule.
fn live_lanes(db: &thegn_core::db::Db) -> Result<Vec<LaneView>> {
    use thegn_core::store::NotificationStore;
    let mut out = Vec::new();
    for row in db.list_dispatches()? {
        if !row.status.is_active() {
            continue;
        }
        let (autonomy, rule) = db.dispatch_automation(row.id)?;
        if let Some(rule) = rule {
            out.push(LaneView {
                id: row.id,
                issue_id: row.issue_id,
                stage: row.stage,
                status: row.status.as_str().to_string(),
                rule,
                autonomy,
            });
        }
    }
    Ok(out)
}

fn pending_from_reply(reply: &serde_json::Value) -> Result<Vec<PendingView>> {
    serde_json::from_value(
        reply
            .get("pending")
            .cloned()
            .unwrap_or_else(|| serde_json::json!([])),
    )
    .context("decode pending approvals")
}

fn status(cfg: &Config, json: bool) -> Result<()> {
    use thegn_core::store::{AutomationStore, WorkspaceStore};
    let limits = Budgets::from_config(&cfg.effective_automations());
    let err = |e: anyhow::Error| format!("{e:#}");
    let db = thegn_core::db::Db::open().map_err(err);
    let (paused, usage, lanes) = match &db {
        Ok(db) => (
            db.get_ui_state(
                thegn_core::automation_control::SCOPE,
                thegn_core::automation_control::PAUSED_KEY,
            )
            .map(|v| v.is_some_and(|v| v == "1"))
            .map_err(err),
            db.automation_budget_usage(thegn_core::util::now_ms())
                .map_err(err),
            live_lanes(db).map_err(err),
        ),
        Err(e) => (Err(e.clone()), Err(e.clone()), Err(e.clone())),
    };
    let pending_approvals = call_daemon("/v1/approvals/list", serde_json::json!({}))
        .and_then(|reply| pending_from_reply(&reply))
        .map_err(err);
    let view = StatusView {
        paused,
        limits,
        usage,
        pending_approvals,
        lanes,
    };
    if json {
        return super::emit_json(&status_json(&view));
    }
    for line in format_status(&view) {
        thegn_core::outln!("{line}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(clap::Parser)]
    struct Cli {
        #[command(subcommand)]
        action: Action,
    }

    fn parse(args: &[&str]) -> Result<Action, clap::Error> {
        use clap::Parser;
        Cli::try_parse_from(std::iter::once("x").chain(args.iter().copied())).map(|c| c.action)
    }

    #[test]
    fn lead_arg_parsing() {
        assert!(matches!(
            parse(&["pause"]).unwrap(),
            Action::Pause {
                keep_sessions: false
            }
        ));
        assert!(matches!(
            parse(&["pause", "--keep-sessions"]).unwrap(),
            Action::Pause {
                keep_sessions: true
            }
        ));
        assert!(matches!(parse(&["resume"]).unwrap(), Action::Resume));
        assert!(matches!(
            parse(&["status"]).unwrap(),
            Action::Status { json: false }
        ));
        assert!(matches!(
            parse(&["status", "--json"]).unwrap(),
            Action::Status { json: true }
        ));
        assert!(parse(&["resume", "--keep-sessions"]).is_err());
        assert!(parse(&["pause", "--json"]).is_err());
        assert!(parse(&[]).is_err());
    }

    #[test]
    fn halt_reply_formatting() {
        let lines = format_halt(&serde_json::json!({
            "paused": true,
            "killed": ["s1", "s2"],
            "validations_cancelled": 1,
            "kept_sessions": false,
            "headless_lead_turn": "may still finish",
        }));
        assert_eq!(lines[0], "automations paused");
        assert!(lines.iter().any(|l| l == "killed 2 sessions: s1, s2"));
        assert!(lines.iter().any(|l| l.contains("cancelled 1 executor")));
        assert!(lines.iter().any(|l| l.contains("may still finish")));

        let kept = format_halt(&serde_json::json!({
            "paused": true, "killed": [], "validations_cancelled": 0, "kept_sessions": true,
        }));
        assert!(kept.iter().any(|l| l.contains("--keep-sessions")));
        let none = format_halt(&serde_json::json!({
            "paused": true, "killed": [], "validations_cancelled": 0, "kept_sessions": false,
        }));
        assert!(none.iter().any(|l| l.starts_with("no automation")));
    }

    fn view() -> StatusView {
        StatusView {
            paused: Ok(true),
            limits: Budgets {
                max_concurrent_lanes: 2,
                max_spawns_per_day: 12,
            },
            usage: Ok(BudgetUsage {
                active_lanes: 1,
                spawns_24h: 5,
            }),
            pending_approvals: Ok(vec![PendingView {
                issue_id: "linear:THE-1".into(),
                stage: "review".into(),
                commit_sha: "0123456789abcdef".into(),
                message: "land\nit".into(),
            }]),
            lanes: Ok(vec![LaneView {
                id: 7,
                issue_id: "linear:THE-1".into(),
                stage: Some("code".into()),
                status: "running".into(),
                rule: "nightly".into(),
                autonomy: Some("review".into()),
            }]),
        }
    }

    #[test]
    fn status_formatting_shows_flag_budgets_approvals_and_lanes() {
        let lines = format_status(&view());
        assert!(lines[0].contains("PAUSED"), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("1 of 2 active")));
        assert!(lines.iter().any(|l| l.contains("5 of 12 in the last 24 h")));
        assert!(lines.iter().any(|l| l == "pending approvals: 1"));
        assert!(
            lines
                .iter()
                .any(|l| l.contains("linear:THE-1 review 0123456  land it"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("#7 linear:THE-1 code running rule=nightly"))
        );
    }

    #[test]
    fn status_formatting_names_unreadable_sections() {
        let mut v = view();
        v.paused = Ok(false);
        v.usage = Err("db locked".into());
        v.pending_approvals = Err("daemon not running".into());
        v.lanes = Ok(vec![]);
        let lines = format_status(&v);
        assert_eq!(lines[0], "automations: running");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("budget: unreadable (db locked)"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("pending approvals: unavailable (daemon not running)"))
        );
        assert!(lines.iter().any(|l| l == "live automation lanes: none"));
    }

    #[test]
    fn status_json_shape() {
        let mut v = view();
        v.pending_approvals = Err("down".into());
        let j = status_json(&v);
        assert_eq!(j["paused"], true);
        assert_eq!(j["budget"]["max_concurrent_lanes"], 2);
        assert_eq!(j["budget"]["usage"]["spawns_24h"], 5);
        assert_eq!(j["pending_approvals"]["error"], "down");
        assert_eq!(j["lanes"][0]["rule"], "nightly");
    }

    #[test]
    fn pending_decodes_from_the_approvals_list_reply() {
        let reply = serde_json::json!({
            "approvals": [],
            "pending": [{
                "issue_id": "linear:THE-2", "stage": "s", "commit_sha": "abc",
                "worktree": "/wt", "requested_at": 1, "notification_id": 3, "message": "m",
            }],
        });
        let p = pending_from_reply(&reply).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].issue_id, "linear:THE-2");
        assert!(
            pending_from_reply(&serde_json::json!({}))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn lanes_list_only_active_rows_with_a_rule() {
        use thegn_core::issue::{AgentDispatchStatus, NewDispatch};
        use thegn_core::store::NotificationStore;
        let db = thegn_core::db::Db::open_memory().unwrap();
        assert!(live_lanes(&db).unwrap().is_empty());
        let put = |issue: &str| {
            db.put_agent_dispatch(NewDispatch {
                issue_id: issue,
                worktree_path: "/wt",
                agent_name: "codex",
                stage: None,
                parent_id: None,
                session_id: None,
                artifact_path: None,
                chunk_path: None,
            })
            .unwrap()
        };
        let auto = put("linear:THE-1");
        db.set_dispatch_automation(auto, "review", "nightly")
            .unwrap();
        let _manual = put("linear:THE-2");
        let finished = put("linear:THE-3");
        db.set_dispatch_automation(finished, "merge", "nightly")
            .unwrap();
        db.update_dispatch_status(finished, AgentDispatchStatus::Done)
            .unwrap();
        let lanes = live_lanes(&db).unwrap();
        assert_eq!(lanes.len(), 1, "{lanes:?}");
        assert_eq!(lanes[0].id, auto);
        assert_eq!(lanes[0].rule, "nightly");
        assert_eq!(lanes[0].autonomy.as_deref(), Some("review"));
    }
}
