//! Jira Cloud/Server REST v3 backend.
//!
//! Auth: `Authorization: Basic base64(email:api_token)`.
//! All requests target `/rest/api/3/…` endpoints.

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use thegn_core::issue::{
    Issue, IssueComment, IssueDetail, IssueDraft, IssueFilter, IssuePatch, IssuePriority,
    IssueStatus,
};

use super::{IssueBackend, IssueError};
use futures_util::future::BoxFuture;

pub struct JiraBackend {
    client: Client,
    base_url: String,
    auth: String,
    project_key: Option<String>,
}

impl JiraBackend {
    pub fn new(
        base_url: String,
        email: String,
        api_token: String,
        project_key: Option<String>,
    ) -> Self {
        let creds = format!("{email}:{api_token}");
        let auth = format!("Basic {}", B64.encode(creds.as_bytes()));
        JiraBackend {
            // Bounded timeouts: a stalled tracker must not pin a background
            // permit forever (mirrors gh.rs's OCTOCRAB_REQUEST_TIMEOUT). Falls
            // back to the default client if the builder somehow fails.
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .connect_timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap_or_default(),
            base_url: base_url.trim_end_matches('/').to_string(),
            auth,
            project_key,
        }
    }

    fn url(&self, path: &str) -> String {
        format!(
            "{}/rest/api/3/{}",
            self.base_url,
            path.trim_start_matches('/')
        )
    }

    async fn get<R: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<R, IssueError> {
        let resp = self
            .client
            .get(self.url(path))
            .header("Authorization", &self.auth)
            .header("Accept", "application/json")
            .send()
            .await?;
        if resp.status() == 401 || resp.status() == 403 {
            return Err(IssueError::Auth(format!("Jira HTTP {}", resp.status())));
        }
        if !resp.status().is_success() {
            return Err(IssueError::Api(format!("Jira HTTP {}", resp.status())));
        }
        resp.json()
            .await
            .map_err(|e| IssueError::Parse(e.to_string()))
    }

    async fn post<B: Serialize, R: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<R, IssueError> {
        let resp = self
            .client
            .post(self.url(path))
            .header("Authorization", &self.auth)
            .header("Content-Type", "application/json")
            .json(body)
            .send()
            .await?;
        if resp.status() == 401 || resp.status() == 403 {
            return Err(IssueError::Auth(format!("Jira HTTP {}", resp.status())));
        }
        if !resp.status().is_success() {
            let txt = resp.text().await.unwrap_or_default();
            return Err(IssueError::Api(format!("Jira POST {}: {txt}", path)));
        }
        resp.json()
            .await
            .map_err(|e| IssueError::Parse(e.to_string()))
    }

    async fn put<B: Serialize>(&self, path: &str, body: &B) -> Result<(), IssueError> {
        let resp = self
            .client
            .put(self.url(path))
            .header("Authorization", &self.auth)
            .header("Content-Type", "application/json")
            .json(body)
            .send()
            .await?;
        if resp.status() == 401 || resp.status() == 403 {
            return Err(IssueError::Auth(format!("Jira HTTP {}", resp.status())));
        }
        if !resp.status().is_success() {
            let txt = resp.text().await.unwrap_or_default();
            return Err(IssueError::Api(format!("Jira PUT {}: {txt}", path)));
        }
        Ok(())
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
    self_url: String,
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

fn jira_issue_to_domain(ji: JiraIssue) -> Issue {
    let body = ji
        .fields
        .description
        .as_ref()
        .map(extract_text)
        .filter(|s| !s.is_empty());
    // Derive a browse URL from the self URL.
    let url = {
        // self URL: https://myorg.atlassian.net/rest/api/3/issue/10001
        // browse URL: https://myorg.atlassian.net/browse/KEY-1
        let base = ji
            .self_url
            .split("/rest/api")
            .next()
            .unwrap_or("")
            .trim_end_matches('/');
        format!("{base}/browse/{}", ji.key)
    };
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

const JIRA_FIELDS: &str =
    "summary,description,status,priority,assignee,labels,updated,duedate,comment";

impl IssueBackend for JiraBackend {
    fn provider_id(&self) -> &'static str {
        "jira"
    }

    fn list_issues<'a>(
        &'a self,
        filter: &'a IssueFilter,
    ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
        Box::pin(async move {
            let mut jql_parts = Vec::new();

            if filter.assignee_me {
                jql_parts.push("assignee = currentUser()".to_string());
            }

            if let Some(proj) = &self.project_key {
                jql_parts.push(format!("project = \"{proj}\""));
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
            let result: SearchResult = self.get(&path).await?;
            Ok(result
                .issues
                .into_iter()
                .map(jira_issue_to_domain)
                .collect())
        })
    }

    fn get_issue<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<IssueDetail, IssueError>> {
        Box::pin(async move {
            let key = id.strip_prefix("jira:").unwrap_or(id);
            let ji: JiraIssue = self
                .get(&format!("issue/{key}?fields={JIRA_FIELDS}"))
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
            Ok(IssueDetail {
                issue: jira_issue_to_domain(ji),
                comments,
            })
        })
    }

    fn create_issue<'a>(
        &'a self,
        draft: &'a IssueDraft,
    ) -> BoxFuture<'a, Result<Issue, IssueError>> {
        Box::pin(async move {
            let project_key = self
                .project_key
                .as_deref()
                .or(draft.project_id.as_deref())
                .ok_or_else(|| {
                    IssueError::Api("Jira create requires a project key in config".into())
                })?
                .to_string();

            let priority_name = match draft.priority {
                IssuePriority::Urgent => "Highest",
                IssuePriority::High => "High",
                IssuePriority::Medium => "Medium",
                IssuePriority::Low => "Low",
                IssuePriority::None => "Medium",
            };

            #[derive(Serialize)]
            struct CreateBody {
                fields: CreateFields,
            }
            #[derive(Serialize)]
            struct CreateFields {
                project: ProjectKey,
                summary: String,
                description: Option<serde_json::Value>,
                issuetype: IssueType,
                priority: PriorityName,
            }
            #[derive(Serialize)]
            struct ProjectKey {
                key: String,
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
                    summary: draft.title.clone(),
                    description: draft.body.as_ref().map(|b| {
                        serde_json::json!({
                            "type": "doc",
                            "version": 1,
                            "content": [{
                                "type": "paragraph",
                                "content": [{ "type": "text", "text": b }]
                            }]
                        })
                    }),
                    issuetype: IssueType { name: "Task" },
                    priority: PriorityName {
                        name: priority_name,
                    },
                },
            };

            let created: CreateResponse = self.post("issue", &body).await?;
            let ji: JiraIssue = self
                .get(&format!("issue/{}?fields={JIRA_FIELDS}", created.key))
                .await?;
            Ok(jira_issue_to_domain(ji))
        })
    }

    fn update_issue<'a>(
        &'a self,
        id: &'a str,
        patch: &'a IssuePatch,
    ) -> BoxFuture<'a, Result<Issue, IssueError>> {
        Box::pin(async move {
            let key = id.strip_prefix("jira:").unwrap_or(id);

            // Status update via transitions.
            if let Some(status) = patch.status {
                let transitions: JiraTransitions =
                    self.get(&format!("issue/{key}/transitions")).await?;
                let target_cat = match status {
                    IssueStatus::Backlog | IssueStatus::Todo => "new",
                    IssueStatus::InProgress => "indeterminate",
                    IssueStatus::Done | IssueStatus::Cancelled => "done",
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
                            "no transition to '{target_cat}' state available for {key}"
                        ))
                    })?;

                #[derive(Serialize)]
                struct TransitionBody {
                    transition: TransitionId,
                }
                #[derive(Serialize)]
                struct TransitionId {
                    id: String,
                }
                let _: serde_json::Value = self
                    .post(
                        &format!("issue/{key}/transitions"),
                        &TransitionBody {
                            transition: TransitionId {
                                id: trans.id.clone(),
                            },
                        },
                    )
                    .await
                    .unwrap_or(serde_json::Value::Null);
            }

            // Title / summary update.
            if let Some(title) = &patch.title {
                #[derive(Serialize)]
                struct UpdateBody {
                    fields: UpdateFields,
                }
                #[derive(Serialize)]
                struct UpdateFields {
                    summary: String,
                }
                self.put(
                    &format!("issue/{key}"),
                    &UpdateBody {
                        fields: UpdateFields {
                            summary: title.clone(),
                        },
                    },
                )
                .await?;
            }

            let ji: JiraIssue = self
                .get(&format!("issue/{key}?fields={JIRA_FIELDS}"))
                .await?;
            Ok(jira_issue_to_domain(ji))
        })
    }

    fn search<'a>(
        &'a self,
        query_str: &'a str,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
        Box::pin(async move {
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
            let result: SearchResult = self.get(&path).await?;
            Ok(result
                .issues
                .into_iter()
                .map(jira_issue_to_domain)
                .collect())
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
    use serde_json::json;

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
        let issue = jira_issue_to_domain(ji);
        assert_eq!(issue.id, "jira:PROJ-7");
        assert_eq!(issue.number, "PROJ-7");
        assert_eq!(issue.provider, "jira");
        assert_eq!(issue.title, "Fix the thing");
        assert_eq!(issue.body.as_deref(), Some("details here"));
        assert_eq!(issue.status, IssueStatus::InProgress);
        assert_eq!(issue.priority, IssuePriority::High);
        assert_eq!(issue.assignees, vec!["Dana Scully".to_string()]);
        assert_eq!(issue.labels, vec!["bug".to_string(), "p1".to_string()]);
        assert_eq!(issue.url, "https://myorg.atlassian.net/browse/PROJ-7");
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
        let issue = jira_issue_to_domain(ji);
        assert_eq!(issue.title, "");
        assert_eq!(issue.body, None, "empty description filtered to None");
        assert_eq!(issue.status, IssueStatus::Backlog);
        assert_eq!(issue.priority, IssuePriority::None);
        assert!(issue.assignees.is_empty());
        assert_eq!(issue.updated_at_ms, 0);
        assert_eq!(issue.url, "https://h.example/browse/X-1");
    }
}
