//! `thegn kaneo <action>` — Kaneo project-management CLI.
//!
//! `login`/`logout`/`status` manage the device-flow credential stored in the
//! local DB (`kaneo_auth`); the Kaneo issue backend falls back to that token
//! when no `[issues.kaneo].api_key` is configured. The project/board/task verbs
//! (P3) drive the same `KaneoBackend` the panel uses.

use std::time::Duration;

use anyhow::{Result, bail};
use thegn_core::config::Config;
use thegn_core::store::CacheStore;
use thegn_core::{msg, outln};
use thegn_svc::issue::kaneo_auth;

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// Authenticate to a Kaneo instance in the browser (OAuth device flow) and
    /// store the token locally.
    Login {
        /// Instance base URL (defaults to `[issues.kaneo].base_url`).
        #[arg(long)]
        base_url: Option<String>,
        /// OAuth device-flow client id (allowlisted by Kaneo).
        #[arg(long, default_value = kaneo_auth::DEFAULT_CLIENT_ID)]
        client_id: String,
    },
    /// Forget the stored device-flow token for a Kaneo instance.
    Logout {
        #[arg(long)]
        base_url: Option<String>,
    },
    /// Show the resolved Kaneo instance and whether a token is stored.
    Status {
        #[arg(long)]
        base_url: Option<String>,
    },
    /// List projects in the configured Kaneo workspace.
    Projects {
        #[arg(long)]
        json: bool,
    },
    /// Show a project's board (columns and their tasks).
    Board {
        /// Project id (defaults to `[issues.kaneo].project_id`).
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Create a task in a project.
    Create {
        /// Project id (defaults to `[issues.kaneo].project_id`).
        #[arg(long)]
        project: Option<String>,
        title: String,
        #[arg(long)]
        body: Option<String>,
        /// urgent | high | medium | low | none
        #[arg(long, default_value = "none")]
        priority: String,
    },
    /// Add a comment to a task.
    Comment {
        /// Task id (with or without the `kaneo:` prefix).
        task: String,
        body: String,
    },
    /// Attach (or, with --remove, detach) a label on a task.
    Label {
        task: String,
        name: String,
        #[arg(long)]
        remove: bool,
    },
    /// Move a task to another project (and optionally a target column).
    Move {
        task: String,
        /// Destination project id.
        project: String,
        /// Destination column slug.
        #[arg(long)]
        status: Option<String>,
    },
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    match action {
        Action::Login {
            base_url,
            client_id,
        } => login(cfg, base_url, client_id),
        Action::Logout { base_url } => logout(cfg, base_url),
        Action::Status { base_url } => status(cfg, base_url),
        Action::Projects { json } => projects(cfg, json),
        Action::Board { project, json } => board(cfg, project, json),
        Action::Create {
            project,
            title,
            body,
            priority,
        } => create(cfg, project, title, body, priority),
        Action::Comment { task, body } => comment(cfg, task, body),
        Action::Label { task, name, remove } => label(cfg, task, name, remove),
        Action::Move {
            task,
            project,
            status,
        } => move_task(cfg, task, project, status),
    }
}

fn router(cfg: &Config) -> thegn_svc::issue::IssueRouter {
    thegn_svc::issue::IssueRouter::from_config(&cfg.issues)
}

/// Normalize a user-supplied task id to the router's `"kaneo:<id>"` form.
fn kaneo_id(task: &str) -> String {
    if task.starts_with("kaneo:") {
        task.to_string()
    } else {
        format!("kaneo:{task}")
    }
}

fn parse_priority(s: &str) -> thegn_core::issue::IssuePriority {
    use thegn_core::issue::IssuePriority as P;
    match s.trim().to_ascii_lowercase().as_str() {
        "urgent" => P::Urgent,
        "high" => P::High,
        "medium" | "med" => P::Medium,
        "low" => P::Low,
        _ => P::None,
    }
}

fn projects(cfg: &Config, json: bool) -> Result<()> {
    let r = router(cfg);
    let Some(kaneo) = r.kaneo() else {
        bail!("no Kaneo provider configured (set providers = [\"kaneo\", …])");
    };
    match block(kaneo.list_projects()) {
        Ok(projects) => {
            if json {
                let rows: Vec<_> = projects
                    .iter()
                    .map(|p| serde_json::json!({ "id": p.id, "name": p.name, "slug": p.slug }))
                    .collect();
                outln!("{}", serde_json::to_string(&rows)?);
            } else if projects.is_empty() {
                outln!("No projects");
            } else {
                for p in &projects {
                    outln!("{}  {} ({})", p.id, p.name, p.slug);
                }
            }
            Ok(())
        }
        Err(e) => msg::die(&format!("kaneo projects failed: {e}")),
    }
}

fn resolve_project(cfg: &Config, project: Option<String>) -> Result<String> {
    project
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            let p = cfg.issues.kaneo.project_id.clone();
            (!p.is_empty()).then_some(p)
        })
        .ok_or_else(|| {
            anyhow::anyhow!("no project id given and [issues.kaneo].project_id is empty")
        })
}

fn board(cfg: &Config, project: Option<String>, json: bool) -> Result<()> {
    let project = resolve_project(cfg, project)?;
    let r = router(cfg);
    let Some(kaneo) = r.kaneo() else {
        bail!("no Kaneo provider configured");
    };
    match block(kaneo.board(&project)) {
        Ok(cols) => {
            if json {
                outln!("{}", serde_json::to_string(&board_json(&cols))?);
            } else {
                for c in &cols {
                    outln!("\n▌ {} ({} task(s))", c.name, c.issues.len());
                    for i in &c.issues {
                        outln!("  {} #{}  {}", i.status.glyph(), i.number, i.title);
                    }
                }
            }
            Ok(())
        }
        Err(e) => msg::die(&format!("kaneo board failed: {e}")),
    }
}

fn board_json(cols: &[thegn_svc::issue::kaneo::KaneoColumnInfo]) -> serde_json::Value {
    serde_json::json!(
        cols.iter()
            .map(|c| serde_json::json!({
                "name": c.name,
                "slug": c.slug,
                "isFinal": c.is_final,
                "tasks": c.issues,
            }))
            .collect::<Vec<_>>()
    )
}

fn create(
    cfg: &Config,
    project: Option<String>,
    title: String,
    body: Option<String>,
    priority: String,
) -> Result<()> {
    let project = resolve_project(cfg, project)?;
    let draft = thegn_core::issue::IssueDraft {
        title,
        body,
        priority: parse_priority(&priority),
        project_id: Some(project),
    };
    let r = router(cfg);
    match block(r.create_issue(&draft)) {
        Ok(issue) => {
            outln!("✓ Created {} — {}", issue.number, issue.title);
            Ok(())
        }
        Err(e) => msg::die(&format!("kaneo create failed: {e}")),
    }
}

fn comment(cfg: &Config, task: String, body: String) -> Result<()> {
    let r = router(cfg);
    match block(r.add_comment(&kaneo_id(&task), &body)) {
        Ok(()) => {
            outln!("✓ Comment added to {task}");
            Ok(())
        }
        Err(e) => msg::die(&format!("kaneo comment failed: {e}")),
    }
}

fn label(cfg: &Config, task: String, name: String, remove: bool) -> Result<()> {
    let r = router(cfg);
    let id = kaneo_id(&task);
    let res = if remove {
        block(r.detach_label(&id, &name))
    } else {
        block(r.attach_label(&id, &name))
    };
    match res {
        Ok(()) => {
            let verb = if remove { "Removed" } else { "Attached" };
            outln!("✓ {verb} label {name:?} on {task}");
            Ok(())
        }
        Err(e) => msg::die(&format!("kaneo label failed: {e}")),
    }
}

fn move_task(cfg: &Config, task: String, project: String, status: Option<String>) -> Result<()> {
    let r = router(cfg);
    let Some(kaneo) = r.kaneo() else {
        bail!("no Kaneo provider configured");
    };
    match block(kaneo.move_task(&kaneo_id(&task), &project, status.as_deref())) {
        Ok(()) => {
            outln!("✓ Moved {task} → project {project}");
            Ok(())
        }
        Err(e) => msg::die(&format!("kaneo move failed: {e}")),
    }
}

/// Run a single future to completion on a throwaway current-thread runtime.
fn block<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime")
        .block_on(fut)
}

/// Resolve the Kaneo instance URL: an explicit flag wins, then the first
/// configured Kaneo account's `base_url`, then the legacy `[issues.kaneo]`
/// sub-table. Always returned without a trailing slash (the DB key form).
fn resolve_base(cfg: &Config, flag: Option<String>) -> Result<String> {
    let raw = if let Some(b) = flag.filter(|s| !s.trim().is_empty()) {
        b
    } else if let Some(b) = cfg
        .issues
        .active_accounts()
        .into_iter()
        .find(|a| {
            a.provider == thegn_core::config::IssueProviderKind::Kaneo && !a.base_url.is_empty()
        })
        .map(|a| a.base_url)
    {
        b
    } else if !cfg.issues.kaneo.base_url.is_empty() {
        cfg.issues.kaneo.base_url.clone()
    } else {
        bail!("no Kaneo base_url configured; set [issues.kaneo].base_url or pass --base-url");
    };
    Ok(raw.trim_end_matches('/').to_string())
}

fn login(cfg: &Config, base_url: Option<String>, client_id: String) -> Result<()> {
    let base = resolve_base(cfg, base_url)?;
    let token = block(async {
        let code = kaneo_auth::request_device_code(&base, &client_id).await?;
        outln!("Authorize thegn in your browser:\n");
        outln!("  URL:  {}", code.verification_uri);
        outln!("  Code: {}", code.user_code);
        if let Some(complete) = &code.verification_uri_complete {
            outln!("\n  (or open directly: {complete})");
        }
        outln!("\nWaiting for approval…");
        let timeout = if code.expires_in > 0 {
            Duration::from_secs(code.expires_in)
        } else {
            Duration::from_secs(15 * 60)
        };
        kaneo_auth::poll_access_token(
            &base,
            &client_id,
            &code.device_code,
            code.interval,
            timeout,
            || {},
        )
        .await
    });

    match token {
        Ok(token) => match thegn_core::db::Db::open() {
            Ok(db) => {
                db.put_kaneo_token(&base, &token)?;
                outln!("✓ Logged in to {base}");
                Ok(())
            }
            Err(e) => bail!("authenticated, but could not open the DB to store the token: {e}"),
        },
        Err(e) => msg::die(&format!("kaneo login failed: {e}")),
    }
}

fn logout(cfg: &Config, base_url: Option<String>) -> Result<()> {
    let base = resolve_base(cfg, base_url)?;
    let db = thegn_core::db::Db::open()?;
    let had = db.get_kaneo_token(&base)?.is_some();
    db.delete_kaneo_token(&base)?;
    if had {
        outln!("✓ Logged out of {base}");
    } else {
        outln!("No stored token for {base}");
    }
    Ok(())
}

fn status(cfg: &Config, base_url: Option<String>) -> Result<()> {
    let base = resolve_base(cfg, base_url)?;
    outln!("Instance: {base}");
    // Configured either as a literal key, or via the `env:KANEO_API_KEY`
    // placeholder whose variable is actually set.
    let key = &cfg.issues.kaneo.api_key;
    let has_api_key =
        (!key.is_empty() && key != "env:KANEO_API_KEY") || std::env::var("KANEO_API_KEY").is_ok();
    let stored = thegn_core::db::Db::open()
        .ok()
        .and_then(|db| db.get_kaneo_token(&base).ok().flatten());
    outln!(
        "API key:  {}",
        if has_api_key { "configured" } else { "not set" }
    );
    match stored {
        Some((_, at)) => outln!("Device token: stored (updated {}s ago)", age_secs(at)),
        None => outln!("Device token: none (run `thegn kaneo login`)"),
    }
    Ok(())
}

fn age_secs(fetched_at_ms: i64) -> i64 {
    (thegn_core::util::now() - fetched_at_ms).max(0) / 1000
}
