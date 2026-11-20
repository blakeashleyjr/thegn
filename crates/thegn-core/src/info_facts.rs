//! Database, cache, and bounded git facts for the Info screen.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Result, bail};

use crate::config::Config;
use crate::forge::model::{PanelState, PrPanel};
use crate::info_model::{
    Field, GitFacts, InfoTarget, LiveFacts, SessionRow, Source, age, clean, keys, scrub_url,
};
use crate::models::{WorkspaceRow, WorktreeRow};
use crate::remote::GitLoc;
use crate::store::{CacheStore, NotificationStore, WorkspaceStore, WorktreeAuxStore, ZoneStore};

fn same_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn redacted_text(text: &str, max: usize) -> String {
    let line = crate::log_redact::redact_text_line(text);
    let scrubbed = line.split(' ').map(scrub_url).collect::<Vec<_>>().join(" ");
    clean(&scrubbed, max)
}

fn contains_path(root: &Path, path: &Path) -> bool {
    same_path(root, path) || path.starts_with(root)
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn workspace_for<'a>(
    workspaces: &'a [WorkspaceRow],
    slugs: &[(String, String)],
    selector: &str,
) -> Option<&'a WorkspaceRow> {
    workspaces
        .iter()
        .find(|row| same_path(Path::new(&row.repo_path), Path::new(selector)))
        .or_else(|| {
            slugs.iter().find_map(|(repo, slug)| {
                (slug == selector)
                    .then(|| {
                        workspaces
                            .iter()
                            .find(|row| same_path(Path::new(&row.repo_path), Path::new(repo)))
                    })
                    .flatten()
            })
        })
        .or_else(|| workspaces.iter().find(|row| row.name == selector))
}

fn target_for_workspace(row: &WorkspaceRow) -> InfoTarget {
    InfoTarget {
        repo_root: row.repo_path.clone(),
        worktree: None,
    }
}

fn target_for_worktree(row: &WorktreeRow) -> InfoTarget {
    InfoTarget {
        repo_root: row.repo_root.clone(),
        worktree: Some(row.worktree.clone()),
    }
}

/// Resolve the explicit or implicit Info target without mutating the database.
pub fn resolve_target(
    db: &crate::db::Db,
    workspace: Option<&str>,
    worktree: Option<&str>,
    here: &Path,
) -> Result<InfoTarget> {
    let worktrees = db.worktrees()?;
    let workspaces = db.workspaces()?;
    let slugs = db.repo_slug_rows()?;
    let selected_workspace = workspace
        .map(|selector| {
            workspace_for(&workspaces, &slugs, selector)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no workspace with path, slug, or name {selector}"))
        })
        .transpose()?;
    let selected_worktree = if let Some(selector) = worktree {
        if let Some(row) = worktrees.iter().find(|row| {
            row.worktree == selector || same_path(Path::new(&row.worktree), Path::new(selector))
        }) {
            Some(row)
        } else {
            let matches: Vec<_> = worktrees
                .iter()
                .filter(|row| row.branch == selector)
                .collect();
            match matches.as_slice() {
                [row] => Some(*row),
                [] => bail!("no worktree with path or branch {selector}"),
                rows => {
                    let repos = rows
                        .iter()
                        .map(|row| row.repo_root.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    bail!("branch {selector} matches several worktrees: {repos}");
                }
            }
        }
    } else {
        None
    };
    if let (Some(workspace), Some(worktree)) = (&selected_workspace, selected_worktree) {
        if !same_path(
            Path::new(&workspace.repo_path),
            Path::new(&worktree.repo_root),
        ) {
            bail!(
                "worktree {} does not belong to workspace {}",
                worktree.worktree,
                workspace.repo_path
            );
        }
        return Ok(target_for_worktree(worktree));
    }
    if let Some(worktree) = selected_worktree {
        return Ok(target_for_worktree(worktree));
    }
    if let Some(workspace) = selected_workspace {
        return Ok(target_for_workspace(&workspace));
    }
    if let Some(row) = worktrees
        .iter()
        .find(|row| contains_path(Path::new(&row.worktree), here))
    {
        return Ok(target_for_worktree(row));
    }
    if let Some(row) = workspaces
        .iter()
        .find(|row| contains_path(Path::new(&row.repo_path), here))
    {
        return Ok(target_for_workspace(row));
    }
    if let Some(root) = crate::repo::main_worktree(here)
        && let Some(row) = workspaces
            .iter()
            .find(|row| same_path(Path::new(&row.repo_path), &root))
    {
        return Ok(target_for_workspace(row));
    }
    bail!("not inside a thegn workspace; pass --workspace <slug|path>")
}

/// Identity facts for the workspace root.
pub fn workspace_identity(cfg: &Config, db: &crate::db::Db, repo_root: &Path) -> Vec<Field> {
    let root = path_string(repo_root);
    let row = match db.workspaces() {
        Ok(rows) => rows
            .into_iter()
            .find(|row| same_path(Path::new(&row.repo_path), repo_root)),
        Err(error) => {
            return vec![Field::unknown(
                keys::WORKSPACE_NAME,
                "Name",
                error.to_string(),
            )];
        }
    };
    let Some(row) = row else {
        return vec![Field::unknown(
            keys::WORKSPACE_NAME,
            "Name",
            "workspace is not registered",
        )];
    };
    let mut fields = vec![
        Field::value(keys::WORKSPACE_NAME, "Name", row.name, Source::Database),
        match db.repo_slug_lookup(&row.repo_path).ok().flatten() {
            Some(slug) => Field::value(keys::WORKSPACE_SLUG, "Slug", slug, Source::Database),
            None => Field::unset(keys::WORKSPACE_SLUG, "Slug", Source::Database),
        },
        Field::value(
            keys::WORKSPACE_REPO,
            "Repo",
            &row.repo_path,
            Source::Database,
        ),
        Field::value(keys::WORKSPACE_KIND, "Kind", row.kind, Source::Database),
    ];
    match crate::worktree::default_branch_checked(repo_root) {
        Ok(branch) => fields.push(Field::value(
            keys::WORKSPACE_DEFAULT_BRANCH,
            "Default branch",
            branch,
            Source::Git,
        )),
        Err(error) => fields.push(Field::unknown(
            keys::WORKSPACE_DEFAULT_BRANCH,
            "Default branch",
            error,
        )),
    }
    match GitLoc::Local(repo_root.to_path_buf()).origin_url() {
        Some(origin) => fields.push(Field::value(
            keys::WORKSPACE_ORIGIN,
            "Origin",
            scrub_url(&origin),
            Source::Git,
        )),
        None => fields.push(Field::unset(keys::WORKSPACE_ORIGIN, "Origin", Source::Git)),
    }
    match cfg.workspace_overlay(repo_root) {
        crate::workspace_overlay::WorkspaceOverlay::Selected { key, .. } => {
            fields.push(Field::value(
                keys::WORKSPACE_BLOCK,
                "Block",
                format!("[workspace.{key}]"),
                Source::Workspace(key.to_string()),
            ))
        }
        crate::workspace_overlay::WorkspaceOverlay::Refused(reason) => {
            fields.push(Field::unknown(keys::WORKSPACE_BLOCK, "Block", reason.to_string()).warned())
        }
        crate::workspace_overlay::WorkspaceOverlay::Unconfigured => {
            fields.push(Field::unset(keys::WORKSPACE_BLOCK, "Block", Source::Global))
        }
    }
    let discovery = crate::config_repo::discover_repo_overlay(repo_root);
    match discovery.selected() {
        Some(candidate) => {
            let mut field = Field::value(
                keys::WORKSPACE_REPO_OVERLAY,
                "Repo overlay",
                candidate.path.display().to_string(),
                Source::Repo(candidate.path.display().to_string()),
            );
            if discovery.shadow_warning().is_some() {
                field = field.warned();
            }
            fields.push(field);
        }
        None => fields.push(Field::unset(
            keys::WORKSPACE_REPO_OVERLAY,
            "Repo overlay",
            Source::Repo(root),
        )),
    }
    fields
}

fn session_summary(sessions: &[SessionRow]) -> String {
    let mut counts = BTreeMap::<String, usize>::new();
    for session in sessions {
        *counts.entry(session.program.clone()).or_default() += 1;
    }
    let detail = counts
        .into_iter()
        .map(|(program, count)| format!("{program} {count}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{} open ({detail})", sessions.len())
}

/// Aggregate workspace-level database, cache, and sidebar facts.
pub fn workspace_stats(
    db: &crate::db::Db,
    repo_root: &Path,
    live: &LiveFacts,
    now: i64,
) -> Vec<Field> {
    let root = path_string(repo_root);
    let (workspace_rows, mut fields) = match db.worktrees() {
        Ok(rows) => {
            let workspace_rows: Vec<_> = rows
                .into_iter()
                .filter(|row| same_path(Path::new(&row.repo_root), repo_root))
                .collect();
            let count = workspace_rows.len();
            (
                workspace_rows,
                vec![Field::value(
                    keys::STATS_WORKTREES,
                    "Worktrees",
                    count.to_string(),
                    Source::Database,
                )],
            )
        }
        Err(error) => (
            Vec::new(),
            vec![Field::unknown(
                keys::STATS_WORKTREES,
                "Worktrees",
                error.to_string(),
            )],
        ),
    };
    match &live.sessions {
        Some(all) => {
            let paths = workspace_rows
                .iter()
                .map(|row| row.worktree.clone())
                .collect::<Vec<_>>();
            let sessions = sessions_for(all, repo_root, &paths, None);
            fields.push(Field::value(
                keys::STATS_SESSIONS,
                "Sessions",
                session_summary(&sessions),
                live.sessions_source.clone().unwrap_or(Source::Sidebar),
            ));
        }
        None => fields.push(Field::not_measured(keys::STATS_SESSIONS, "Sessions")),
    }
    match (db.all_worktree_disk(), db.all_worktree_disk_stamps()) {
        (Err(error), _) | (_, Err(error)) => {
            fields.push(Field::unknown(keys::STATS_DISK, "Disk", error.to_string()))
        }
        (Ok(disk), Ok(stamps)) => {
            let measured = workspace_rows
                .iter()
                .filter_map(|row| disk.get(&row.worktree).map(|(size, _)| (row, *size)))
                .collect::<Vec<_>>();
            if measured.is_empty() {
                fields.push(Field::not_measured(keys::STATS_DISK, "Disk"));
            } else {
                let total: i64 = measured.iter().map(|(_, size)| *size).sum();
                let oldest = workspace_rows
                    .iter()
                    .filter_map(|row| stamps.get(&row.worktree).copied())
                    .min();
                fields.push(Field::value(
                    keys::STATS_DISK,
                    "Disk",
                    format!(
                        "{} ({} of {} measured)",
                        crate::sandbox_manage::human_bytes(total.max(0) as u64),
                        measured.len(),
                        workspace_rows.len()
                    ),
                    Source::Cache {
                        name: "worktree_disk".into(),
                        at: oldest,
                    },
                ));
            }
        }
    }
    match live.workspace_ahead {
        Some((ahead, total)) => fields.push(Field::value(
            keys::STATS_AHEAD,
            "Ahead",
            format!("{ahead} of {total} worktrees ahead of base"),
            Source::Sidebar,
        )),
        None => fields.push(Field::not_measured(keys::STATS_AHEAD, "Ahead")),
    }
    match db.workspaces() {
        Ok(rows) => {
            if let Some(row) = rows
                .into_iter()
                .find(|row| same_path(Path::new(&row.repo_path), repo_root))
            {
                fields.push(Field::value(
                    keys::STATS_LAST_ACTIVE,
                    "Last active",
                    age(now, row.last_active),
                    Source::Database,
                ));
            } else {
                fields.push(Field::unknown(
                    keys::STATS_LAST_ACTIVE,
                    "Last active",
                    format!("workspace {root} is not registered"),
                ));
            }
        }
        Err(error) => fields.push(Field::unknown(
            keys::STATS_LAST_ACTIVE,
            "Last active",
            error.to_string(),
        )),
    }
    fields
}

fn parse_count(text: &str) -> std::result::Result<(u32, u32), String> {
    let mut parts = text.split_whitespace();
    let left = parts
        .next()
        .ok_or_else(|| "missing left count".to_string())?
        .parse::<u32>()
        .map_err(|e| e.to_string())?;
    let right = parts
        .next()
        .ok_or_else(|| "missing right count".to_string())?
        .parse::<u32>()
        .map_err(|e| e.to_string())?;
    Ok((left, right))
}

fn git_facts(base: &str, wt: &Path) -> std::result::Result<GitFacts, String> {
    let loc = GitLoc::Local(wt.to_path_buf());
    let dirty = loc
        .git_text(&["status", "--porcelain"])
        .map(|text| text.lines().filter(|line| !line.is_empty()).count() as u32)
        .map_err(|e| e.to_string());
    let base_counts = loc
        .git_text(&[
            "rev-list",
            "--left-right",
            "--count",
            &format!("{base}...HEAD"),
        ])
        .map_err(|e| e.to_string())
        .and_then(|text| parse_count(&text));
    let upstream = loc.git_text(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]);
    let upstream_counts = if upstream.is_ok() {
        Some(
            loc.git_text(&["rev-list", "--left-right", "--count", "@{u}...HEAD"])
                .map_err(|e| e.to_string())
                .and_then(|text| parse_count(&text)),
        )
    } else {
        None
    };
    let dirty_files = dirty?;
    let (behind_base, ahead_base) = base_counts?;
    let (behind_upstream, ahead_upstream) = match upstream_counts {
        Some(counts) => {
            let (behind, ahead) = counts?;
            (Some(behind), Some(ahead))
        }
        None => (None, None),
    };
    Ok(GitFacts {
        dirty_files: Some(dirty_files),
        ahead_base: Some(ahead_base),
        behind_base: Some(behind_base),
        ahead_upstream,
        behind_upstream,
        upstream: upstream.ok(),
    })
}

fn git_field(key: &str, label: &str, value: Option<u32>, source: Source) -> Field {
    match value {
        Some(value) => Field::value(key, label, value.to_string(), source),
        None => Field::not_measured(key, label),
    }
}

fn add_git_fields(fields: &mut Vec<Field>, facts: &GitFacts, source: Source) {
    fields.push(git_field(
        keys::WORKTREE_DIRTY,
        "Dirty",
        facts.dirty_files,
        source.clone(),
    ));
    fields.push(git_field(
        keys::WORKTREE_AHEAD_BASE,
        "Ahead base",
        facts.ahead_base,
        source.clone(),
    ));
    fields.push(match &facts.upstream {
        Some(upstream) => Field::value(
            keys::WORKTREE_UPSTREAM,
            "Upstream",
            upstream,
            source.clone(),
        ),
        None => Field::unset(keys::WORKTREE_UPSTREAM, "Upstream", source.clone()),
    });
    fields.push(git_field(
        keys::WORKTREE_AHEAD_UPSTREAM,
        "Ahead upstream",
        facts.ahead_upstream,
        source,
    ));
}

/// Per-worktree facts. Cache reads are always allowed; local git reads require `live.read_git`.
pub fn worktree_fields(
    db: &crate::db::Db,
    repo_root: &Path,
    worktree: &Path,
    live: &LiveFacts,
    now: i64,
) -> Vec<Field> {
    let wt = path_string(worktree);
    let row = match db.worktree_record(&wt) {
        Ok(Some(row)) => row,
        Ok(None) => {
            return vec![Field::unknown(
                keys::WORKTREE_PATH,
                "Path",
                "worktree is not registered",
            )];
        }
        Err(error) => {
            return vec![Field::unknown(
                keys::WORKTREE_PATH,
                "Path",
                error.to_string(),
            )];
        }
    };
    let mut fields = vec![
        Field::value(keys::WORKTREE_PATH, "Path", &row.worktree, Source::Database),
        Field::value(
            keys::WORKTREE_BRANCH,
            "Branch",
            &row.branch,
            Source::Database,
        ),
        Field::value(
            keys::WORKTREE_CREATED,
            "Created",
            age(now, row.created_at),
            Source::Database,
        ),
        if row.agent.trim().is_empty() {
            Field::unset(keys::WORKTREE_AGENT, "Agent", Source::Database)
        } else {
            Field::value(
                keys::WORKTREE_AGENT,
                "Last launched",
                row.agent,
                Source::Database,
            )
        },
        match row.observed_backend.as_deref() {
            Some(backend) if !backend.trim().is_empty() => Field::value(
                keys::WORKTREE_OBSERVED_BACKEND,
                "Observed backend",
                backend,
                Source::Recorded,
            ),
            _ => Field::unset(
                keys::WORKTREE_OBSERVED_BACKEND,
                "Observed backend",
                Source::Recorded,
            ),
        },
    ];
    fields.push(match GitLoc::from_db(&wt, Some(&row.location)) {
        GitLoc::Local(_) => Field::value(
            keys::WORKTREE_LOCATION,
            "Location",
            "local",
            Source::Database,
        ),
        GitLoc::Remote { ssh, path } => Field::value(
            keys::WORKTREE_LOCATION,
            "Location",
            clean(&format!("ssh {}:{path}", ssh.host), 120),
            Source::Database,
        ),
        GitLoc::Provider {
            control_prefix,
            path,
        } => Field::value(
            keys::WORKTREE_LOCATION,
            "Location",
            clean(
                &format!(
                    "provider: {} {path}",
                    crate::log_redact::redact_argv(&control_prefix).join(" ")
                ),
                120,
            ),
            Source::Database,
        ),
    });
    match crate::worktree::default_branch_checked(repo_root) {
        Ok(base) => fields.push(Field::value(keys::WORKTREE_BASE, "Base", base, Source::Git)),
        Err(error) => fields.push(Field::unknown(keys::WORKTREE_BASE, "Base", error)),
    }
    let local = matches!(GitLoc::from_db(&wt, Some(&row.location)), GitLoc::Local(_));
    if let Some(facts) = &live.git {
        add_git_fields(
            &mut fields,
            facts,
            live.git_source.clone().unwrap_or(Source::Git),
        );
    } else if live.read_git && local {
        match crate::worktree::default_branch_checked(repo_root)
            .and_then(|base| git_facts(&base, worktree))
        {
            Ok(facts) => add_git_fields(&mut fields, &facts, Source::Git),
            Err(error) => fields.extend([
                Field::unknown(keys::WORKTREE_DIRTY, "Dirty", error.clone()),
                Field::unknown(keys::WORKTREE_AHEAD_BASE, "Ahead base", error.clone()),
                Field::unknown(keys::WORKTREE_UPSTREAM, "Upstream", error.clone()),
                Field::unknown(keys::WORKTREE_AHEAD_UPSTREAM, "Ahead upstream", error),
            ]),
        }
    } else {
        fields.extend([
            Field::not_measured(keys::WORKTREE_DIRTY, "Dirty"),
            Field::not_measured(keys::WORKTREE_AHEAD_BASE, "Ahead base"),
            Field::not_measured(keys::WORKTREE_UPSTREAM, "Upstream"),
            Field::not_measured(keys::WORKTREE_AHEAD_UPSTREAM, "Ahead upstream"),
        ]);
    }
    if let Ok(Some((json, _fetched_at))) = db.get_pr_cache(&GitLoc::worktree_cache_key(worktree)) {
        match serde_json::from_str::<PrPanel>(&json) {
            Ok(panel) if panel.branch == row.branch => {
                let field = match panel.state {
                    PanelState::Pr(status) => Field::value(
                        keys::WORKTREE_PR,
                        "PR",
                        format!(
                            "#{} {}{} {}",
                            status.number,
                            status.state,
                            if status.is_draft { " draft" } else { "" },
                            clean(&status.title, 60)
                        ),
                        Source::Cache {
                            name: "pr_cache".into(),
                            at: Some(panel.fetched_at),
                        },
                    ),
                    PanelState::NoPr => Field::value(
                        keys::WORKTREE_PR,
                        "PR",
                        "no open PR",
                        Source::Cache {
                            name: "pr_cache".into(),
                            at: Some(panel.fetched_at),
                        },
                    ),
                    other => Field::unknown(
                        keys::WORKTREE_PR,
                        "PR",
                        redacted_text(&format!("{other:?}"), 120),
                    ),
                };
                fields.push(field);
            }
            Ok(panel) => fields.push(Field::unknown(
                keys::WORKTREE_PR,
                "PR",
                format!("cached for branch {}", panel.branch),
            )),
            Err(error) => fields.push(Field::unknown(keys::WORKTREE_PR, "PR", error.to_string())),
        }
    } else {
        fields.push(Field::not_measured(keys::WORKTREE_PR, "PR"));
    }
    if let Ok(Some((json, fetched_at))) = db.get_ci_cache(&wt) {
        match crate::ci::decode_run_cache(&json)
            .and_then(|cache| cache.runs.into_iter().find(|run| run.branch == row.branch))
        {
            Some(run) => fields.push(Field::value(
                keys::WORKTREE_CI,
                "CI",
                format!(
                    "{} {}",
                    format!("{:?}", run.state).to_ascii_lowercase(),
                    run.name
                ),
                Source::Cache {
                    name: "ci_cache".into(),
                    at: Some(fetched_at),
                },
            )),
            None => fields.push(Field::not_measured(keys::WORKTREE_CI, "CI")),
        }
    } else {
        fields.push(Field::not_measured(keys::WORKTREE_CI, "CI"));
    }
    match db
        .linked_issues(&wt)
        .ok()
        .and_then(|ids| ids.first().map(|first| (first.clone(), ids.len())))
    {
        Some((first, count)) => fields.push(Field::value(
            keys::WORKTREE_ISSUE,
            "Issue",
            if count > 1 {
                format!("{first} (+{})", count - 1)
            } else {
                first
            },
            Source::Database,
        )),
        None => fields.push(Field::unset(
            keys::WORKTREE_ISSUE,
            "Issue",
            Source::Database,
        )),
    }
    if let Ok(dispatches) = db.list_dispatches() {
        let active = dispatches
            .into_iter()
            .filter(|dispatch| dispatch.worktree_path == wt && dispatch.status.is_active())
            .map(|dispatch| {
                format!(
                    "{} {} {} {}",
                    dispatch.issue_id,
                    dispatch.stage.unwrap_or_default(),
                    dispatch.agent_name,
                    dispatch.status.as_str()
                )
            })
            .collect::<Vec<_>>();
        fields.push(if active.is_empty() {
            Field::unset(keys::WORKTREE_DISPATCH, "Dispatch", Source::Database)
        } else {
            Field::value(
                keys::WORKTREE_DISPATCH,
                "Dispatch",
                active.join("; "),
                Source::Database,
            )
        });
    }
    fields.push(live.pipeline_stage.as_ref().map_or_else(
        || Field::not_measured(keys::WORKTREE_STAGE, "Stage"),
        |value| Field::value(keys::WORKTREE_STAGE, "Stage", value, Source::Sidebar),
    ));
    if let Ok(Some(queue)) = db
        .list_merge_queue()
        .map(|rows| rows.into_iter().find(|queue| queue.worktree == wt))
    {
        let has_error = queue.error_detail.is_some();
        let value = match queue.error_detail.as_deref() {
            Some(error) => format!(
                "{} → {}: {}",
                queue.status,
                queue.target_branch,
                redacted_text(error, 120)
            ),
            None => format!("{} → {}", queue.status, queue.target_branch),
        };
        let mut field = Field::value(
            keys::WORKTREE_MERGE_QUEUE,
            "Merge queue",
            value,
            Source::Database,
        );
        if has_error {
            field = field.warned();
        }
        fields.push(field);
    } else {
        fields.push(Field::unset(
            keys::WORKTREE_MERGE_QUEUE,
            "Merge queue",
            Source::Database,
        ));
    }
    match db.get_worktree_disk(&wt).ok().flatten() {
        Some((size, _, fetched_at)) => fields.push(Field::value(
            keys::WORKTREE_DISK,
            "Disk",
            crate::sandbox_manage::human_bytes(size.max(0) as u64),
            Source::Cache {
                name: "worktree_disk".into(),
                at: Some(fetched_at),
            },
        )),
        None => fields.push(Field::not_measured(keys::WORKTREE_DISK, "Disk")),
    }
    fields.push(live.activity.as_ref().map_or_else(
        || Field::not_measured(keys::WORKTREE_ACTIVITY, "Activity"),
        |value| Field::value(keys::WORKTREE_ACTIVITY, "Activity", value, Source::Sidebar),
    ));
    fields.push(live.devcontainer.as_ref().map_or_else(
        || Field::not_measured(keys::WORKTREE_DEVCONTAINER, "Devcontainer"),
        |value| {
            Field::value(
                keys::WORKTREE_DEVCONTAINER,
                "Devcontainer",
                value,
                Source::Sidebar,
            )
        },
    ));
    fields
}

/// Explicit database/config overrides that participate in resolving a target.
pub fn overrides(
    cfg: &Config,
    db: &crate::db::Db,
    repo_root: &Path,
    worktree: Option<&Path>,
) -> Vec<Field> {
    let mut fields = Vec::new();
    let wt = worktree.map(path_string);
    if let Some(value) = wt
        .as_deref()
        .and_then(|path| db.worktree_env(path).ok().flatten())
    {
        fields.push(Field::value(
            keys::OVERRIDE_WORKTREE_ENV,
            "Worktree env",
            value,
            Source::WorktreeDb,
        ));
    }
    if let Some(value) = wt
        .as_deref()
        .and_then(|path| db.worktree_sandbox_profile(path).ok().flatten())
    {
        fields.push(Field::value(
            keys::OVERRIDE_PROFILE_PIN,
            "Profile pin",
            value.as_str(),
            Source::WorktreeDb,
        ));
    }
    if let Some(value) = wt
        .as_deref()
        .and_then(|path| db.worktree_sandbox(path).ok().flatten())
        .filter(|value| {
            let value = value.trim();
            !value.is_empty() && value != "auto"
        })
    {
        fields.push(Field::value(
            keys::OVERRIDE_BACKEND_PICK,
            "Backend pick",
            value,
            Source::WorktreePick,
        ));
    }
    if let Some(value) = db.workspace_env(&path_string(repo_root)).ok().flatten() {
        fields.push(Field::value(
            keys::OVERRIDE_WORKSPACE_ENV,
            "Workspace env",
            value,
            Source::WorkspaceDb,
        ));
    }
    if let crate::workspace_overlay::WorkspaceOverlay::Selected { key, overlay } =
        cfg.workspace_overlay(repo_root)
    {
        let default =
            serde_json::to_value(crate::config::WorkspaceConfig::default()).unwrap_or_default();
        let mut value = serde_json::to_value(overlay).unwrap_or_default();
        crate::redact::redact_json(&mut value);
        if let serde_json::Value::Object(map) = value {
            let mut names = map
                .iter()
                .filter(|(key, value)| {
                    key.as_str() != "repo_path" && default.get(key.as_str()) != Some(value)
                })
                .map(|(key, _)| key)
                .cloned()
                .collect::<Vec<_>>();
            names.sort();
            if !names.is_empty() {
                fields.push(Field::value(
                    keys::OVERRIDE_WORKSPACE_BLOCK,
                    "Workspace block",
                    names.join(", "),
                    Source::Workspace(key.to_string()),
                ));
            }
        }
    }
    if let Some(candidate) = crate::config_repo::discover_repo_overlay(repo_root).selected() {
        fields.push(Field::value(
            keys::OVERRIDE_REPO_OVERLAY,
            "Repo overlay",
            candidate.path.display().to_string(),
            Source::Repo(candidate.path.display().to_string()),
        ));
    }
    if let Some(path) = wt
        && let Ok(Some(zone)) = db.zone_of_worktree(&path)
    {
        fields.push(Field::value(
            keys::OVERRIDE_ZONE,
            "Zone",
            zone.name.clone(),
            Source::Zone(zone.name),
        ));
    }
    fields
}

/// Keep only sessions belonging to the selected worktree or workspace.
pub fn sessions_for(
    all: &[SessionRow],
    repo_root: &Path,
    worktrees: &[String],
    worktree: Option<&Path>,
) -> Vec<SessionRow> {
    all.iter()
        .filter(|session| match (worktree, session.worktree.as_deref()) {
            (Some(worktree), Some(path)) => path == worktree.to_string_lossy().as_ref(),
            (Some(_), None) => false,
            (None, Some(path)) => {
                worktrees.iter().any(|wt| wt == path) || Path::new(path).starts_with(repo_root)
            }
            (None, None) => false,
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let output = crate::util::git_cmd(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .expect("git fixture command starts");
        assert!(
            output.status.success(),
            "git fixture command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn committed_repo(dir: &Path) {
        git(dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("fixture"), "first\n").expect("write fixture");
        git(dir, &["add", "fixture"]);
        git(dir, &["commit", "-q", "-m", "initial"]);
    }

    #[test]
    fn resolve_target_prefers_registered_path_and_unique_branch() {
        let db = crate::db::Db::open_memory().unwrap();
        db.put_workspace("/repo", "Repo", "repo").unwrap();
        db.put_worktree("repo/wt", "/repo", "/repo/wt", "feature", None, None)
            .unwrap();
        let by_path = resolve_target(&db, None, Some("/repo/wt"), Path::new("/tmp")).unwrap();
        assert_eq!(by_path.worktree.as_deref(), Some("/repo/wt"));
        let by_branch = resolve_target(&db, None, Some("feature"), Path::new("/tmp")).unwrap();
        assert_eq!(by_branch, by_path);
    }

    #[test]
    fn resolve_target_rejects_ambiguous_branch_and_mismatched_workspace() {
        let db = crate::db::Db::open_memory().unwrap();
        db.put_workspace("/repo", "Repo", "repo").unwrap();
        db.put_workspace("/other", "Other", "repo").unwrap();
        db.put_worktree("repo/a", "/repo", "/repo/a", "same", None, None)
            .unwrap();
        db.put_worktree("other/a", "/other", "/other/a", "same", None, None)
            .unwrap();
        assert!(resolve_target(&db, None, Some("same"), Path::new("/tmp")).is_err());
        assert!(resolve_target(&db, Some("/repo"), Some("/other/a"), Path::new("/tmp")).is_err());
    }

    #[test]
    fn resolve_target_rejects_here_outside_registered_workspaces() {
        let db = crate::db::Db::open_memory().unwrap();
        db.put_workspace("/repo", "Repo", "repo").unwrap();
        db.put_worktree("repo/a", "/repo", "/deleted/a", "a", None, None)
            .unwrap();
        let here = tempfile::tempdir().unwrap();

        let error = resolve_target(&db, None, None, here.path()).unwrap_err();
        assert!(error.to_string().contains("not inside a thegn workspace"));
    }

    #[test]
    fn resolve_target_accepts_workspace_slug() {
        let db = crate::db::Db::open_memory().unwrap();
        db.put_workspace("/repo", "Repo", "repo").unwrap();
        db.slug_for_repo("/repo", "project").unwrap();
        let target = resolve_target(&db, Some("project"), None, Path::new("/tmp")).unwrap();
        assert_eq!(target.repo_root, "/repo");
        assert!(target.worktree.is_none());
    }

    #[test]
    fn workspace_stats_counts_only_cached_disk_rows() {
        let db = crate::db::Db::open_memory().unwrap();
        db.put_workspace("/repo", "Repo", "repo").unwrap();
        db.put_worktree("repo/a", "/repo", "/repo/a", "a", None, None)
            .unwrap();
        db.put_worktree("repo/b", "/repo", "/repo/b", "b", None, None)
            .unwrap();
        db.put_worktree_disk("/repo/a", 1_500, 0).unwrap();
        let fields = workspace_stats(&db, Path::new("/repo"), &LiveFacts::default(), 0);
        let disk = fields
            .iter()
            .find(|field| field.key == keys::STATS_DISK)
            .unwrap();
        assert_eq!(
            disk.shown,
            crate::info_model::Shown::Value("1.5 kB (1 of 2 measured)".into())
        );
    }

    #[test]
    fn pr_cache_for_another_branch_is_unknown() {
        let db = crate::db::Db::open_memory().unwrap();
        db.put_worktree("repo/a", "/repo", "/repo/a", "current", None, None)
            .unwrap();
        db.put_pr_cache(
            "/repo/a",
            "other",
            r#"{"kind":"no_pr","worktree":"/repo/a","branch":"other","fetched_at":42}"#,
        )
        .unwrap();
        let fields = worktree_fields(
            &db,
            Path::new("/repo"),
            Path::new("/repo/a"),
            &LiveFacts::default(),
            0,
        );
        let pr = fields
            .iter()
            .find(|field| field.key == keys::WORKTREE_PR)
            .unwrap();
        assert!(matches!(pr.shown, crate::info_model::Shown::Unknown(_)));
    }

    #[test]
    fn worktree_git_facts_are_not_measured_when_git_read_is_disabled() {
        let db = crate::db::Db::open_memory().unwrap();
        db.put_worktree("repo/a", "/repo", "/repo/a", "a", None, None)
            .unwrap();
        let fields = worktree_fields(
            &db,
            Path::new("/repo"),
            Path::new("/repo/a"),
            &LiveFacts {
                read_git: false,
                ..LiveFacts::default()
            },
            0,
        );

        for key in [keys::WORKTREE_DIRTY, keys::WORKTREE_AHEAD_BASE] {
            let field = fields.iter().find(|field| field.key == key).unwrap();
            assert_eq!(field.shown, crate::info_model::Shown::NotMeasured);
        }
    }

    #[test]
    fn worktree_git_facts_report_one_commit_ahead_of_base() {
        let dir = tempfile::tempdir().unwrap();
        committed_repo(dir.path());
        git(dir.path(), &["checkout", "-q", "-b", "feature"]);
        std::fs::write(dir.path().join("fixture"), "second\n").expect("write fixture");
        git(dir.path(), &["add", "fixture"]);
        git(dir.path(), &["commit", "-q", "-m", "feature"]);

        let db = crate::db::Db::open_memory().unwrap();
        let path = dir.path().to_string_lossy().into_owned();
        db.put_worktree(&path, &path, &path, "feature", None, None)
            .unwrap();
        let fields = worktree_fields(
            &db,
            dir.path(),
            dir.path(),
            &LiveFacts {
                read_git: true,
                ..LiveFacts::default()
            },
            0,
        );

        let ahead = fields
            .iter()
            .find(|field| field.key == keys::WORKTREE_AHEAD_BASE)
            .unwrap();
        assert_eq!(
            ahead.shown,
            crate::info_model::Shown::Value("1".to_string())
        );
    }

    #[test]
    fn workspace_identity_scrubs_origin_credentials() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]);
        git(
            dir.path(),
            &["remote", "add", "origin", "https://u:t@github.com/a/b"],
        );
        let db = crate::db::Db::open_memory().unwrap();
        let path = dir.path().to_string_lossy().into_owned();
        db.put_workspace(&path, "Repo", "repo").unwrap();

        let origin = workspace_identity(&Config::default(), &db, dir.path())
            .into_iter()
            .find(|field| field.key == keys::WORKSPACE_ORIGIN)
            .unwrap();
        assert_eq!(
            origin.shown,
            crate::info_model::Shown::Value("https://github.com/a/b".to_string())
        );
    }

    #[test]
    fn merge_queue_error_detail_is_redacted() {
        let db = crate::db::Db::open_memory().unwrap();
        db.put_worktree("repo/a", "/repo", "/repo/a", "a", None, None)
            .unwrap();
        db.enqueue_merge("/repo/a", "a", "main").unwrap();
        db.update_merge_status(
            "/repo/a",
            "gate_error",
            None,
            None,
            Some("fatal: unable to access 'https://u:tok@github.com/a/b/'"),
        )
        .unwrap();

        let merge_queue = worktree_fields(
            &db,
            Path::new("/repo"),
            Path::new("/repo/a"),
            &LiveFacts::default(),
            0,
        )
        .into_iter()
        .find(|field| field.key == keys::WORKTREE_MERGE_QUEUE)
        .unwrap();
        match merge_queue.shown {
            crate::info_model::Shown::Value(value) => assert!(!value.contains("tok")),
            shown => panic!("expected merge queue value, got {shown:?}"),
        }
    }

    #[test]
    fn overrides_reports_worktree_env_backend_pick_and_zone() {
        let db = crate::db::Db::open_memory().unwrap();
        db.put_workspace("/repo", "Repo", "repo").unwrap();
        db.put_worktree("repo/a", "/repo", "/repo/a", "a", None, None)
            .unwrap();
        db.set_worktree_env("/repo/a", "dev").unwrap();
        db.set_worktree_sandbox("/repo/a", "bwrap").unwrap();
        let zone = db.create_zone("client", 1).unwrap();
        db.assign_workspace_zone("/repo", Some(zone)).unwrap();

        let fields = overrides(
            &Config::default(),
            &db,
            Path::new("/repo"),
            Some(Path::new("/repo/a")),
        );
        assert!(fields.iter().any(|field| {
            field.key == keys::OVERRIDE_WORKTREE_ENV
                && field.shown == crate::info_model::Shown::Value("dev".to_string())
        }));
        assert!(fields.iter().any(|field| {
            field.key == keys::OVERRIDE_BACKEND_PICK
                && field.shown == crate::info_model::Shown::Value("bwrap".to_string())
        }));
        assert!(fields.iter().any(|field| {
            field.key == keys::OVERRIDE_ZONE
                && field.shown == crate::info_model::Shown::Value("client".to_string())
        }));
    }

    #[test]
    fn sessions_for_filters_worktree_and_workspace_rows() {
        let all = vec![
            SessionRow {
                id: "a".into(),
                program: "claude".into(),
                worktree: Some("/repo/wt".into()),
                attached: 1,
            },
            SessionRow {
                id: "b".into(),
                program: "codex".into(),
                worktree: Some("/repo/other".into()),
                attached: 0,
            },
            SessionRow {
                id: "c".into(),
                program: "claude".into(),
                worktree: None,
                attached: 1,
            },
        ];
        assert_eq!(
            sessions_for(&all, Path::new("/repo"), &["/repo/wt".into()], None).len(),
            2
        );
        assert_eq!(
            sessions_for(&all, Path::new("/repo"), &[], Some(Path::new("/repo/wt"))).len(),
            1
        );
    }

    #[test]
    fn parse_count_reads_git_left_right_order() {
        assert_eq!(parse_count("2\t5").unwrap(), (2, 5));
    }
}
