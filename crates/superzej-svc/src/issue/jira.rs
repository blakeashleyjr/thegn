//! Jira Cloud/Server REST v3 backend.
//!
//! Auth: `Authorization: Basic base64(email:api_token)`.
//! All requests target `/rest/api/3/…` endpoints.

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use superzej_core::issue::{
    Issue, IssueComment, IssueDetail, IssueDraft, IssueFilter, IssuePatch, IssuePriority,
    IssueStatus,
};

use super::{IssueBackend, IssueError};

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
            client: Client::new(),
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
        ..Default::default()
    }
}

const JIRA_FIELDS: &str = "summary,description,status,priority,assignee,labels,updated,comment";

#[allow(async_fn_in_trait)]
impl IssueBackend for JiraBackend {
    fn provider_id(&self) -> &'static str {
        "jira"
    }

    async fn list_issues(&self, filter: &IssueFilter) -> Result<Vec<Issue>, IssueError> {
        let mut jql_parts = Vec::new();

        if filter.assignee_me {
            jql_parts.push("assignee = currentUser()".to_string());
        }

        if let Some(proj) = &self.project_key {
            jql_parts.push(format!("project = \"{proj}\""));
        }

        if !filter.statuses.is_empty() {
            let cats: Vec<&str> = filter
                .statuses
                .iter()
                .map(|s| match s {
                    IssueStatus::Backlog => "\"To Do\"",
                    IssueStatus::Todo => "\"To Do\"",
                    IssueStatus::InProgress => "\"In Progress\"",
                    IssueStatus::Done => "\"Done\"",
                    IssueStatus::Cancelled => "\"Done\"",
                })
                .collect();
            let cats_deduped: Vec<&&str> = cats
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            jql_parts.push(format!(
                "status in ({})",
                cats_deduped
                    .iter()
                    .map(|s| **s)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        } else {
            // Default: active issues only.
            jql_parts.push(r#"statusCategory in ("To Do", "In Progress")"#.to_string());
        }

        if let Some(q) = &filter.query {
            jql_parts.push(format!("text ~ \"{q}\""));
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
    }

    async fn get_issue(&self, id: &str) -> Result<IssueDetail, IssueError> {
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
    }

    async fn create_issue(&self, draft: &IssueDraft) -> Result<Issue, IssueError> {
        let project_key = self
            .project_key
            .as_deref()
            .or(draft.project_id.as_deref())
            .ok_or_else(|| IssueError::Api("Jira create requires a project key in config".into()))?
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
    }

    async fn update_issue(&self, id: &str, patch: &IssuePatch) -> Result<Issue, IssueError> {
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
    }

    async fn search(&self, query_str: &str, limit: usize) -> Result<Vec<Issue>, IssueError> {
        let q_escaped = urlencoding_simple(query_str);
        let limit = limit.min(100);
        let path = format!(
            "search?jql=text+~+\"{q_escaped}\"+ORDER+BY+updated+DESC&fields={JIRA_FIELDS}&maxResults={limit}"
        );
        let result: SearchResult = self.get(&path).await?;
        Ok(result
            .issues
            .into_iter()
            .map(jira_issue_to_domain)
            .collect())
    }
}

/// Minimal percent-encoding for JQL query strings (no external dep needed).
fn urlencoding_simple(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' | '+' | '=' | ' ' => {
                if c == ' ' {
                    out.push('+');
                } else {
                    out.push(c);
                }
            }
            _ => {
                for byte in c.to_string().as_bytes() {
                    out.push_str(&format!("%{byte:02X}"));
                }
            }
        }
    }
    out
}
