//! Jira Cloud/Server REST v3 backend.
//!
//! Auth: `Authorization: Basic base64(email:api_token)`.
//! All requests target `/rest/api/3/…` endpoints.

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use serde::{Deserialize, Serialize};
use thegn_core::issue::{
    Issue, IssueComment, IssueDetail, IssueDraft, IssueFilter, IssuePatch, IssuePriority,
    IssueStatus,
};

use super::http::{
    TrackerHttpBudget, TrackerHttpClient, TrackerHttpOperation, ensure_dynamic_input,
};
use super::{IssueBackend, IssueError};
use futures_util::future::BoxFuture;

pub struct JiraBackend {
    http: Option<TrackerHttpClient>,
    http_error: Option<&'static str>,
    /// Configured instance URL, retained for provider-independent browse links.
    base_url: String,
    project_key: Option<String>,
}

impl JiraBackend {
    pub fn new(
        base_url: String,
        email: String,
        api_token: String,
        project_key: Option<String>,
    ) -> Self {
        Self::new_with_budget(
            base_url,
            email,
            api_token,
            project_key,
            TrackerHttpBudget::process(),
        )
    }

    pub(crate) fn new_with_budget(
        base_url: String,
        email: String,
        api_token: String,
        project_key: Option<String>,
        budget: std::sync::Arc<TrackerHttpBudget>,
    ) -> Self {
        let creds = format!("{email}:{api_token}");
        let auth = format!("Basic {}", B64.encode(creds.as_bytes()));
        let (http, http_error) = match TrackerHttpClient::new("jira", &base_url, auth, budget) {
            Ok(http) => (Some(http), None),
            Err(IssueError::Policy(message)) => (None, Some(message)),
            Err(_) => (None, Some("tracker HTTP client configuration failed")),
        };
        JiraBackend {
            http,
            http_error,
            base_url,
            project_key,
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
        op.get(&format!("/rest/api/3/{}", path.trim_start_matches('/')))
            .await
    }

    async fn post<B: Serialize, R: for<'de> Deserialize<'de>>(
        op: &mut TrackerHttpOperation<'_>,
        path: &str,
        body: &B,
    ) -> Result<R, IssueError> {
        op.json(
            reqwest::Method::POST,
            &format!("/rest/api/3/{}", path.trim_start_matches('/')),
            body,
        )
        .await
    }

    async fn put<B: Serialize>(
        op: &mut TrackerHttpOperation<'_>,
        path: &str,
        body: &B,
    ) -> Result<(), IssueError> {
        op.json_empty(
            reqwest::Method::PUT,
            &format!("/rest/api/3/{}", path.trim_start_matches('/')),
            body,
        )
        .await
        .map(|_| ())
    }

    async fn post_empty<B: Serialize>(
        op: &mut TrackerHttpOperation<'_>,
        path: &str,
        body: &B,
    ) -> Result<(), IssueError> {
        op.json_empty(
            reqwest::Method::POST,
            &format!("/rest/api/3/{}", path.trim_start_matches('/')),
            body,
        )
        .await
    }
}

// ---- Jira JSON response shapes ----------------------------------------------

#[derive(Deserialize)]
struct SearchResult {
    issues: Vec<JiraIssue>,
}

#[derive(Deserialize)]
struct JiraIssue {
    #[allow(dead_code)]
    id: String,
    key: String,
    #[serde(rename = "self")]
    _self_url: String,
    fields: JiraFields,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct JiraFields {
    summary: String,
    description: Option<serde_json::Value>,
    status: Option<JiraStatus>,
    priority: Option<JiraPriority>,
    assignee: Option<JiraUser>,
    labels: Vec<String>,
    #[serde(rename = "updated")]
    updated: Option<String>,
    /// Date-only `YYYY-MM-DD` (Jira's `duedate` field); absent when unset.
    duedate: Option<String>,
    comment: Option<JiraCommentSection>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JiraStatus {
    #[allow(dead_code)]
    name: String,
    status_category: Option<JiraStatusCategory>,
}

#[derive(Deserialize)]
struct JiraStatusCategory {
    key: String, // "new" | "indeterminate" | "done"
}

#[derive(Deserialize)]
struct JiraPriority {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JiraUser {
    display_name: String,
}

#[derive(Deserialize)]
struct JiraCommentSection {
    comments: Vec<JiraComment>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JiraComment {
    body: Option<serde_json::Value>,
    author: Option<JiraUser>,
    created: Option<String>,
}

#[derive(Deserialize)]
struct JiraTransitions {
    transitions: Vec<JiraTransition>,
}

#[derive(Deserialize)]
struct JiraTransition {
    id: String,
    #[serde(rename = "to")]
    to: JiraTransitionState,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JiraTransitionState {
    #[allow(dead_code)]
    status_category: Option<JiraStatusCategory>,
    #[allow(dead_code)]
    name: String,
}

fn map_jira_status(status: &Option<JiraStatus>) -> IssueStatus {
    let cat = status
        .as_ref()
        .and_then(|s| s.status_category.as_ref())
        .map(|c| c.key.as_str());
    match cat {
        Some("new") => IssueStatus::Todo,
        Some("indeterminate") => IssueStatus::InProgress,
        Some("done") => IssueStatus::Done,
        _ => IssueStatus::Backlog,
    }
}

fn target_status_category(status: IssueStatus) -> &'static str {
    match status {
        IssueStatus::Backlog | IssueStatus::Todo => "new",
        IssueStatus::InProgress => "indeterminate",
        IssueStatus::Done | IssueStatus::Cancelled => "done",
    }
}

fn jira_status_category(status: &Option<JiraStatus>) -> Option<&str> {
    status
        .as_ref()
        .and_then(|status| status.status_category.as_ref())
        .map(|category| category.key.as_str())
}

fn partial_update(
    applied: &[&'static str],
    unapplied: &[&'static str],
    source: IssueError,
) -> IssueError {
    IssueError::PartialUpdate {
        applied: applied.to_vec(),
        unapplied: unapplied.to_vec(),
        source: Box::new(source),
    }
}

fn map_jira_priority(p: &Option<JiraPriority>) -> IssuePriority {
    match p.as_ref().map(|p| p.name.as_str()) {
        Some("Highest") => IssuePriority::Urgent,
        Some("High") => IssuePriority::High,
        Some("Medium") => IssuePriority::Medium,
        Some("Low") => IssuePriority::Low,
        Some("Lowest") => IssuePriority::Low,
        _ => IssuePriority::None,
    }
}

fn parse_ms(s: Option<&str>) -> i64 {
    s.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(0)
}

fn extract_text(val: &serde_json::Value) -> String {
    match val {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(m) => {
            // Atlassian Document Format (ADF) — walk `content` array.
            if let Some(content) = m.get("content").and_then(|c| c.as_array()) {
                content
                    .iter()
                    .map(extract_text)
                    .collect::<Vec<_>>()
                    .join(" ")
            } else if let Some(text) = m.get("text").and_then(|t| t.as_str()) {
                text.to_string()
            } else {
                String::new()
            }
        }
        serde_json::Value::Array(arr) => arr.iter().map(extract_text).collect::<Vec<_>>().join(" "),
        _ => String::new(),
    }
}

fn jira_issue_to_domain(ji: JiraIssue, configured_base_url: &str) -> Issue {
    let body = ji
        .fields
        .description
        .as_ref()
        .map(extract_text)
        .filter(|s| !s.is_empty());
    // The response's `self` link is untrusted; browse links stay on the
    // configured Jira origin and never inherit a response-controlled host.
    let url = jira_browse_url(configured_base_url, &ji.key);
    Issue {
        id: format!("jira:{}", ji.key),
        number: ji.key.clone(),
        provider: "jira".into(),
        title: ji.fields.summary,
        body,
        status: map_jira_status(&ji.fields.status),
        priority: map_jira_priority(&ji.fields.priority),
        assignees: ji
            .fields
            .assignee
            .map(|u| vec![u.display_name])
            .unwrap_or_default(),
        labels: ji.fields.labels,
        url,
        branch_hint: None,
        updated_at_ms: parse_ms(ji.fields.updated.as_deref()),
        due_at_ms: ji
            .fields
            .duedate
            .as_deref()
            .and_then(super::parse_due_date_ms),
        ..Default::default()
    }
}

fn jira_browse_url(configured_base_url: &str, key: &str) -> String {
    if super::identity::jira_key(key).is_err() {
        return String::new();
    }
    let Ok(mut base) = reqwest::Url::parse(configured_base_url) else {
        return String::new();
    };
    if !matches!(base.scheme(), "https" | "http")
        || base.host_str().is_none()
        || !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
    {
        return String::new();
    }
    let path = format!("{}/browse/{key}", base.path().trim_end_matches('/'));
    base.set_path(&path);
    base.set_query(None);
    base.to_string()
}

fn checked_jira_key(raw: &str) -> Result<&str, IssueError> {
    super::identity::jira_key(raw).map_err(IssueError::Parse)
}

fn project_filter_jql(project: &str) -> Result<String, IssueError> {
    super::identity::jira_project(project).map_err(IssueError::Parse)?;
    Ok(format!("project = \"{project}\""))
}

fn jira_path(key: &str, suffix: &str) -> Result<String, IssueError> {
    checked_jira_key(key)?;
    if !matches!(suffix, "" | "/transitions") {
        return Err(IssueError::Parse(
            "Jira path suffix contains an unstructured delimiter".into(),
        ));
    }
    Ok(format!("issue/{key}{suffix}"))
}

const JIRA_FIELDS: &str =
    "summary,description,status,priority,assignee,labels,updated,duedate,comment";

impl IssueBackend for JiraBackend {
    fn provider_id(&self) -> &'static str {
        "jira"
    }

    fn caps(&self) -> super::IssueCaps {
        super::IssueCaps::default()
    }

    fn list_issues<'a>(
        &'a self,
        filter: &'a IssueFilter,
    ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
        Box::pin(async move {
            let http = self.http()?;
            let mut op = http.operation();
            op.prepare().await?;
            if let Some(project_key) = self.project_key.as_deref() {
                ensure_dynamic_input(project_key)?;
            }
            if let Some(query) = filter.query.as_deref() {
                ensure_dynamic_input(query)?;
            }
            let mut jql_parts = Vec::new();

            if filter.assignee_me {
                jql_parts.push("assignee = currentUser()".to_string());
            }

            if let Some(proj) = &self.project_key {
                jql_parts.push(project_filter_jql(proj)?);
            }

            if !filter.statuses.is_empty() {
                jql_parts.push(status_category_jql(&filter.statuses));
            } else {
                // Default: active issues only.
                jql_parts.push(r#"statusCategory in ("To Do", "In Progress")"#.to_string());
            }

            if let Some(q) = &filter.query {
                jql_parts.push(format!("text ~ \"{}\"", escape_jql_str(q)));
            }

            let jql = if jql_parts.is_empty() {
                "ORDER BY updated DESC".to_string()
            } else {
                format!("{} ORDER BY updated DESC", jql_parts.join(" AND "))
            };

            let limit = filter.limit.min(100);
            let path = format!(
                "search?jql={}&fields={JIRA_FIELDS}&maxResults={limit}",
                urlencoding_simple(&jql)
            );
            let result: SearchResult = Self::get(&mut op, &path).await?;
            result
                .issues
                .into_iter()
                .map(|issue| {
                    checked_jira_key(&issue.key)?;
                    Ok(jira_issue_to_domain(issue, &self.base_url))
                })
                .collect()
        })
    }

    fn get_issue<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<IssueDetail, IssueError>> {
        Box::pin(async move {
            let http = self.http()?;
            let mut op = http.operation();
            op.prepare().await?;
            let key = id.strip_prefix("jira:").unwrap_or(id);
            checked_jira_key(key)?;
            let ji: JiraIssue = Self::get(
                &mut op,
                &format!("{}?fields={JIRA_FIELDS}", jira_path(key, "")?),
            )
            .await?;
            let comments = ji
                .fields
                .comment
                .as_ref()
                .map(|cs| &cs.comments)
                .into_iter()
                .flatten()
                .map(|c| IssueComment {
                    author: c
                        .author
                        .as_ref()
                        .map(|a| a.display_name.clone())
                        .unwrap_or_else(|| "unknown".into()),
                    body: c.body.as_ref().map(extract_text).unwrap_or_default(),
                    created_at_ms: parse_ms(c.created.as_deref()),
                })
                .collect();
            checked_jira_key(&ji.key)?;
            Ok(IssueDetail {
                issue: jira_issue_to_domain(ji, &self.base_url),
                comments,
            })
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
            let project_key = self
                .project_key
                .as_deref()
                .or(draft.project_id.as_deref())
                .ok_or_else(|| {
                    IssueError::Api("Jira create requires a project key in config".into())
                })?;
            ensure_dynamic_input(project_key)?;
            ensure_dynamic_input(&draft.title)?;
            if let Some(body) = draft.body.as_deref() {
                ensure_dynamic_input(body)?;
            }
            super::identity::jira_project(project_key).map_err(IssueError::Parse)?;

            let priority_name = match draft.priority {
                IssuePriority::Urgent => "Highest",
                IssuePriority::High => "High",
                IssuePriority::Medium => "Medium",
                IssuePriority::Low => "Low",
                IssuePriority::None => "Medium",
            };

            #[derive(Serialize)]
            struct CreateBody<'a> {
                fields: CreateFields<'a>,
            }
            #[derive(Serialize)]
            struct CreateFields<'a> {
                project: ProjectKey<'a>,
                summary: &'a str,
                description: Option<Description<'a>>,
                issuetype: IssueType,
                priority: PriorityName,
            }
            #[derive(Serialize)]
            struct ProjectKey<'a> {
                key: &'a str,
            }
            #[derive(Serialize)]
            struct Description<'a> {
                #[serde(rename = "type")]
                kind: &'static str,
                version: u8,
                content: [DescriptionBlock<'a>; 1],
            }
            #[derive(Serialize)]
            struct DescriptionBlock<'a> {
                #[serde(rename = "type")]
                kind: &'static str,
                content: [DescriptionText<'a>; 1],
            }
            #[derive(Serialize)]
            struct DescriptionText<'a> {
                #[serde(rename = "type")]
                kind: &'static str,
                text: &'a str,
            }
            #[derive(Serialize)]
            struct IssueType {
                name: &'static str,
            }
            #[derive(Serialize)]
            struct PriorityName {
                name: &'static str,
            }
            #[derive(Deserialize)]
            struct CreateResponse {
                key: String,
            }

            let body = CreateBody {
                fields: CreateFields {
                    project: ProjectKey { key: project_key },
                    summary: &draft.title,
                    description: draft.body.as_deref().map(|body| Description {
                        kind: "doc",
                        version: 1,
                        content: [DescriptionBlock {
                            kind: "paragraph",
                            content: [DescriptionText {
                                kind: "text",
                                text: body,
                            }],
                        }],
                    }),
                    issuetype: IssueType { name: "Task" },
                    priority: PriorityName {
                        name: priority_name,
                    },
                },
            };

            let created: CreateResponse = Self::post(&mut op, "issue", &body).await?;
            checked_jira_key(&created.key)?;
            let ji: JiraIssue = Self::get(
                &mut op,
                &format!("{}?fields={JIRA_FIELDS}", jira_path(&created.key, "")?),
            )
            .await?;
            checked_jira_key(&ji.key)?;
            Ok(jira_issue_to_domain(ji, &self.base_url))
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
            let key = id.strip_prefix("jira:").unwrap_or(id);
            checked_jira_key(key)?;
            if let Some(title) = patch.title.as_deref() {
                ensure_dynamic_input(title)?;
            }

            let mut applied = Vec::new();

            // Title is cosmetic, idempotent, and reversible, so apply it before
            // the workflow transition. A title failure leaves status untouched.
            if let Some(title) = &patch.title {
                #[derive(Serialize)]
                struct UpdateBody {
                    fields: UpdateFields,
                }
                #[derive(Serialize)]
                struct UpdateFields {
                    summary: String,
                }
                Self::put(
                    &mut op,
                    &jira_path(key, "")?,
                    &UpdateBody {
                        fields: UpdateFields {
                            summary: title.clone(),
                        },
                    },
                )
                .await?;
                applied.push("title");
            }

            // Status transitions can trigger irreversible workflow effects, so
            // they are the last mutation. If this fails after a title write,
            // retain the source error and report the field-level outcome.
            if let Some(status) = patch.status {
                let target_cat = target_status_category(status);
                let transitions: JiraTransitions =
                    match Self::get(&mut op, &jira_path(key, "/transitions")?).await {
                        Ok(transitions) => transitions,
                        Err(error) if !applied.is_empty() => {
                            return Err(partial_update(&applied, &["status"], error));
                        }
                        Err(error) => return Err(error),
                    };
                let trans = transitions
                    .transitions
                    .iter()
                    .find(|t| {
                        t.to.status_category
                            .as_ref()
                            .map(|c| c.key == target_cat)
                            .unwrap_or(false)
                    })
                    .ok_or_else(|| {
                        IssueError::Api(format!(
                            "Jira transition target unavailable for {key}: {} ({target_cat})",
                            status.label()
                        ))
                    });
                let trans = match trans {
                    Ok(trans) => trans,
                    Err(error) if !applied.is_empty() => {
                        return Err(partial_update(&applied, &["status"], error));
                    }
                    Err(error) => return Err(error),
                };

                #[derive(Serialize)]
                struct TransitionBody {
                    transition: TransitionId,
                }
                #[derive(Serialize)]
                struct TransitionId {
                    id: String,
                }
                if let Err(error) = Self::post_empty(
                    &mut op,
                    &jira_path(key, "/transitions")?,
                    &TransitionBody {
                        transition: TransitionId {
                            id: trans.id.clone(),
                        },
                    },
                )
                .await
                {
                    if !applied.is_empty() {
                        return Err(partial_update(&applied, &["status"], error));
                    }
                    return Err(error);
                }
                applied.push("status");
            }

            let ji: JiraIssue = Self::get(
                &mut op,
                &format!("{}?fields={JIRA_FIELDS}", jira_path(key, "")?),
            )
            .await
            .map_err(|error| {
                if applied.is_empty() {
                    error
                } else {
                    partial_update(&applied, &[], error)
                }
            })?;
            if let Err(error) = checked_jira_key(&ji.key) {
                return Err(if applied.is_empty() {
                    error
                } else {
                    partial_update(&applied, &[], error)
                });
            }

            if let Some(status) = patch.status {
                let target_cat = target_status_category(status);
                if jira_status_category(&ji.fields.status) != Some(target_cat) {
                    let error = IssueError::Api(format!(
                        "Jira status verification failed for {key}: expected category {target_cat}"
                    ));
                    return Err(partial_update(&applied, &[], error));
                }
            }
            Ok(jira_issue_to_domain(ji, &self.base_url))
        })
    }

    fn search<'a>(
        &'a self,
        query_str: &'a str,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
        Box::pin(async move {
            let http = self.http()?;
            let mut op = http.operation();
            op.prepare().await?;
            ensure_dynamic_input(query_str)?;
            // Escape JQL string-literal metachars first, then percent-encode the
            // whole `text ~ "…"` clause so quotes/backslashes in the query neither
            // break the JQL nor the query string.
            let jql = format!(
                "text ~ \"{}\" ORDER BY updated DESC",
                escape_jql_str(query_str)
            );
            let limit = limit.min(100);
            let path = format!(
                "search?jql={}&fields={JIRA_FIELDS}&maxResults={limit}",
                urlencoding_simple(&jql)
            );
            let result: SearchResult = Self::get(&mut op, &path).await?;
            result
                .issues
                .into_iter()
                .map(|issue| {
                    checked_jira_key(&issue.key)?;
                    Ok(jira_issue_to_domain(issue, &self.base_url))
                })
                .collect()
        })
    }
}

/// Build the `statusCategory in (...)` JQL clause for a set of domain statuses.
/// Filters on statusCategory, not status display names: category names
/// ("To Do"/"In Progress"/"Done") are fixed per Jira, whereas an instance may
/// rename its workflow statuses (Open/Resolved/…) — a `status in ("To Do")`
/// clause 400s the whole query on those.
fn status_category_jql(statuses: &[IssueStatus]) -> String {
    let cats_deduped: Vec<&str> = statuses
        .iter()
        .map(|s| match s {
            IssueStatus::Backlog | IssueStatus::Todo => "\"To Do\"",
            IssueStatus::InProgress => "\"In Progress\"",
            IssueStatus::Done | IssueStatus::Cancelled => "\"Done\"",
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    format!("statusCategory in ({})", cats_deduped.join(", "))
}

/// Escape a user string for embedding inside a JQL double-quoted literal.
/// JQL escapes with a backslash; a raw `"` or trailing `\` would otherwise
/// terminate/break the literal, and newlines are illegal inside one.
fn escape_jql_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out
}

/// Minimal percent-encoding for JQL query strings (no external dep needed).
/// `+` and `=` are NOT in the pass-through set: since ' ' is emitted as '+', a
/// literal '+' must be percent-encoded (%2B) so the server doesn't decode it
/// back to a space; '=' (%3D) would otherwise be read as a query-param break.
fn urlencoding_simple(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => out.push(c),
            ' ' => out.push('+'),
            _ => {
                for byte in c.to_string().as_bytes() {
                    out.push_str(&format!("%{byte:02X}"));
                }
            }
        }
    }
    out
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

    fn status(cat: &str) -> Option<JiraStatus> {
        serde_json::from_value(json!({
            "name": "whatever",
            "statusCategory": { "key": cat }
        }))
        .unwrap()
    }

    #[test]
    fn map_status_by_category_and_fallback() {
        assert_eq!(map_jira_status(&status("new")), IssueStatus::Todo);
        assert_eq!(
            map_jira_status(&status("indeterminate")),
            IssueStatus::InProgress
        );
        assert_eq!(map_jira_status(&status("done")), IssueStatus::Done);
        // Unknown category and a status with no category both fall back to Backlog.
        assert_eq!(map_jira_status(&status("mystery")), IssueStatus::Backlog);
        assert_eq!(map_jira_status(&None), IssueStatus::Backlog);
    }

    #[test]
    fn map_priority_by_name_and_fallback() {
        let p = |name: &str| -> Option<JiraPriority> {
            serde_json::from_value(json!({ "name": name })).unwrap()
        };
        assert_eq!(map_jira_priority(&p("Highest")), IssuePriority::Urgent);
        assert_eq!(map_jira_priority(&p("High")), IssuePriority::High);
        assert_eq!(map_jira_priority(&p("Medium")), IssuePriority::Medium);
        assert_eq!(map_jira_priority(&p("Low")), IssuePriority::Low);
        assert_eq!(map_jira_priority(&p("Lowest")), IssuePriority::Low);
        assert_eq!(map_jira_priority(&p("Trivial")), IssuePriority::None);
        assert_eq!(map_jira_priority(&None), IssuePriority::None);
    }

    #[test]
    fn parse_ms_valid_and_invalid() {
        assert_eq!(
            parse_ms(Some("1970-01-01T00:00:01Z")),
            1000,
            "one second past the epoch"
        );
        assert_eq!(parse_ms(Some("not-a-date")), 0);
        assert_eq!(parse_ms(None), 0);
    }

    #[test]
    fn extract_text_walks_adf_and_handles_edges() {
        // A realistic ADF doc with nested paragraphs.
        let doc = json!({
            "type": "doc",
            "content": [
                { "type": "paragraph", "content": [{ "type": "text", "text": "Hello" }] },
                { "type": "paragraph", "content": [
                    { "type": "text", "text": "world" },
                    { "type": "text", "text": "again" }
                ]}
            ]
        });
        assert_eq!(extract_text(&doc), "Hello world again");
        // A bare string node round-trips.
        assert_eq!(extract_text(&json!("plain")), "plain");
        // An object that is neither content-bearing nor a text node is empty.
        assert_eq!(extract_text(&json!({ "type": "hardBreak" })), "");
        // Null / number nodes contribute nothing.
        assert_eq!(extract_text(&json!(null)), "");
        assert_eq!(extract_text(&json!(42)), "");
    }

    #[test]
    fn issue_to_domain_maps_all_fields_and_derives_browse_url() {
        let ji: JiraIssue = serde_json::from_value(json!({
            "id": "10001",
            "key": "PROJ-7",
            "self": "https://myorg.atlassian.net/rest/api/3/issue/10001",
            "fields": {
                "summary": "Fix the thing",
                "description": {
                    "type": "doc",
                    "content": [{ "type": "paragraph",
                        "content": [{ "type": "text", "text": "details here" }] }]
                },
                "status": { "name": "In Progress",
                    "statusCategory": { "key": "indeterminate" } },
                "priority": { "name": "High" },
                "assignee": { "displayName": "Dana Scully" },
                "labels": ["bug", "p1"],
                "updated": "1970-01-01T00:00:02Z"
            }
        }))
        .unwrap();
        let issue = jira_issue_to_domain(ji, "https://jira.example");
        assert_eq!(issue.id, "jira:PROJ-7");
        assert_eq!(issue.number, "PROJ-7");
        assert_eq!(issue.provider, "jira");
        assert_eq!(issue.title, "Fix the thing");
        assert_eq!(issue.body.as_deref(), Some("details here"));
        assert_eq!(issue.status, IssueStatus::InProgress);
        assert_eq!(issue.priority, IssuePriority::High);
        assert_eq!(issue.assignees, vec!["Dana Scully".to_string()]);
        assert_eq!(issue.labels, vec!["bug".to_string(), "p1".to_string()]);
        assert_eq!(issue.url, "https://jira.example/browse/PROJ-7");
        assert_eq!(issue.updated_at_ms, 2000);
    }

    #[test]
    fn status_filter_uses_statuscategory_not_display_names() {
        // The clause must reference statusCategory (fixed names) — never a bare
        // `status in (...)` on renamable display names.
        let jql = status_category_jql(&[IssueStatus::Todo, IssueStatus::InProgress]);
        assert!(
            jql.starts_with("statusCategory in ("),
            "must filter on statusCategory, got: {jql}"
        );
        assert!(
            !jql.contains("status in ("),
            "must not use bare status: {jql}"
        );
        assert!(jql.contains("\"To Do\""));
        assert!(jql.contains("\"In Progress\""));
        // Backlog+Todo collapse to a single "To Do", Done+Cancelled to "Done".
        let all = status_category_jql(&[
            IssueStatus::Backlog,
            IssueStatus::Todo,
            IssueStatus::InProgress,
            IssueStatus::Done,
            IssueStatus::Cancelled,
        ]);
        assert_eq!(all.matches("\"To Do\"").count(), 1, "deduped: {all}");
        assert_eq!(all.matches("\"Done\"").count(), 1, "deduped: {all}");
    }

    #[test]
    fn project_filter_accepts_configured_project_without_issue_number() {
        assert_eq!(project_filter_jql("PROJ").unwrap(), "project = \"PROJ\"");
        assert!(project_filter_jql("PROJ-7").is_err());
    }

    #[test]
    fn urlencoding_percent_encodes_plus_and_equals() {
        // A literal '+' must become %2B (not pass through), else the server
        // decodes it as a space; '=' must become %3D; ' ' stays '+'.
        assert_eq!(urlencoding_simple("C++"), "C%2B%2B");
        assert_eq!(urlencoding_simple("a b"), "a+b");
        assert_eq!(urlencoding_simple("x=1"), "x%3D1");
        // Unreserved chars pass through untouched.
        assert_eq!(urlencoding_simple("A-z_0.~9"), "A-z_0.~9");
    }

    #[test]
    fn escape_jql_str_neutralizes_quote_backslash_newline() {
        assert_eq!(escape_jql_str(r#"say "hi""#), r#"say \"hi\""#);
        assert_eq!(escape_jql_str(r"trailing\"), r"trailing\\");
        assert_eq!(escape_jql_str("line1\nline2\rx"), "line1\\nline2\\rx");
        assert_eq!(escape_jql_str("plain"), "plain");
    }

    #[test]
    fn issue_to_domain_tolerates_missing_optionals() {
        // Only the required key/self plus an empty fields object.
        let ji: JiraIssue = serde_json::from_value(json!({
            "id": "1",
            "key": "X-1",
            "self": "https://h.example/rest/api/3/issue/1",
            "fields": {}
        }))
        .unwrap();
        let issue = jira_issue_to_domain(ji, "https://jira.example");
        assert_eq!(issue.title, "");
        assert_eq!(issue.body, None, "empty description filtered to None");
        assert_eq!(issue.status, IssueStatus::Backlog);
        assert_eq!(issue.priority, IssuePriority::None);
        assert!(issue.assignees.is_empty());
        assert_eq!(issue.updated_at_ms, 0);
        assert_eq!(issue.url, "https://jira.example/browse/X-1");
    }

    #[test]
    fn constructor_admits_self_hosted_base_path() {
        let backend = JiraBackend::new_with_budget(
            "http://jira.lan:8080/company/jira".into(),
            "user@example.test".into(),
            "jira-test-token".into(),
            Some("PROJ".into()),
            std::sync::Arc::new(TrackerHttpBudget::with_permits(1)),
        );
        assert!(backend.http().is_ok());
    }

    #[derive(Clone)]
    struct FixtureResponse {
        status: StatusCode,
        body: String,
    }

    impl FixtureResponse {
        fn new(status: StatusCode, body: impl Into<String>) -> Self {
            Self {
                status,
                body: body.into(),
            }
        }
    }

    fn transition_fixture(category: &str) -> FixtureResponse {
        FixtureResponse::new(
            StatusCode::OK,
            json!({
                "transitions": [{
                    "id": "31",
                    "to": {
                        "name": "fixture status",
                        "statusCategory": { "key": category }
                    }
                }]
            })
            .to_string(),
        )
    }

    fn issue_fixture(summary: &str, category: &str) -> FixtureResponse {
        FixtureResponse::new(
            StatusCode::OK,
            json!({
                "id": "10001",
                "key": "PROJ-1",
                "self": "http://jira.example/rest/api/3/issue/10001",
                "fields": {
                    "summary": summary,
                    "status": {
                        "name": "fixture status",
                        "statusCategory": { "key": category }
                    }
                }
            })
            .to_string(),
        )
    }

    async fn run_update(
        patch: IssuePatch,
        responses: Vec<FixtureResponse>,
    ) -> (Result<Issue, IssueError>, Vec<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let responses = Arc::new(responses);
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let route_responses = Arc::clone(&responses);
        let route_calls = Arc::clone(&calls);
        let server = tokio::spawn(async move {
            let app = Router::new().fallback(any(move |request: Request| {
                let route_responses = Arc::clone(&route_responses);
                let route_calls = Arc::clone(&route_calls);
                async move {
                    let call = {
                        let mut calls = route_calls.lock().unwrap();
                        let call = calls.len();
                        calls.push(format!("{} {}", request.method(), request.uri().path()));
                        call
                    };
                    let fixture = route_responses.get(call).cloned().unwrap_or_else(|| {
                        FixtureResponse::new(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "fixture response exhausted",
                        )
                    });
                    let has_json = !fixture.body.is_empty();
                    let mut response = (fixture.status, Body::from(fixture.body)).into_response();
                    if has_json {
                        response.headers_mut().insert(
                            reqwest::header::CONTENT_TYPE,
                            HeaderValue::from_static("application/json"),
                        );
                    }
                    response
                }
            }));
            axum::serve(listener, app).await.unwrap();
        });

        let backend = JiraBackend::new_with_budget(
            format!("http://{address}"),
            "user@example.test".into(),
            "jira-secret".into(),
            None,
            Arc::new(TrackerHttpBudget::with_permits(1)),
        );
        let result = backend.update_issue("jira:PROJ-1", &patch).await;
        server.abort();
        let _ = server.await;
        let calls = calls.lock().unwrap().clone();
        (result, calls)
    }

    #[test]
    fn target_status_categories_preserve_domain_equivalence() {
        assert_eq!(target_status_category(IssueStatus::Backlog), "new");
        assert_eq!(target_status_category(IssueStatus::Todo), "new");
        assert_eq!(
            target_status_category(IssueStatus::InProgress),
            "indeterminate"
        );
        assert_eq!(target_status_category(IssueStatus::Done), "done");
        assert_eq!(target_status_category(IssueStatus::Cancelled), "done");
    }

    #[tokio::test]
    async fn title_failure_does_not_attempt_status_transition() {
        let (result, calls) = run_update(
            IssuePatch {
                title: Some("new title".into()),
                status: Some(IssueStatus::Done),
                ..Default::default()
            },
            vec![FixtureResponse::new(StatusCode::BAD_GATEWAY, "secret-body")],
        )
        .await;

        assert!(matches!(
            result,
            Err(IssueError::Api(message)) if message == "jira HTTP 502"
        ));
        assert_eq!(calls, vec!["PUT /rest/api/3/issue/PROJ-1"]);
    }

    #[tokio::test]
    async fn title_success_then_status_http_failure_reports_partial_fields() {
        let (result, calls) = run_update(
            IssuePatch {
                title: Some("new title".into()),
                status: Some(IssueStatus::InProgress),
                ..Default::default()
            },
            vec![
                FixtureResponse::new(StatusCode::NO_CONTENT, ""),
                transition_fixture("indeterminate"),
                FixtureResponse::new(StatusCode::SERVICE_UNAVAILABLE, "secret-body"),
            ],
        )
        .await;

        match result {
            Err(IssueError::PartialUpdate {
                applied,
                unapplied,
                source,
            }) => {
                assert_eq!(applied, vec!["title"]);
                assert_eq!(unapplied, vec!["status"]);
                let source_debug = format!("{source:?}");
                assert!(
                    matches!(source.as_ref(), IssueError::Api(message) if message == "jira HTTP 503")
                );
                assert!(!source_debug.contains("secret-body"));
            }
            other => panic!("expected typed partial update, got {other:?}"),
        }
        assert_eq!(
            calls,
            vec![
                "PUT /rest/api/3/issue/PROJ-1",
                "GET /rest/api/3/issue/PROJ-1/transitions",
                "POST /rest/api/3/issue/PROJ-1/transitions",
            ]
        );
    }

    #[tokio::test]
    async fn transition_lookup_http_failure_is_propagated_without_followup() {
        let (result, calls) = run_update(
            IssuePatch {
                status: Some(IssueStatus::Done),
                ..Default::default()
            },
            vec![FixtureResponse::new(StatusCode::BAD_GATEWAY, "")],
        )
        .await;

        assert!(matches!(
            result,
            Err(IssueError::Api(message)) if message == "jira HTTP 502"
        ));
        assert_eq!(calls, vec!["GET /rest/api/3/issue/PROJ-1/transitions"]);
    }

    #[tokio::test]
    async fn unavailable_transition_is_bounded_and_does_not_post() {
        let (result, calls) = run_update(
            IssuePatch {
                status: Some(IssueStatus::InProgress),
                ..Default::default()
            },
            vec![transition_fixture("new")],
        )
        .await;

        match result {
            Err(IssueError::Api(message)) => {
                assert!(message.contains("In Progress"));
                assert!(message.contains("indeterminate"));
                assert!(!message.contains("secret-body"));
            }
            other => panic!("expected unavailable transition error, got {other:?}"),
        }
        assert_eq!(calls, vec!["GET /rest/api/3/issue/PROJ-1/transitions"]);
    }

    #[tokio::test]
    async fn stale_transition_id_propagates_post_failure_without_final_fetch() {
        let (result, calls) = run_update(
            IssuePatch {
                status: Some(IssueStatus::Done),
                ..Default::default()
            },
            vec![
                transition_fixture("done"),
                FixtureResponse::new(StatusCode::NOT_FOUND, "stale transition body"),
            ],
        )
        .await;

        assert!(matches!(
            result,
            Err(IssueError::Api(message)) if message == "jira HTTP 404"
        ));
        assert_eq!(
            calls,
            vec![
                "GET /rest/api/3/issue/PROJ-1/transitions",
                "POST /rest/api/3/issue/PROJ-1/transitions",
            ]
        );
    }

    #[tokio::test]
    async fn empty_transition_response_is_followed_by_verified_fetch() {
        let (result, calls) = run_update(
            IssuePatch {
                status: Some(IssueStatus::Backlog),
                ..Default::default()
            },
            vec![
                transition_fixture("new"),
                FixtureResponse::new(StatusCode::NO_CONTENT, ""),
                issue_fixture("existing title", "new"),
            ],
        )
        .await;

        let issue = result.expect("verified transition should succeed");
        assert_eq!(issue.status, IssueStatus::Todo);
        assert_eq!(
            calls,
            vec![
                "GET /rest/api/3/issue/PROJ-1/transitions",
                "POST /rest/api/3/issue/PROJ-1/transitions",
                "GET /rest/api/3/issue/PROJ-1",
            ]
        );
    }

    #[tokio::test]
    async fn partial_status_failure_can_retry_only_the_unapplied_field() {
        let (first, _) = run_update(
            IssuePatch {
                title: Some("new title".into()),
                status: Some(IssueStatus::Done),
                ..Default::default()
            },
            vec![
                FixtureResponse::new(StatusCode::NO_CONTENT, ""),
                transition_fixture("done"),
                FixtureResponse::new(StatusCode::BAD_GATEWAY, ""),
            ],
        )
        .await;
        assert!(matches!(
            first,
            Err(IssueError::PartialUpdate {
                applied,
                unapplied,
                ..
            }) if applied == vec!["title"] && unapplied == vec!["status"]
        ));

        let (retry, calls) = run_update(
            IssuePatch {
                status: Some(IssueStatus::Done),
                ..Default::default()
            },
            vec![
                transition_fixture("done"),
                FixtureResponse::new(StatusCode::NO_CONTENT, ""),
                issue_fixture("new title", "done"),
            ],
        )
        .await;
        assert!(
            retry.is_ok(),
            "the caller can retry only the unapplied field"
        );
        assert_eq!(
            calls,
            vec![
                "GET /rest/api/3/issue/PROJ-1/transitions",
                "POST /rest/api/3/issue/PROJ-1/transitions",
                "GET /rest/api/3/issue/PROJ-1",
            ]
        );
    }

    #[tokio::test]
    async fn mismatched_final_category_is_not_reported_as_success() {
        let (result, calls) = run_update(
            IssuePatch {
                status: Some(IssueStatus::InProgress),
                ..Default::default()
            },
            vec![
                transition_fixture("indeterminate"),
                FixtureResponse::new(StatusCode::NO_CONTENT, ""),
                issue_fixture("existing title", "done"),
            ],
        )
        .await;

        match result {
            Err(IssueError::PartialUpdate {
                applied,
                unapplied,
                source,
            }) => {
                assert_eq!(applied, vec!["status"]);
                assert!(unapplied.is_empty());
                assert!(
                    matches!(source.as_ref(), IssueError::Api(message) if message.contains("expected category indeterminate"))
                );
            }
            other => panic!("expected verification error, got {other:?}"),
        }
        assert_eq!(calls.len(), 3, "verification must fetch the issue");
    }

    #[tokio::test]
    async fn title_then_status_success_returns_verified_issue() {
        let (result, calls) = run_update(
            IssuePatch {
                title: Some("new title".into()),
                status: Some(IssueStatus::Done),
                ..Default::default()
            },
            vec![
                FixtureResponse::new(StatusCode::NO_CONTENT, ""),
                transition_fixture("done"),
                FixtureResponse::new(StatusCode::NO_CONTENT, ""),
                issue_fixture("new title", "done"),
            ],
        )
        .await;

        let issue = result.expect("title and verified status should succeed");
        assert_eq!(issue.title, "new title");
        assert_eq!(issue.status, IssueStatus::Done);
        assert_eq!(
            calls,
            vec![
                "PUT /rest/api/3/issue/PROJ-1",
                "GET /rest/api/3/issue/PROJ-1/transitions",
                "POST /rest/api/3/issue/PROJ-1/transitions",
                "GET /rest/api/3/issue/PROJ-1",
            ]
        );
    }

    #[tokio::test]
    async fn create_issue_returns_budget_error_before_jira_followup() {
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
                        json!({"key": "PROJ-2"})
                    } else {
                        json!({})
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
        let backend = JiraBackend::new_with_budget(
            format!("http://{address}"),
            "user@example.test".into(),
            "jira-secret".into(),
            None,
            Arc::clone(&budget),
        );
        let result = backend
            .create_issue(&IssueDraft {
                title: "created through fixture".into(),
                project_id: Some("PROJ".into()),
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
