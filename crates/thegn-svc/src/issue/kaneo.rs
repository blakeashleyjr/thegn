//! Kaneo (self-hosted, open-source PM) REST backend.
//!
//! Auth: `Authorization: Bearer <api_key>` — Kaneo's `authenticate-api-request`
//! runs `verifyApiKey` on the bearer token before falling back to a better-auth
//! session, so a static API key (or a stored device-flow token) works as a
//! bearer. All requests target `{base_url}/api/…`.
//!
//! Kaneo's hierarchy is workspace → project → task, and a task's `status` is a
//! **project-scoped column slug** (a free-form string, not a fixed enum). The
//! tasks endpoint returns tasks grouped under their columns, each column
//! carrying an `isFinal` flag (a "done" column) — we use that plus a name
//! heuristic to map onto thegn's fixed [`IssueStatus`]. On a status update the
//! target [`IssueStatus`] is resolved back to a concrete column slug for the
//! task's project (see `resolve_status_slug`).

use reqwest::Client;
use serde::{Deserialize, Serialize};
use thegn_core::issue::{
    Issue, IssueComment, IssueDetail, IssueDraft, IssueFilter, IssuePatch, IssuePriority,
    IssueStatus,
};

use super::{IssueBackend, IssueError};

pub struct KaneoBackend {
    client: Client,
    /// Origin without a trailing slash and without the `/api` suffix.
    base_url: String,
    api_key: String,
    workspace_id: Option<String>,
    project_id: Option<String>,
}

impl KaneoBackend {
    pub fn new(
        base_url: String,
        api_key: String,
        workspace_id: Option<String>,
        project_id: Option<String>,
    ) -> Self {
        KaneoBackend {
            // Bounded timeouts: a stalled tracker must not pin a background
            // permit forever (mirrors gh.rs's OCTOCRAB_REQUEST_TIMEOUT). Falls
            // back to the default client if the builder somehow fails.
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .connect_timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap_or_default(),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            workspace_id: workspace_id.filter(|s| !s.is_empty()),
            project_id: project_id.filter(|s| !s.is_empty()),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/{}", self.base_url, path.trim_start_matches('/'))
    }

    async fn get<R: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<R, IssueError> {
        let resp = self
            .client
            .get(self.url(path))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Accept", "application/json")
            .send()
            .await?;
        if resp.status() == 401 || resp.status() == 403 {
            return Err(IssueError::Auth(format!("Kaneo HTTP {}", resp.status())));
        }
        if !resp.status().is_success() {
            return Err(IssueError::Api(format!("Kaneo HTTP {}", resp.status())));
        }
        resp.json()
            .await
            .map_err(|e| IssueError::Parse(e.to_string()))
    }

    async fn send_body<B: Serialize, R: for<'de> Deserialize<'de>>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: &B,
    ) -> Result<R, IssueError> {
        let resp = self
            .client
            .request(method, self.url(path))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(body)
            .send()
            .await?;
        if resp.status() == 401 || resp.status() == 403 {
            return Err(IssueError::Auth(format!("Kaneo HTTP {}", resp.status())));
        }
        if !resp.status().is_success() {
            let txt = resp.text().await.unwrap_or_default();
            return Err(IssueError::Api(format!("Kaneo {path}: {txt}")));
        }
        resp.json()
            .await
            .map_err(|e| IssueError::Parse(e.to_string()))
    }

    async fn delete_req(&self, path: &str) -> Result<(), IssueError> {
        let resp = self
            .client
            .delete(self.url(path))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Accept", "application/json")
            .send()
            .await?;
        if resp.status() == 401 || resp.status() == 403 {
            return Err(IssueError::Auth(format!("Kaneo HTTP {}", resp.status())));
        }
        if !resp.status().is_success() {
            let txt = resp.text().await.unwrap_or_default();
            return Err(IssueError::Api(format!("Kaneo DELETE {path}: {txt}")));
        }
        Ok(())
    }

    /// Best-effort resolve of the authenticated user's id, so `assignee_me` can
    /// narrow via the `assigneeId` query param. Works for session / device-flow
    /// tokens (`/auth/get-session` returns a user); under API-key auth the
    /// session is null and we return `None` (⇒ no assignee narrowing).
    async fn current_user_id(&self) -> Option<String> {
        let session: SessionResp = self.get("auth/get-session").await.ok()?;
        session.user.map(|u| u.id)
    }

    /// The project ids to scan: the single configured project, else every
    /// project in the configured workspace. Empty when neither is configured.
    async fn scope_project_ids(&self) -> Result<Vec<String>, IssueError> {
        if let Some(pid) = &self.project_id {
            return Ok(vec![pid.clone()]);
        }
        let Some(ws) = &self.workspace_id else {
            // Nothing to scope to and no cheap way to enumerate workspaces over
            // REST — surface as unconfigured so the router logs once, not a hard
            // error (the panel then shows this account as empty).
            return Ok(Vec::new());
        };
        let projects: Vec<KaneoProject> = self.get(&format!("project?workspaceId={ws}")).await?;
        Ok(projects.into_iter().map(|p| p.id).collect())
    }

    /// Fetch the board for one project and flatten it to domain issues, tagging
    /// each with the status derived from its column (`isFinal` + name).
    async fn project_issues(
        &self,
        project_id: &str,
        assignee_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Issue>, IssueError> {
        let mut path = format!("task/tasks/{project_id}?limit={}", limit.clamp(1, 100));
        if let Some(uid) = assignee_id {
            path.push_str(&format!("&assigneeId={uid}"));
        }
        let board: BoardResp = self.get(&path).await?;
        let mut out = Vec::new();
        for col in board.data.columns {
            let status = map_column_status(&col.slug, &col.name, col.is_final);
            for t in col.tasks {
                out.push(task_to_domain(
                    t,
                    status,
                    &self.base_url,
                    board.data.workspace_id.as_deref(),
                ));
            }
        }
        Ok(out)
    }
}

// ---- Kaneo JSON response shapes ---------------------------------------------

#[derive(Deserialize)]
struct SessionResp {
    #[serde(default)]
    user: Option<SessionUser>,
}
#[derive(Deserialize)]
struct SessionUser {
    id: String,
}

#[derive(Deserialize)]
struct KaneoProject {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    slug: String,
}

/// A workspace label row (`GET /label/workspace/:id` / `GET /label/task/:id`).
#[derive(Deserialize)]
struct KaneoLabelRow {
    id: String,
    #[serde(default)]
    name: String,
}

/// One project in a workspace, for `thegn kaneo projects`.
#[derive(Debug, Clone)]
pub struct KaneoProjectInfo {
    pub id: String,
    pub name: String,
    pub slug: String,
}

/// One board column with its issues, for `thegn kaneo board`.
#[derive(Debug, Clone)]
pub struct KaneoColumnInfo {
    pub name: String,
    pub slug: String,
    pub is_final: bool,
    pub issues: Vec<Issue>,
}

/// `GET /task/tasks/:projectId` → `{ data: { columns: [...] , workspaceId } }`.
#[derive(Deserialize)]
struct BoardResp {
    data: BoardData,
}
#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct BoardData {
    columns: Vec<BoardColumn>,
    workspace_id: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BoardColumn {
    #[serde(default)]
    slug: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    is_final: bool,
    #[serde(default)]
    tasks: Vec<KaneoTask>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct KaneoTask {
    id: String,
    #[serde(default)]
    number: Option<i64>,
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    status: String,
    #[serde(default)]
    priority: String,
    #[serde(default)]
    project_id: String,
    #[serde(default)]
    assignee_name: Option<String>,
    #[serde(default)]
    labels: Vec<KaneoLabel>,
    #[serde(default)]
    created_at: Option<String>,
}

#[derive(Deserialize)]
struct KaneoLabel {
    #[serde(default)]
    name: String,
}

// ---- domain conversion ------------------------------------------------------

fn map_priority(p: &str) -> IssuePriority {
    match p {
        "urgent" => IssuePriority::Urgent,
        "high" => IssuePriority::High,
        "medium" => IssuePriority::Medium,
        "low" => IssuePriority::Low,
        _ => IssuePriority::None,
    }
}

fn priority_to_kaneo(p: IssuePriority) -> &'static str {
    match p {
        IssuePriority::Urgent => "urgent",
        IssuePriority::High => "high",
        IssuePriority::Medium => "medium",
        IssuePriority::Low => "low",
        IssuePriority::None => "no-priority",
    }
}

/// Map a Kaneo column (slug + display name + `isFinal`) onto a domain status.
/// `isFinal` columns are Done; otherwise a case-insensitive name/slug heuristic
/// covers the common column vocabularies, defaulting to Backlog.
fn map_column_status(slug: &str, name: &str, is_final: bool) -> IssueStatus {
    if is_final {
        return IssueStatus::Done;
    }
    let hay = format!(
        "{} {}",
        slug.to_ascii_lowercase(),
        name.to_ascii_lowercase()
    );
    let has = |needle: &str| hay.contains(needle);
    if has("cancel") || has("wont") || has("won't") || has("reject") || has("archiv") {
        IssueStatus::Cancelled
    } else if has("progress") || has("doing") || has("started") || has("review") || has("active") {
        IssueStatus::InProgress
    } else if has("done") || has("complete") || has("closed") || has("shipped") || has("finished") {
        IssueStatus::Done
    } else if has("todo") || has("to-do") || has("to do") || has("ready") || has("planned") {
        IssueStatus::Todo
    } else {
        IssueStatus::Backlog
    }
}

fn parse_ms(s: Option<&str>) -> i64 {
    s.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(0)
}

/// Best-effort web deep link to the task. Assumes the common single-origin
/// deploy where the web client is served from `base_url` and the API from
/// `base_url/api`; when the web client lives elsewhere this link may not
/// resolve, but the id/number still identify the task.
fn task_url(base_url: &str, workspace_id: Option<&str>, project_id: &str, task_id: &str) -> String {
    match workspace_id {
        Some(ws) if !ws.is_empty() => {
            format!("{base_url}/dashboard/{ws}/project/{project_id}/board?task={task_id}")
        }
        _ => format!("{base_url}/dashboard/project/{project_id}/board?task={task_id}"),
    }
}

fn task_to_domain(
    t: KaneoTask,
    status: IssueStatus,
    base_url: &str,
    workspace_id: Option<&str>,
) -> Issue {
    let number = t
        .number
        .map(|n| n.to_string())
        .unwrap_or_else(|| t.id.clone());
    let url = task_url(base_url, workspace_id, &t.project_id, &t.id);
    Issue {
        id: format!("kaneo:{}", t.id),
        number,
        provider: "kaneo".into(),
        title: t.title,
        body: t.description.filter(|s| !s.is_empty()),
        status,
        priority: map_priority(&t.priority),
        assignees: t.assignee_name.into_iter().collect(),
        labels: t.labels.into_iter().map(|l| l.name).collect(),
        url,
        branch_hint: None,
        updated_at_ms: parse_ms(t.created_at.as_deref()),
        project_ids: if t.project_id.is_empty() {
            Vec::new()
        } else {
            vec![t.project_id]
        },
        ..Default::default()
    }
}

/// Fetch a project's columns and resolve the column slug whose derived status
/// matches `target` (used to translate an [`IssueStatus`] update back into the
/// project-scoped column Kaneo expects). Returns `None` when the project has no
/// matching column.
async fn resolve_status_slug(
    backend: &KaneoBackend,
    project_id: &str,
    target: IssueStatus,
) -> Result<Option<String>, IssueError> {
    let board: BoardResp = backend
        .get(&format!("task/tasks/{project_id}?limit=1"))
        .await?;
    // Prefer an exact status match; fall back to a Done→final column.
    let mut fallback_final: Option<String> = None;
    for col in &board.data.columns {
        if map_column_status(&col.slug, &col.name, col.is_final) == target {
            return Ok(Some(col.slug.clone()));
        }
        if col.is_final && fallback_final.is_none() {
            fallback_final = Some(col.slug.clone());
        }
    }
    Ok(if matches!(target, IssueStatus::Done) {
        fallback_final
    } else {
        None
    })
}

// ---- project-management extras (Kaneo-specific board / project browsing) ----

impl KaneoBackend {
    fn require_workspace(&self) -> Result<&str, IssueError> {
        self.workspace_id.as_deref().ok_or_else(|| {
            IssueError::Api("this action requires a configured Kaneo workspace_id".into())
        })
    }

    /// Every project in the configured workspace.
    pub async fn list_projects(&self) -> Result<Vec<KaneoProjectInfo>, IssueError> {
        let ws = self.require_workspace()?;
        let projects: Vec<KaneoProject> = self.get(&format!("project?workspaceId={ws}")).await?;
        Ok(projects
            .into_iter()
            .map(|p| KaneoProjectInfo {
                id: p.id,
                name: p.name,
                slug: p.slug,
            })
            .collect())
    }

    /// A project's board: columns (in order) each with their issues.
    pub async fn board(&self, project_id: &str) -> Result<Vec<KaneoColumnInfo>, IssueError> {
        let board: BoardResp = self
            .get(&format!("task/tasks/{project_id}?limit=100"))
            .await?;
        let ws = board.data.workspace_id.clone();
        Ok(board
            .data
            .columns
            .into_iter()
            .map(|c| {
                let status = map_column_status(&c.slug, &c.name, c.is_final);
                let issues = c
                    .tasks
                    .into_iter()
                    .map(|t| task_to_domain(t, status, &self.base_url, ws.as_deref()))
                    .collect();
                KaneoColumnInfo {
                    name: c.name,
                    slug: c.slug,
                    is_final: c.is_final,
                    issues,
                }
            })
            .collect())
    }

    /// Move a task to another project (and optionally a target column/status).
    pub async fn move_task(
        &self,
        id: &str,
        dest_project: &str,
        dest_status: Option<&str>,
    ) -> Result<(), IssueError> {
        let task_id = id.strip_prefix("kaneo:").unwrap_or(id);
        let mut body = serde_json::json!({ "destinationProjectId": dest_project });
        if let Some(s) = dest_status {
            body["destinationStatus"] = serde_json::json!(s);
        }
        let _: serde_json::Value = self
            .send_body(reqwest::Method::PUT, &format!("task/move/{task_id}"), &body)
            .await?;
        Ok(())
    }
}

#[allow(async_fn_in_trait)]
impl IssueBackend for KaneoBackend {
    fn provider_id(&self) -> &'static str {
        "kaneo"
    }

    async fn list_issues(&self, filter: &IssueFilter) -> Result<Vec<Issue>, IssueError> {
        let projects = self.scope_project_ids().await?;
        if projects.is_empty() {
            return Ok(Vec::new());
        }
        let assignee = if filter.assignee_me {
            self.current_user_id().await
        } else {
            None
        };
        let per_project = filter.limit.clamp(1, 100);
        let mut all = Vec::new();
        for pid in projects {
            match self
                .project_issues(&pid, assignee.as_deref(), per_project)
                .await
            {
                Ok(issues) => all.extend(issues),
                Err(e) => {
                    tracing::warn!(project = %pid, error = %e, "kaneo project fetch failed")
                }
            }
        }
        // Client-side status filter (Kaneo filters by a single column slug, not
        // our status buckets) + overall limit.
        if !filter.statuses.is_empty() {
            all.retain(|i| filter.statuses.contains(&i.status));
        }
        all.sort_by_key(|i| std::cmp::Reverse(i.updated_at_ms));
        if filter.limit > 0 {
            all.truncate(filter.limit);
        }
        Ok(all)
    }

    async fn get_issue(&self, id: &str) -> Result<IssueDetail, IssueError> {
        let task_id = id.strip_prefix("kaneo:").unwrap_or(id);
        let task: KaneoTask = self.get(&format!("task/{task_id}")).await?;
        // A bare task fetch has no column `isFinal` context; map from the slug.
        let status = map_column_status(&task.status, &task.status, false);
        let issue = task_to_domain(task, status, &self.base_url, self.workspace_id.as_deref());
        let comments: Vec<KaneoComment> = self
            .get(&format!("comment/{task_id}"))
            .await
            .unwrap_or_default();
        let comments = comments
            .into_iter()
            .map(|c| IssueComment {
                author: c.author_name.unwrap_or_else(|| "unknown".into()),
                body: c.content,
                created_at_ms: parse_ms(c.created_at.as_deref()),
            })
            .collect();
        Ok(IssueDetail { issue, comments })
    }

    async fn create_issue(&self, draft: &IssueDraft) -> Result<Issue, IssueError> {
        let project_id = draft
            .project_id
            .clone()
            .or_else(|| self.project_id.clone())
            .ok_or_else(|| {
                IssueError::Api("Kaneo create requires a project id (config or draft)".into())
            })?;
        // Initial column: the first column of the project's board.
        let board: BoardResp = self
            .get(&format!("task/tasks/{project_id}?limit=1"))
            .await?;
        let status = board
            .data
            .columns
            .first()
            .map(|c| c.slug.clone())
            .unwrap_or_else(|| "to-do".into());

        #[derive(Serialize)]
        struct CreateBody {
            title: String,
            description: String,
            priority: &'static str,
            status: String,
        }
        let body = CreateBody {
            title: draft.title.clone(),
            description: draft.body.clone().unwrap_or_default(),
            priority: priority_to_kaneo(draft.priority),
            status,
        };
        let created: KaneoTask = self
            .send_body(reqwest::Method::POST, &format!("task/{project_id}"), &body)
            .await?;
        let status = map_column_status(&created.status, &created.status, false);
        Ok(task_to_domain(
            created,
            status,
            &self.base_url,
            self.workspace_id.as_deref(),
        ))
    }

    async fn update_issue(&self, id: &str, patch: &IssuePatch) -> Result<Issue, IssueError> {
        let task_id = id.strip_prefix("kaneo:").unwrap_or(id);

        if let Some(title) = &patch.title {
            #[derive(Serialize)]
            struct TitleBody<'a> {
                title: &'a str,
            }
            let _: serde_json::Value = self
                .send_body(
                    reqwest::Method::PUT,
                    &format!("task/title/{task_id}"),
                    &TitleBody { title },
                )
                .await?;
        }

        if let Some(p) = patch.priority {
            #[derive(Serialize)]
            struct PrioBody {
                priority: &'static str,
            }
            let _: serde_json::Value = self
                .send_body(
                    reqwest::Method::PUT,
                    &format!("task/priority/{task_id}"),
                    &PrioBody {
                        priority: priority_to_kaneo(p),
                    },
                )
                .await?;
        }

        if let Some(status) = patch.status {
            // Resolve the target status to a project column slug first (needs
            // the task's project id).
            let task: KaneoTask = self.get(&format!("task/{task_id}")).await?;
            if let Some(slug) = resolve_status_slug(self, &task.project_id, status).await? {
                #[derive(Serialize)]
                struct StatusBody {
                    status: String,
                }
                let _: serde_json::Value = self
                    .send_body(
                        reqwest::Method::PUT,
                        &format!("task/status/{task_id}"),
                        &StatusBody { status: slug },
                    )
                    .await?;
            } else {
                return Err(IssueError::Api(format!(
                    "no Kaneo column maps to status {:?} in this project",
                    status
                )));
            }
        }

        // Return the refreshed task.
        self.get_issue(id).await.map(|d| d.issue)
    }

    async fn search(&self, query: &str, limit: usize) -> Result<Vec<Issue>, IssueError> {
        // Kaneo has no workspace-wide text search over REST that we rely on, so
        // list within scope and filter titles client-side.
        let filter = IssueFilter {
            limit: limit.max(1),
            ..Default::default()
        };
        let needle = query.to_ascii_lowercase();
        let mut issues = self.list_issues(&filter).await?;
        issues.retain(|i| i.title.to_ascii_lowercase().contains(&needle));
        issues.truncate(limit.max(1));
        Ok(issues)
    }

    async fn add_comment(&self, id: &str, body: &str) -> Result<(), IssueError> {
        let task_id = id.strip_prefix("kaneo:").unwrap_or(id);
        let _: serde_json::Value = self
            .send_body(
                reqwest::Method::POST,
                &format!("comment/{task_id}"),
                &serde_json::json!({ "content": body }),
            )
            .await?;
        Ok(())
    }

    async fn attach_label(&self, id: &str, label: &str) -> Result<(), IssueError> {
        let task_id = id.strip_prefix("kaneo:").unwrap_or(id);
        let ws = self.require_workspace()?;
        // Reuse an existing workspace label of the same name; otherwise create
        // it and assign in one shot (labels are workspace-scoped).
        let existing: Vec<KaneoLabelRow> = self
            .get(&format!("label/workspace/{ws}"))
            .await
            .unwrap_or_default();
        if let Some(l) = existing.iter().find(|l| l.name.eq_ignore_ascii_case(label)) {
            let _: serde_json::Value = self
                .send_body(
                    reqwest::Method::PUT,
                    &format!("label/{}/task", l.id),
                    &serde_json::json!({ "taskId": task_id }),
                )
                .await?;
        } else {
            let _: serde_json::Value = self
                .send_body(
                    reqwest::Method::POST,
                    "label",
                    &serde_json::json!({
                        "name": label,
                        "color": "#6b7280",
                        "workspaceId": ws,
                        "taskId": task_id,
                    }),
                )
                .await?;
        }
        Ok(())
    }

    async fn detach_label(&self, id: &str, label: &str) -> Result<(), IssueError> {
        let task_id = id.strip_prefix("kaneo:").unwrap_or(id);
        let on_task: Vec<KaneoLabelRow> = self
            .get(&format!("label/task/{task_id}"))
            .await
            .unwrap_or_default();
        let Some(l) = on_task.iter().find(|l| l.name.eq_ignore_ascii_case(label)) else {
            return Err(IssueError::Api(format!("task has no label {label:?}")));
        };
        self.delete_req(&format!("label/{}/task", l.id)).await
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct KaneoComment {
    #[serde(default)]
    content: String,
    #[serde(default)]
    author_name: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn priority_maps_and_reverses() {
        assert_eq!(map_priority("urgent"), IssuePriority::Urgent);
        assert_eq!(map_priority("high"), IssuePriority::High);
        assert_eq!(map_priority("medium"), IssuePriority::Medium);
        assert_eq!(map_priority("low"), IssuePriority::Low);
        assert_eq!(map_priority("no-priority"), IssuePriority::None);
        assert_eq!(map_priority("weird"), IssuePriority::None);
        for p in [
            IssuePriority::Urgent,
            IssuePriority::High,
            IssuePriority::Medium,
            IssuePriority::Low,
        ] {
            assert_eq!(map_priority(priority_to_kaneo(p)), p, "round-trip {p:?}");
        }
        // None round-trips through the canonical "no-priority" slug.
        assert_eq!(priority_to_kaneo(IssuePriority::None), "no-priority");
        assert_eq!(map_priority("no-priority"), IssuePriority::None);
    }

    #[test]
    fn column_status_uses_is_final_then_name_heuristic() {
        // isFinal wins outright.
        assert_eq!(
            map_column_status("whatever", "Whatever", true),
            IssueStatus::Done
        );
        assert_eq!(
            map_column_status("in-progress", "In Progress", false),
            IssueStatus::InProgress
        );
        assert_eq!(
            map_column_status("to-do", "To Do", false),
            IssueStatus::Todo
        );
        assert_eq!(
            map_column_status("backlog", "Backlog", false),
            IssueStatus::Backlog
        );
        assert_eq!(map_column_status("done", "Done", false), IssueStatus::Done);
        assert_eq!(
            map_column_status("cancelled", "Cancelled", false),
            IssueStatus::Cancelled
        );
        assert_eq!(
            map_column_status("archived", "Archived", false),
            IssueStatus::Cancelled
        );
        assert_eq!(
            map_column_status("in-review", "In Review", false),
            IssueStatus::InProgress
        );
        // Unknown vocabulary defaults to Backlog.
        assert_eq!(
            map_column_status("frobnicate", "Frobnicate", false),
            IssueStatus::Backlog
        );
    }

    #[test]
    fn parse_ms_valid_and_invalid() {
        assert_eq!(parse_ms(Some("1970-01-01T00:00:02Z")), 2000);
        assert_eq!(parse_ms(Some("garbage")), 0);
        assert_eq!(parse_ms(None), 0);
    }

    #[test]
    fn board_response_flattens_to_issues() {
        let board: BoardResp = serde_json::from_value(json!({
            "data": {
                "workspaceId": "ws-1",
                "columns": [
                    {
                        "slug": "in-progress", "name": "In Progress", "isFinal": false,
                        "tasks": [{
                            "id": "t1", "number": 7, "title": "Ship it",
                            "description": "do the thing", "status": "in-progress",
                            "priority": "high", "projectId": "p1",
                            "assigneeName": "Fox Mulder",
                            "labels": [{ "name": "feature" }],
                            "createdAt": "1970-01-01T00:00:04Z"
                        }]
                    },
                    {
                        "slug": "done", "name": "Done", "isFinal": true,
                        "tasks": [{
                            "id": "t2", "number": 8, "title": "Old task",
                            "status": "done", "priority": "no-priority", "projectId": "p1"
                        }]
                    }
                ]
            }
        }))
        .unwrap();

        let mut issues = Vec::new();
        for col in board.data.columns {
            let status = map_column_status(&col.slug, &col.name, col.is_final);
            for t in col.tasks {
                issues.push(task_to_domain(
                    t,
                    status,
                    "https://kaneo.example.com",
                    board.data.workspace_id.as_deref(),
                ));
            }
        }
        assert_eq!(issues.len(), 2);
        let a = &issues[0];
        assert_eq!(a.id, "kaneo:t1");
        assert_eq!(a.number, "7");
        assert_eq!(a.provider, "kaneo");
        assert_eq!(a.title, "Ship it");
        assert_eq!(a.body.as_deref(), Some("do the thing"));
        assert_eq!(a.status, IssueStatus::InProgress);
        assert_eq!(a.priority, IssuePriority::High);
        assert_eq!(a.assignees, vec!["Fox Mulder".to_string()]);
        assert_eq!(a.labels, vec!["feature".to_string()]);
        assert_eq!(a.updated_at_ms, 4000);
        assert_eq!(a.project_ids, vec!["p1".to_string()]);
        assert!(
            a.url.contains("ws-1") && a.url.contains("p1") && a.url.contains("t1"),
            "url: {}",
            a.url
        );
        // The final column maps to Done regardless of name.
        assert_eq!(issues[1].status, IssueStatus::Done);
    }

    #[test]
    fn task_missing_optionals_tolerated() {
        let t: KaneoTask = serde_json::from_value(json!({
            "id": "bare",
            "status": "backlog",
            "priority": "low",
            "projectId": "p9"
        }))
        .unwrap();
        let issue = task_to_domain(t, IssueStatus::Backlog, "https://k", None);
        // No `number` ⇒ number falls back to the opaque id.
        assert_eq!(issue.number, "bare");
        assert_eq!(issue.title, "");
        assert_eq!(issue.body, None);
        assert!(issue.assignees.is_empty());
        assert!(issue.labels.is_empty());
        assert_eq!(issue.updated_at_ms, 0);
    }

    #[test]
    fn url_falls_back_without_workspace() {
        let u = task_url("https://k", None, "p1", "t1");
        assert!(u.contains("/project/p1/") && u.contains("task=t1"), "{u}");
        assert!(!u.contains("//project"), "no empty workspace segment: {u}");
    }

    #[test]
    fn project_deserializes_name_and_slug() {
        let p: KaneoProject = serde_json::from_value(json!({
            "id": "p1", "name": "Web App", "slug": "web", "workspaceId": "ws-1"
        }))
        .unwrap();
        assert_eq!(p.id, "p1");
        assert_eq!(p.name, "Web App");
        assert_eq!(p.slug, "web");
        // Missing name/slug default to empty (tolerant).
        let bare: KaneoProject = serde_json::from_value(json!({ "id": "p2" })).unwrap();
        assert_eq!(bare.name, "");
        assert_eq!(bare.slug, "");
    }

    #[test]
    fn label_row_deserializes() {
        let l: KaneoLabelRow = serde_json::from_value(json!({
            "id": "l1", "name": "bug", "color": "#ff0000", "taskId": "t1"
        }))
        .unwrap();
        assert_eq!(l.id, "l1");
        assert_eq!(l.name, "bug");
    }
}
