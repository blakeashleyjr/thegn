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

use serde::{Deserialize, Serialize};
use thegn_core::issue::{
    Issue, IssueComment, IssueDetail, IssueDraft, IssueFilter, IssuePatch, IssuePriority,
    IssueStatus,
};

use super::http::{TrackerHttpBudget, TrackerHttpClient, TrackerHttpOperation};
use super::{IssueBackend, IssueError};
use futures_util::future::BoxFuture;

pub struct KaneoBackend {
    http: Option<TrackerHttpClient>,
    http_error: Option<&'static str>,
    /// Origin without a trailing slash, retained for issue permalink output.
    base_url: String,
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
        Self::new_with_budget(
            base_url,
            api_key,
            workspace_id,
            project_id,
            TrackerHttpBudget::process(),
        )
    }

    pub(crate) fn new_with_budget(
        base_url: String,
        api_key: String,
        workspace_id: Option<String>,
        project_id: Option<String>,
        budget: std::sync::Arc<TrackerHttpBudget>,
    ) -> Self {
        let authorization = format!("Bearer {api_key}");
        let (http, http_error) =
            match TrackerHttpClient::new("kaneo", &base_url, authorization, budget) {
                Ok(http) => (Some(http), None),
                Err(IssueError::Policy(message)) => (None, Some(message)),
                Err(_) => (None, Some("tracker HTTP client configuration failed")),
            };
        KaneoBackend {
            http,
            http_error,
            base_url: base_url.trim_end_matches('/').to_string(),
            workspace_id: workspace_id.filter(|s| !s.is_empty()),
            project_id: project_id.filter(|s| !s.is_empty()),
        }
    }

    fn http(&self) -> Result<&TrackerHttpClient, IssueError> {
        self.http.as_ref().ok_or_else(|| {
            IssueError::Policy(self.http_error.unwrap_or("tracker HTTP origin refused"))
        })
    }

    async fn get<R: for<'de> Deserialize<'de>>(
        op: &mut TrackerHttpOperation<'_>,
        path: &str,
    ) -> Result<R, IssueError> {
        op.get(&api_path(path)?).await
    }

    async fn send_body<B: Serialize, R: for<'de> Deserialize<'de>>(
        op: &mut TrackerHttpOperation<'_>,
        method: reqwest::Method,
        path: &str,
        body: &B,
    ) -> Result<R, IssueError> {
        op.json(method, &api_path(path)?, body).await
    }

    async fn delete_req(op: &mut TrackerHttpOperation<'_>, path: &str) -> Result<(), IssueError> {
        op.empty(reqwest::Method::DELETE, &api_path(path)?).await
    }

    /// Best-effort resolve of the authenticated user's id, so `assignee_me` can
    /// narrow via the `assigneeId` query param. Works for session / device-flow
    /// tokens (`/auth/get-session` returns a user); under API-key auth the
    /// session is null and we return `None` (⇒ no assignee narrowing).
    async fn current_user_id(&self, op: &mut TrackerHttpOperation<'_>) -> Option<String> {
        let session: SessionResp = Self::get(op, "auth/get-session").await.ok()?;
        session.user.map(|u| u.id)
    }

    /// The project ids to scan: the single configured project, else every
    /// project in the configured workspace. Empty when neither is configured.
    async fn scope_project_ids(
        &self,
        op: &mut TrackerHttpOperation<'_>,
    ) -> Result<Vec<String>, IssueError> {
        if let Some(pid) = &self.project_id {
            checked_id(pid, "Kaneo project id")?;
            return Ok(vec![pid.clone()]);
        }
        let Some(ws) = &self.workspace_id else {
            // Nothing to scope to and no cheap way to enumerate workspaces over
            // REST — surface as unconfigured so the router logs once, not a hard
            // error (the panel then shows this account as empty).
            return Ok(Vec::new());
        };
        checked_id(ws, "Kaneo workspace id")?;
        let projects: Vec<KaneoProject> =
            Self::get(op, &format!("project?workspaceId={ws}")).await?;
        projects
            .into_iter()
            .map(|p| checked_id(&p.id, "Kaneo project id").map(str::to_owned))
            .collect()
    }

    /// Fetch the board for one project and flatten it to domain issues, tagging
    /// each with the status derived from its column (`isFinal` + name).
    async fn project_issues(
        &self,
        op: &mut TrackerHttpOperation<'_>,
        project_id: &str,
        assignee_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Issue>, IssueError> {
        checked_id(project_id, "Kaneo project id")?;
        if let Some(uid) = assignee_id {
            checked_id(uid, "Kaneo assignee id")?;
        }
        let mut path = format!("task/tasks/{project_id}?limit={}", limit.clamp(1, 100));
        if let Some(uid) = assignee_id {
            path.push_str(&format!("&assigneeId={uid}"));
        }
        let board: BoardResp = Self::get(op, &path).await?;
        let mut out = Vec::new();
        for col in board.data.columns {
            let status = map_column_status(&col.slug, &col.name, col.is_final);
            for t in col.tasks {
                out.push(task_to_domain(
                    t,
                    status,
                    &self.base_url,
                    board.data.workspace_id.as_deref(),
                )?);
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
    /// RFC3339 timestamp (Kaneo `dueDate`); absent when unset.
    #[serde(default)]
    due_date: Option<String>,
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
    let Ok(mut url) = reqwest::Url::parse(base_url) else {
        return String::new();
    };
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return String::new();
    }
    {
        let Ok(mut segments) = url.path_segments_mut() else {
            return String::new();
        };
        segments.clear();
        segments.push("dashboard");
        if let Some(ws) = workspace_id.filter(|ws| !ws.is_empty()) {
            segments.push(ws);
        }
        segments.push("project");
        segments.push(project_id);
        segments.push("board");
    }
    url.set_query(None);
    url.query_pairs_mut().append_pair("task", task_id);
    url.to_string()
}

fn task_to_domain(
    t: KaneoTask,
    status: IssueStatus,
    base_url: &str,
    workspace_id: Option<&str>,
) -> Result<Issue, IssueError> {
    checked_id(&t.id, "Kaneo task id")?;
    if !t.project_id.is_empty() {
        checked_id(&t.project_id, "Kaneo project id")?;
    }
    if let Some(ws) = workspace_id.filter(|ws| !ws.is_empty()) {
        checked_id(ws, "Kaneo workspace id")?;
    }
    let number = t
        .number
        .map(|n| n.to_string())
        .unwrap_or_else(|| t.id.clone());
    let url = task_url(base_url, workspace_id, &t.project_id, &t.id);
    Ok(Issue {
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
        due_at_ms: t.due_date.as_deref().and_then(super::parse_due_date_ms),
        project_ids: if t.project_id.is_empty() {
            Vec::new()
        } else {
            vec![t.project_id]
        },
        ..Default::default()
    })
}

/// Encode the bounded provider route without selecting an account authority.
/// The shared HTTP operation joins this relative path to its admitted base URL.
fn api_path(path: &str) -> Result<String, IssueError> {
    super::http::ensure_dynamic_input(path)?;
    let (route, query) = path.split_once('?').unwrap_or((path, ""));
    let route = route.trim_start_matches('/');
    for segment in route.split('/') {
        super::identity::builtin_segment(segment, "Kaneo route segment")
            .map_err(IssueError::Parse)?;
    }
    // This URL is only an encoder; its authority is never sent to a client.
    let mut encoded = reqwest::Url::parse("http://path.invalid/").expect("static encoder URL");
    encoded.set_path(&format!("/api/{route}"));
    if !query.is_empty() {
        let mut pairs = encoded.query_pairs_mut();
        for pair in query.split('&') {
            let (key, value) = pair.split_once('=').ok_or_else(|| {
                IssueError::Parse("Kaneo query must contain key=value pairs".into())
            })?;
            super::identity::builtin_segment(key, "Kaneo query key").map_err(IssueError::Parse)?;
            super::identity::builtin_segment(value, "Kaneo query value")
                .map_err(IssueError::Parse)?;
            pairs.append_pair(key, value);
        }
    }
    let mut result = encoded.path().to_owned();
    if let Some(query) = encoded.query() {
        result.push('?');
        result.push_str(query);
    }
    Ok(result)
}

fn checked_id<'a>(raw: &'a str, label: &str) -> Result<&'a str, IssueError> {
    super::identity::kaneo_id(raw, label).map_err(IssueError::Parse)
}

fn checked_task_id(raw: &str) -> Result<&str, IssueError> {
    checked_id(raw.strip_prefix("kaneo:").unwrap_or(raw), "Kaneo task id")
}

/// Fetch a project's columns and resolve the column slug whose derived status
/// matches `target` (used to translate an [`IssueStatus`] update back into the
/// project-scoped column Kaneo expects). Returns `None` when the project has no
/// matching column.
async fn resolve_status_slug(
    op: &mut TrackerHttpOperation<'_>,
    project_id: &str,
    target: IssueStatus,
) -> Result<Option<String>, IssueError> {
    checked_id(project_id, "Kaneo project id")?;
    let board: BoardResp =
        KaneoBackend::get(op, &format!("task/tasks/{project_id}?limit=1")).await?;
    // Prefer an exact status match; fall back to a Done→final column.
    let mut fallback_final: Option<String> = None;
    for col in &board.data.columns {
        checked_id(&col.slug, "Kaneo status slug")?;
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
        let http = self.http()?;
        let mut op = http.operation();
        op.prepare().await?;
        let ws = self.require_workspace()?;
        checked_id(ws, "Kaneo workspace id")?;
        let projects: Vec<KaneoProject> =
            Self::get(&mut op, &format!("project?workspaceId={ws}")).await?;
        projects
            .into_iter()
            .map(|p| {
                checked_id(&p.id, "Kaneo project id")?;
                Ok(KaneoProjectInfo {
                    id: p.id,
                    name: p.name,
                    slug: p.slug,
                })
            })
            .collect()
    }

    /// A project's board: columns (in order) each with their issues.
    pub async fn board(&self, project_id: &str) -> Result<Vec<KaneoColumnInfo>, IssueError> {
        let http = self.http()?;
        let mut op = http.operation();
        op.prepare().await?;
        checked_id(project_id, "Kaneo project id")?;
        let board: BoardResp =
            Self::get(&mut op, &format!("task/tasks/{project_id}?limit=100")).await?;
        let ws = board.data.workspace_id.clone();
        board
            .data
            .columns
            .into_iter()
            .map(|c| {
                let status = map_column_status(&c.slug, &c.name, c.is_final);
                let issues = c
                    .tasks
                    .into_iter()
                    .map(|t| task_to_domain(t, status, &self.base_url, ws.as_deref()))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(KaneoColumnInfo {
                    name: c.name,
                    slug: c.slug,
                    is_final: c.is_final,
                    issues,
                })
            })
            .collect()
    }

    /// Move a task to another project (and optionally a target column/status).
    pub async fn move_task(
        &self,
        id: &str,
        dest_project: &str,
        dest_status: Option<&str>,
    ) -> Result<(), IssueError> {
        let http = self.http()?;
        let mut op = http.operation();
        op.prepare().await?;
        let task_id = checked_task_id(id)?;
        checked_id(dest_project, "Kaneo destination project id")?;
        let mut body = serde_json::json!({ "destinationProjectId": dest_project });
        if let Some(s) = dest_status {
            body["destinationStatus"] = serde_json::json!(s);
        }
        let _: serde_json::Value = Self::send_body(
            &mut op,
            reqwest::Method::PUT,
            &format!("task/move/{task_id}"),
            &body,
        )
        .await?;
        Ok(())
    }
}

impl KaneoBackend {
    async fn list_issues_with_op(
        &self,
        op: &mut TrackerHttpOperation<'_>,
        filter: &IssueFilter,
    ) -> Result<Vec<Issue>, IssueError> {
        let projects = self.scope_project_ids(op).await?;
        if projects.is_empty() {
            return Ok(Vec::new());
        }
        let assignee = if filter.assignee_me {
            self.current_user_id(op).await
        } else {
            None
        };
        let per_project = filter.limit.clamp(1, 100);
        let mut all = Vec::new();
        for pid in projects {
            match self
                .project_issues(op, &pid, assignee.as_deref(), per_project)
                .await
            {
                Ok(issues) => all.extend(issues),
                Err(e @ IssueError::Timeout(_)) => return Err(e),
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
}

impl IssueBackend for KaneoBackend {
    fn provider_id(&self) -> &'static str {
        "kaneo"
    }

    fn caps(&self) -> super::IssueCaps {
        super::IssueCaps {
            comments: true,
            labels: true,
        }
    }

    /// The router downcasts through this for Kaneo-shaped board/project
    /// browsing (columns per project), which is not provider-agnostic.
    fn as_kaneo(&self) -> Option<&KaneoBackend> {
        Some(self)
    }

    fn list_issues<'a>(
        &'a self,
        filter: &'a IssueFilter,
    ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
        Box::pin(async move {
            let http = self.http()?;
            let mut op = http.operation();
            op.prepare().await?;
            self.list_issues_with_op(&mut op, filter).await
        })
    }

    fn get_issue<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<IssueDetail, IssueError>> {
        Box::pin(async move {
            let http = self.http()?;
            let mut op = http.operation();
            op.prepare().await?;
            let task_id = checked_task_id(id)?;
            let task: KaneoTask = Self::get(&mut op, &format!("task/{task_id}")).await?;
            // A bare task fetch has no column `isFinal` context; map from the slug.
            let status = map_column_status(&task.status, &task.status, false);
            let issue = task_to_domain(task, status, &self.base_url, self.workspace_id.as_deref())?;
            let comments: Vec<KaneoComment> = Self::get(&mut op, &format!("comment/{task_id}"))
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
        })
    }

    fn create_issue<'a>(
        &'a self,
        draft: &'a IssueDraft,
    ) -> BoxFuture<'a, Result<Issue, IssueError>> {
        Box::pin(async move {
            let http = self.http()?;
            let mut op = http.operation();
            op.prepare().await?;
            let project_id = draft
                .project_id
                .clone()
                .or_else(|| self.project_id.clone())
                .ok_or_else(|| {
                    IssueError::Api("Kaneo create requires a project id (config or draft)".into())
                })?;
            checked_id(&project_id, "Kaneo project id")?;
            // Initial column: the first column of the project's board.
            let board: BoardResp =
                Self::get(&mut op, &format!("task/tasks/{project_id}?limit=1")).await?;
            let status = board
                .data
                .columns
                .first()
                .map(|c| c.slug.clone())
                .unwrap_or_else(|| "to-do".into());

            #[derive(Serialize)]
            struct CreateBody<'a> {
                title: &'a str,
                description: &'a str,
                priority: &'static str,
                status: String,
            }
            let body = CreateBody {
                title: &draft.title,
                description: draft.body.as_deref().unwrap_or_default(),
                priority: priority_to_kaneo(draft.priority),
                status,
            };
            let created: KaneoTask = Self::send_body(
                &mut op,
                reqwest::Method::POST,
                &format!("task/{project_id}"),
                &body,
            )
            .await?;
            let status = map_column_status(&created.status, &created.status, false);
            Ok(task_to_domain(
                created,
                status,
                &self.base_url,
                self.workspace_id.as_deref(),
            )?)
        })
    }

    fn update_issue<'a>(
        &'a self,
        id: &'a str,
        patch: &'a IssuePatch,
    ) -> BoxFuture<'a, Result<Issue, IssueError>> {
        Box::pin(async move {
            let http = self.http()?;
            let mut op = http.operation();
            op.prepare().await?;
            let task_id = checked_task_id(id)?;

            if let Some(title) = &patch.title {
                #[derive(Serialize)]
                struct TitleBody<'a> {
                    title: &'a str,
                }
                let _: serde_json::Value = Self::send_body(
                    &mut op,
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
                let _: serde_json::Value = Self::send_body(
                    &mut op,
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
                let task: KaneoTask = Self::get(&mut op, &format!("task/{task_id}")).await?;
                checked_id(&task.project_id, "Kaneo project id")?;
                if let Some(slug) = resolve_status_slug(&mut op, &task.project_id, status).await? {
                    #[derive(Serialize)]
                    struct StatusBody {
                        status: String,
                    }
                    let _: serde_json::Value = Self::send_body(
                        &mut op,
                        reqwest::Method::PUT,
                        &format!("task/status/{task_id}"),
                        &StatusBody { status: slug },
                    )
                    .await?;
                } else {
                    return Err(IssueError::Api("Kaneo status target unavailable".into()));
                }
            }

            // Return the refreshed task.
            let task: KaneoTask = Self::get(&mut op, &format!("task/{task_id}")).await?;
            let status = map_column_status(&task.status, &task.status, false);
            task_to_domain(task, status, &self.base_url, self.workspace_id.as_deref())
        })
    }

    fn search<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
        Box::pin(async move {
            let http = self.http()?;
            let mut op = http.operation();
            op.prepare().await?;
            // Kaneo has no workspace-wide text search over REST that we rely on, so
            // list within scope and filter titles client-side.
            let filter = IssueFilter {
                limit: limit.max(1),
                ..Default::default()
            };
            let needle = query.to_ascii_lowercase();
            let mut issues = self.list_issues_with_op(&mut op, &filter).await?;
            issues.retain(|i| i.title.to_ascii_lowercase().contains(&needle));
            issues.truncate(limit.max(1));
            Ok(issues)
        })
    }

    fn add_comment<'a>(
        &'a self,
        id: &'a str,
        body: &'a str,
    ) -> BoxFuture<'a, Result<(), IssueError>> {
        Box::pin(async move {
            let http = self.http()?;
            let mut op = http.operation();
            op.prepare().await?;
            let task_id = checked_task_id(id)?;
            #[derive(Serialize)]
            struct CommentBody<'a> {
                content: &'a str,
            }
            let _: serde_json::Value = Self::send_body(
                &mut op,
                reqwest::Method::POST,
                &format!("comment/{task_id}"),
                &CommentBody { content: body },
            )
            .await?;
            Ok(())
        })
    }

    fn attach_label<'a>(
        &'a self,
        id: &'a str,
        label: &'a str,
    ) -> BoxFuture<'a, Result<(), IssueError>> {
        Box::pin(async move {
            let http = self.http()?;
            let mut op = http.operation();
            op.prepare().await?;
            let task_id = checked_task_id(id)?;
            let ws = self.require_workspace()?;
            checked_id(ws, "Kaneo workspace id")?;
            // Reuse an existing workspace label of the same name; otherwise create
            // it and assign in one shot (labels are workspace-scoped).
            let existing: Vec<KaneoLabelRow> = Self::get(&mut op, &format!("label/workspace/{ws}"))
                .await
                .unwrap_or_default();
            if let Some(l) = existing.iter().find(|l| l.name.eq_ignore_ascii_case(label)) {
                checked_id(&l.id, "Kaneo label id")?;
                let _: serde_json::Value = Self::send_body(
                    &mut op,
                    reqwest::Method::PUT,
                    &format!("label/{}/task", l.id),
                    &serde_json::json!({ "taskId": task_id }),
                )
                .await?;
            } else {
                let _: serde_json::Value = Self::send_body(
                    &mut op,
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
        })
    }

    fn detach_label<'a>(
        &'a self,
        id: &'a str,
        label: &'a str,
    ) -> BoxFuture<'a, Result<(), IssueError>> {
        Box::pin(async move {
            let http = self.http()?;
            let mut op = http.operation();
            op.prepare().await?;
            let task_id = checked_task_id(id)?;
            let on_task: Vec<KaneoLabelRow> = Self::get(&mut op, &format!("label/task/{task_id}"))
                .await
                .unwrap_or_default();
            let Some(l) = on_task.iter().find(|l| l.name.eq_ignore_ascii_case(label)) else {
                return Err(IssueError::Api("Kaneo label target unavailable".into()));
            };
            checked_id(&l.id, "Kaneo label id")?;
            Self::delete_req(&mut op, &format!("label/{}/task", l.id)).await
        })
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
    use axum::Router;
    use axum::body::Body;
    use axum::extract::Request;
    use axum::http::{HeaderValue, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::any;
    use serde_json::json;
    use std::sync::Arc;

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
                issues.push(
                    task_to_domain(
                        t,
                        status,
                        "https://kaneo.example.com",
                        board.data.workspace_id.as_deref(),
                    )
                    .unwrap(),
                );
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
        // Exercise the actual Kaneo producer output at the router/cache
        // identity boundary; task links legitimately carry `?task=`.
        assert!(
            crate::issue::validate_issue_identity(a).is_ok(),
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
        let issue = task_to_domain(t, IssueStatus::Backlog, "https://k", None).unwrap();
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
    fn api_url_encodes_structured_query_and_rejects_traversal() {
        let encoded = api_path("task/tasks/p1?workspaceId=客户").unwrap();
        assert!(
            encoded.contains("workspaceId=%E5%AE%A2%E6%88%B7"),
            "{encoded}"
        );
        assert!(api_path("task/../secret").is_err());
        assert!(api_path("task/t?workspaceId=a%26evil%3Dx").is_err());
        assert!(api_path("task/t?workspaceId=a&evil=x").is_ok());
    }

    #[test]
    fn task_link_uses_url_segments_and_query_encoding() {
        let u = task_url("https://k", Some("ws/客户"), "p/1", "t#1");
        assert!(
            u.contains("/dashboard/ws%2F%E5%AE%A2%E6%88%B7/project/p%2F1/board"),
            "{u}"
        );
        assert!(u.ends_with("?task=t%231"), "{u}");
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
    #[tokio::test]
    async fn list_expansion_returns_budget_error_before_kaneo_project_fetch() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let route_calls = Arc::clone(&calls);
        let server = tokio::spawn(async move {
            let app = Router::new().fallback(any(move |_: Request| {
                let route_calls = Arc::clone(&route_calls);
                async move {
                    let call = route_calls.fetch_add(1, Ordering::SeqCst);
                    let body = if call == 0 {
                        json!([{"id": "project-1"}])
                    } else {
                        json!({"data": {"columns": []}})
                    };
                    let mut response =
                        (StatusCode::OK, Body::from(body.to_string())).into_response();
                    response.headers_mut().insert(
                        reqwest::header::CONTENT_TYPE,
                        HeaderValue::from_static("application/json"),
                    );
                    response
                }
            }));
            axum::serve(listener, app).await.unwrap();
        });
        let budget = Arc::new(TrackerHttpBudget::with_permits(1));
        budget.expire_after_responses_for_test(1);
        let backend = KaneoBackend::new_with_budget(
            format!("http://{address}"),
            "kaneo-secret".into(),
            Some("workspace-1".into()),
            None,
            Arc::clone(&budget),
        );
        let result = backend
            .list_issues(&IssueFilter {
                limit: 1,
                ..Default::default()
            })
            .await;
        assert!(matches!(
            result,
            Err(IssueError::Timeout("operation deadline exceeded"))
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}
