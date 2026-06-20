//! Linear.app GraphQL backend.
//!
//! Auth: `Authorization: <api_key>` (no "Bearer" prefix — Linear's convention).
//! All queries are hand-rolled strings; no graphql_client codegen.

use reqwest::Client;
use serde::{Deserialize, Serialize};
use superzej_core::issue::{
    Issue, IssueComment, IssueDetail, IssueDraft, IssueFilter, IssuePatch, IssuePriority,
    IssueStatus,
};

use super::{IssueBackend, IssueError};

const LINEAR_API: &str = "https://api.linear.app/graphql";

pub struct LinearBackend {
    client: Client,
    api_key: String,
    team_id: Option<String>,
}

impl LinearBackend {
    pub fn new(api_key: String, team_id: Option<String>) -> Self {
        LinearBackend {
            client: Client::new(),
            api_key,
            team_id,
        }
    }

    async fn gql<Q: Serialize, R: for<'de> Deserialize<'de>>(
        &self,
        query: &str,
        variables: Q,
    ) -> Result<R, IssueError> {
        #[derive(Serialize)]
        struct Body<'a, V> {
            query: &'a str,
            variables: V,
        }
        #[derive(Deserialize)]
        struct GqlResponse<D> {
            data: Option<D>,
            errors: Option<Vec<GqlError>>,
        }
        #[derive(Deserialize)]
        struct GqlError {
            message: String,
        }

        let resp = self
            .client
            .post(LINEAR_API)
            .header("Authorization", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&Body { query, variables })
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(IssueError::Auth(format!(
                "HTTP {} from Linear API",
                resp.status()
            )));
        }
        let gql: GqlResponse<R> = resp.json().await?;
        if let Some(errs) = gql.errors
            && !errs.is_empty()
        {
            return Err(IssueError::Api(errs[0].message.clone()));
        }
        gql.data
            .ok_or_else(|| IssueError::Parse("no data in response".into()))
    }
}

// ---- GraphQL response shapes ------------------------------------------------

#[derive(Deserialize)]
struct IssueNodes {
    issues: IssueConnection,
}

#[derive(Deserialize)]
struct IssueConnection {
    nodes: Vec<LinearIssue>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LinearIssue {
    #[allow(dead_code)]
    id: String,
    identifier: String,
    title: String,
    description: Option<String>,
    #[serde(default)]
    state: Option<LinearState>,
    #[serde(default)]
    priority: i64,
    #[serde(default)]
    assignees: Option<LinearUserList>,
    #[serde(default)]
    labels: Option<LinearLabelList>,
    #[serde(default)]
    branch_name: Option<String>,
    url: String,
    updated_at: String,
}

#[derive(Deserialize)]
struct LinearState {
    #[serde(rename = "type")]
    state_type: String,
}

#[derive(Deserialize, Default)]
struct LinearUserList {
    nodes: Vec<LinearUser>,
}

#[derive(Deserialize)]
struct LinearUser {
    name: String,
}

#[derive(Deserialize, Default)]
struct LinearLabelList {
    nodes: Vec<LinearLabel>,
}

#[derive(Deserialize)]
struct LinearLabel {
    name: String,
}

#[derive(Deserialize)]
struct LinearIssueWithComments {
    issue: LinearIssueDetail,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LinearIssueDetail {
    #[serde(flatten)]
    issue: LinearIssue,
    comments: Option<LinearCommentList>,
}

#[derive(Deserialize, Default)]
struct LinearCommentList {
    nodes: Vec<LinearComment>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LinearComment {
    body: String,
    user: Option<LinearUser>,
    created_at: String,
}

#[derive(Deserialize)]
struct IssueCreateData {
    #[serde(rename = "issueCreate")]
    issue_create: IssueCreatePayload,
}

#[derive(Deserialize)]
struct IssueCreatePayload {
    issue: Option<LinearIssue>,
}

#[derive(Deserialize)]
struct IssueUpdateData {
    #[serde(rename = "issueUpdate")]
    issue_update: IssueUpdatePayload,
}

#[derive(Deserialize)]
struct IssueUpdatePayload {
    issue: Option<LinearIssue>,
}

// ---- domain type conversion -------------------------------------------------

fn map_state(state: Option<&LinearState>) -> IssueStatus {
    match state.map(|s| s.state_type.as_str()) {
        Some("triage") => IssueStatus::Backlog,
        Some("unstarted") => IssueStatus::Todo,
        Some("started") => IssueStatus::InProgress,
        Some("completed") => IssueStatus::Done,
        Some("cancelled") => IssueStatus::Cancelled,
        _ => IssueStatus::Backlog,
    }
}

fn map_priority(p: i64) -> IssuePriority {
    match p {
        1 => IssuePriority::Urgent,
        2 => IssuePriority::High,
        3 => IssuePriority::Medium,
        4 => IssuePriority::Low,
        _ => IssuePriority::None,
    }
}

fn priority_to_int(p: IssuePriority) -> i64 {
    match p {
        IssuePriority::Urgent => 1,
        IssuePriority::High => 2,
        IssuePriority::Medium => 3,
        IssuePriority::Low => 4,
        IssuePriority::None => 0,
    }
}

fn parse_updated_at(s: &str) -> i64 {
    // RFC3339 → unix ms; fall back to 0 on parse failure.
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(0)
}

fn linear_issue_to_domain(li: LinearIssue) -> Issue {
    Issue {
        id: format!("linear:{}", li.identifier),
        number: li.identifier.clone(),
        provider: "linear".into(),
        title: li.title,
        body: li.description,
        status: map_state(li.state.as_ref()),
        priority: map_priority(li.priority),
        assignees: li
            .assignees
            .unwrap_or_default()
            .nodes
            .into_iter()
            .map(|u| u.name)
            .collect(),
        labels: li
            .labels
            .unwrap_or_default()
            .nodes
            .into_iter()
            .map(|l| l.name)
            .collect(),
        url: li.url,
        branch_hint: li.branch_name,
        updated_at_ms: parse_updated_at(&li.updated_at),
        ..Default::default()
    }
}

// ---- query constants --------------------------------------------------------

const ISSUE_FIELDS: &str = r#"
    id identifier title description
    state { type }
    priority
    assignees { nodes { name } }
    labels { nodes { name } }
    branchName url updatedAt
"#;

#[allow(async_fn_in_trait)]
impl IssueBackend for LinearBackend {
    fn provider_id(&self) -> &'static str {
        "linear"
    }

    async fn list_issues(&self, filter: &IssueFilter) -> Result<Vec<Issue>, IssueError> {
        let mut conditions = Vec::new();

        if filter.assignee_me {
            conditions.push(r#"assignee: { isMe: { eq: true } }"#.to_string());
        }

        if !filter.statuses.is_empty() {
            let types: Vec<&str> = filter
                .statuses
                .iter()
                .map(|s| match s {
                    IssueStatus::Backlog => "triage",
                    IssueStatus::Todo => "unstarted",
                    IssueStatus::InProgress => "started",
                    IssueStatus::Done => "completed",
                    IssueStatus::Cancelled => "cancelled",
                })
                .collect();
            let types_str = types
                .iter()
                .map(|t| format!(r#"{{ eq: "{t}" }}"#))
                .collect::<Vec<_>>()
                .join(", ");
            conditions.push(format!("state: {{ type: {{ in: [{types_str}] }} }}"));
        }

        if let Some(team_id) = &self.team_id {
            conditions.push(format!(r#"team: {{ id: {{ eq: "{team_id}" }} }}"#));
        }

        let filter_block = if conditions.is_empty() {
            String::new()
        } else {
            format!("filter: {{ {} }}", conditions.join(", "))
        };

        let limit = filter.limit.min(250);
        let query = format!(
            r#"query {{ issues({filter_block}, first: {limit}, orderBy: updatedAt) {{
                nodes {{ {ISSUE_FIELDS} }}
            }} }}"#
        );

        #[derive(Serialize)]
        struct Vars {}
        let data: IssueNodes = self.gql(&query, Vars {}).await?;
        Ok(data
            .issues
            .nodes
            .into_iter()
            .map(linear_issue_to_domain)
            .collect())
    }

    async fn get_issue(&self, id: &str) -> Result<IssueDetail, IssueError> {
        // id is in "linear:ABC-123" form; the raw Linear id is the identifier.
        let identifier = id.strip_prefix("linear:").unwrap_or(id);
        let query = format!(
            r#"query {{ issue(id: "{identifier}") {{
                {ISSUE_FIELDS}
                comments {{ nodes {{ body user {{ name }} createdAt }} }}
            }} }}"#
        );
        #[derive(Serialize)]
        struct Vars {}
        let data: LinearIssueWithComments = self.gql(&query, Vars {}).await?;
        let li = data.issue;
        let comments = li
            .comments
            .unwrap_or_default()
            .nodes
            .into_iter()
            .map(|c| IssueComment {
                author: c.user.map(|u| u.name).unwrap_or_else(|| "unknown".into()),
                body: c.body,
                created_at_ms: parse_updated_at(&c.created_at),
            })
            .collect();
        Ok(IssueDetail {
            issue: linear_issue_to_domain(li.issue),
            comments,
        })
    }

    async fn create_issue(&self, draft: &IssueDraft) -> Result<Issue, IssueError> {
        let priority = priority_to_int(draft.priority);
        let team_part = match (&self.team_id, &draft.project_id) {
            (_, Some(pid)) => format!(r#"teamId: "{pid}""#),
            (Some(tid), None) => format!(r#"teamId: "{tid}""#),
            (None, None) => String::new(),
        };
        let body_part = draft
            .body
            .as_deref()
            .map(|b| format!(r#", description: "{b}""#))
            .unwrap_or_default();
        let query = format!(
            r#"mutation {{
                issueCreate(input: {{ title: "{}", priority: {priority}{body_part}, {team_part} }}) {{
                    issue {{ {ISSUE_FIELDS} }}
                }}
            }}"#,
            draft.title.replace('"', "\\\"")
        );
        #[derive(Serialize)]
        struct Vars {}
        let data: IssueCreateData = self.gql(&query, Vars {}).await?;
        data.issue_create
            .issue
            .map(linear_issue_to_domain)
            .ok_or_else(|| IssueError::Api("issueCreate returned no issue".into()))
    }

    async fn update_issue(&self, id: &str, patch: &IssuePatch) -> Result<Issue, IssueError> {
        let identifier = id.strip_prefix("linear:").unwrap_or(id);
        let mut fields = Vec::new();
        if let Some(p) = patch.priority {
            fields.push(format!("priority: {}", priority_to_int(p)));
        }
        if let Some(t) = &patch.title {
            fields.push(format!(r#"title: "{}""#, t.replace('"', "\\\"")));
        }
        // Status update requires knowing the stateId for the target state+team.
        // For simplicity we pass the status type as a string; callers that need
        // the exact stateId should use the raw Linear API directly.
        if let Some(s) = patch.status {
            let type_str = match s {
                IssueStatus::Backlog => "triage",
                IssueStatus::Todo => "unstarted",
                IssueStatus::InProgress => "started",
                IssueStatus::Done => "completed",
                IssueStatus::Cancelled => "cancelled",
            };
            // We query for the first state of the correct type in the issue's team.
            // This is a best-effort approach; a full implementation would cache
            // the state list per team and resolve the exact stateId.
            let state_query = format!(
                r#"query {{ workflowStates(filter: {{ type: {{ eq: "{type_str}" }} }}, first: 1) {{
                    nodes {{ id }}
                }} }}"#
            );
            #[derive(Deserialize)]
            struct StatesData {
                #[serde(rename = "workflowStates")]
                workflow_states: StatesConnection,
            }
            #[derive(Deserialize)]
            struct StatesConnection {
                nodes: Vec<StateNode>,
            }
            #[derive(Deserialize)]
            struct StateNode {
                id: String,
            }
            #[derive(Serialize)]
            struct Vars {}
            let states: StatesData = self.gql(&state_query, Vars {}).await?;
            if let Some(state_node) = states.workflow_states.nodes.first() {
                fields.push(format!(r#"stateId: "{}""#, state_node.id));
            }
        }

        if fields.is_empty() {
            // Nothing to change — fetch and return the current state.
            return self.get_issue(id).await.map(|d| d.issue);
        }

        let fields_str = fields.join(", ");
        let query = format!(
            r#"mutation {{
                issueUpdate(id: "{identifier}", input: {{ {fields_str} }}) {{
                    issue {{ {ISSUE_FIELDS} }}
                }}
            }}"#
        );
        #[derive(Serialize)]
        struct Vars {}
        let data: IssueUpdateData = self.gql(&query, Vars {}).await?;
        data.issue_update
            .issue
            .map(linear_issue_to_domain)
            .ok_or_else(|| IssueError::Api("issueUpdate returned no issue".into()))
    }

    async fn search(&self, query_str: &str, limit: usize) -> Result<Vec<Issue>, IssueError> {
        let q_escaped = query_str.replace('"', "\\\"");
        let limit = limit.min(250);
        let query = format!(
            r#"query {{
                issues(filter: {{ title: {{ containsIgnoreCase: "{q_escaped}" }} }},
                       first: {limit}, orderBy: updatedAt) {{
                    nodes {{ {ISSUE_FIELDS} }}
                }}
            }}"#
        );
        #[derive(Serialize)]
        struct Vars {}
        let data: IssueNodes = self.gql(&query, Vars {}).await?;
        Ok(data
            .issues
            .nodes
            .into_iter()
            .map(linear_issue_to_domain)
            .collect())
    }
}
