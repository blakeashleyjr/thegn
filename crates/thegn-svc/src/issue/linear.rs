//! Linear.app GraphQL backend.
//!
//! Auth: `Authorization: <api_key>` (no "Bearer" prefix — Linear's convention).
//! All queries are hand-rolled strings; no graphql_client codegen.

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

const LINEAR_API: &str = "https://api.linear.app";

pub struct LinearBackend {
    http: Option<TrackerHttpClient>,
    http_error: Option<&'static str>,
    team_id: Option<String>,
}

impl LinearBackend {
    pub fn new(api_key: String, team_id: Option<String>) -> Self {
        Self::new_with_budget(api_key, team_id, TrackerHttpBudget::process())
    }

    pub(crate) fn new_with_budget(
        api_key: String,
        team_id: Option<String>,
        budget: std::sync::Arc<TrackerHttpBudget>,
    ) -> Self {
        Self::new_at_origin(api_key, team_id, budget, LINEAR_API)
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(
        api_key: String,
        team_id: Option<String>,
        budget: std::sync::Arc<TrackerHttpBudget>,
        origin: &str,
    ) -> Self {
        Self::new_at_origin(api_key, team_id, budget, origin)
    }

    fn new_at_origin(
        api_key: String,
        team_id: Option<String>,
        budget: std::sync::Arc<TrackerHttpBudget>,
        origin: &str,
    ) -> Self {
        let (http, http_error) =
            match TrackerHttpClient::new("linear", origin, api_key.clone(), budget) {
                Ok(http) => (Some(http), None),
                Err(IssueError::Policy(message)) => (None, Some(message)),
                Err(_) => (None, Some("tracker HTTP client configuration failed")),
            };
        LinearBackend {
            http,
            http_error,
            team_id,
        }
    }

    fn http(&self) -> Result<&TrackerHttpClient, IssueError> {
        self.http.as_ref().ok_or_else(|| {
            IssueError::Policy(self.http_error.unwrap_or("tracker HTTP origin refused"))
        })
    }

    async fn gql<Q: Serialize, R: for<'de> Deserialize<'de>>(
        op: &mut TrackerHttpOperation<'_>,
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
            #[allow(dead_code)]
            message: String,
        }

        let gql: GqlResponse<R> = op
            .json(
                reqwest::Method::POST,
                "/graphql",
                &Body { query, variables },
            )
            .await?;
        if let Some(errs) = gql.errors
            && !errs.is_empty()
        {
            return Err(IssueError::Api("Linear GraphQL operation failed".into()));
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
    /// Linear's `Issue.assignee` is singular and nullable; the domain type
    /// carries a `Vec` because other providers have many (see THE-72).
    #[serde(default)]
    assignee: Option<LinearUser>,
    #[serde(default)]
    labels: Option<LinearLabelList>,
    #[serde(default)]
    branch_name: Option<String>,
    /// Date-only `YYYY-MM-DD` (Linear `dueDate`); absent when unset.
    #[serde(default)]
    due_date: Option<String>,
    url: String,
    updated_at: String,
}

#[derive(Deserialize)]
struct LinearState {
    #[serde(rename = "type")]
    state_type: String,
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

/// Linear's workflow-state `type` values, folded onto the five domain
/// statuses. `triage` merges into `Backlog`: it is Linear's intake queue and
/// the domain has no separate status for it.
fn map_state(state: Option<&LinearState>) -> IssueStatus {
    match state.map(|s| s.state_type.as_str()) {
        Some("triage") | Some("backlog") => IssueStatus::Backlog,
        Some("unstarted") => IssueStatus::Todo,
        Some("started") => IssueStatus::InProgress,
        Some("completed") => IssueStatus::Done,
        Some("canceled") => IssueStatus::Cancelled,
        _ => IssueStatus::Backlog,
    }
}

/// Every Linear state `type` that reads back as `s`. Backlog covers both
/// `backlog` and `triage` so a `--status backlog` filter cannot drop issues the
/// unfiltered list labels Backlog.
fn status_to_state_types(s: IssueStatus) -> &'static [&'static str] {
    match s {
        IssueStatus::Backlog => &["backlog", "triage"],
        IssueStatus::Todo => &["unstarted"],
        IssueStatus::InProgress => &["started"],
        IssueStatus::Done => &["completed"],
        IssueStatus::Cancelled => &["canceled"],
    }
}

/// The single canonical write target for `issueUpdate`'s stateId lookup —
/// Backlog writes to `backlog`, never to the `triage` intake queue.
fn status_to_write_state_type(s: IssueStatus) -> &'static str {
    match s {
        IssueStatus::Backlog => "backlog",
        IssueStatus::Todo => "unstarted",
        IssueStatus::InProgress => "started",
        IssueStatus::Done => "completed",
        IssueStatus::Cancelled => "canceled",
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

/// Escape a string for embedding inside a GraphQL double-quoted literal.
/// A bare `"`, a trailing `\`, or a raw newline would terminate/break the
/// literal (GraphQL string literals cannot contain raw line terminators);
/// backslash must be escaped first so we don't double-escape our own output.
fn escape_graphql_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out
}

fn parse_updated_at(s: &str) -> i64 {
    // RFC3339 → unix ms; fall back to 0 on parse failure.
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(0)
}

fn checked_workflow_state_id(raw: &str) -> Result<String, IssueError> {
    let admitted = super::identity::builtin_segment(raw, "Linear workflow state id")
        .map_err(IssueError::Parse)?;
    // This value is still placed in a GraphQL string literal below. Escape it
    // even after syntax admission so a provider response cannot terminate the
    // literal or graft fields onto the mutation.
    Ok(escape_graphql_str(admitted))
}

fn linear_issue_to_domain(li: LinearIssue) -> Result<Issue, IssueError> {
    super::identity::builtin_identity(&li.identifier).map_err(IssueError::Parse)?;
    super::validate_public_url(&li.url)?;
    let issue = Issue {
        id: format!("linear:{}", li.identifier),
        number: li.identifier.clone(),
        provider: "linear".into(),
        title: li.title,
        body: li.description,
        status: map_state(li.state.as_ref()),
        priority: map_priority(li.priority),
        assignees: li.assignee.map(|u| u.name).into_iter().collect(),
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
        due_at_ms: li.due_date.as_deref().and_then(super::parse_due_date_ms),
        ..Default::default()
    };
    super::validate_issue_identity(&issue)?;
    Ok(issue)
}

// ---- query constants --------------------------------------------------------

const ISSUE_FIELDS: &str = r#"
    id identifier title description
    state { type }
    priority
    assignee { name }
    labels { nodes { name } }
    branchName dueDate url updatedAt
"#;

/// Linear's pagination arguments accept `1..=250`; `first: 0` is rejected.
const MAX_PAGE_SIZE: usize = 250;

/// `limit == 0` means "no cap" to our callers (`cmd/issue.rs` only truncates
/// when `limit > 0`), so it maps to the page maximum, not to 1.
fn page_size(limit: usize) -> usize {
    if limit == 0 {
        MAX_PAGE_SIZE
    } else {
        limit.min(MAX_PAGE_SIZE)
    }
}

// The document builders are free functions with no `self` and no I/O so the
// emitted strings are reachable from unit tests — every THE-72 defect was a
// query string no test could see.

fn build_list_query(filter: &IssueFilter, team_id: Option<&str>) -> String {
    let mut conditions = Vec::new();

    if filter.assignee_me {
        conditions.push(r#"assignee: { isMe: { eq: true } }"#.to_string());
    }

    if !filter.statuses.is_empty() {
        // `WorkflowStateFilter.type` is a StringComparator: `in` is `[String!]`,
        // bare strings. Backlog expands to two types, so de-duplicate.
        let mut types: Vec<&str> = Vec::new();
        for s in &filter.statuses {
            for t in status_to_state_types(*s) {
                if !types.contains(t) {
                    types.push(t);
                }
            }
        }
        let types_str = types
            .iter()
            .map(|t| format!(r#""{t}""#))
            .collect::<Vec<_>>()
            .join(", ");
        conditions.push(format!("state: {{ type: {{ in: [{types_str}] }} }}"));
    }

    if let Some(team_id) = team_id {
        let team_id = escape_graphql_str(team_id);
        conditions.push(format!(r#"team: {{ id: {{ eq: "{team_id}" }} }}"#));
    }

    // Join the arguments rather than splicing a possibly-empty filter block:
    // `issues(, first: 250)` is a GraphQL *parse* error, and an unfiltered list
    // is the default CLI shape.
    let mut args = Vec::new();
    if !conditions.is_empty() {
        args.push(format!("filter: {{ {} }}", conditions.join(", ")));
    }
    args.push(format!("first: {}", page_size(filter.limit)));
    args.push("orderBy: updatedAt".to_string());
    let args = args.join(", ");

    format!(
        r#"query {{ issues({args}) {{
                nodes {{ {ISSUE_FIELDS} }}
            }} }}"#
    )
}

fn build_search_query(query: &str, limit: usize) -> String {
    let q_escaped = escape_graphql_str(query);
    let first = page_size(limit);
    format!(
        r#"query {{
                issues(filter: {{ title: {{ containsIgnoreCase: "{q_escaped}" }} }},
                       first: {first}, orderBy: updatedAt) {{
                    nodes {{ {ISSUE_FIELDS} }}
                }}
            }}"#
    )
}

/// Every user-controlled value is a GraphQL variable, so the mutation document
/// itself is constant apart from the shared selection.
fn build_create_mutation() -> String {
    format!(
        r#"mutation($title: String!, $priority: Int, $description: String, $teamId: String) {{
            issueCreate(input: {{ title: $title, priority: $priority, description: $description, teamId: $teamId }}) {{
                issue {{ {ISSUE_FIELDS} }}
            }}
        }}"#
    )
}

/// The identifier is user-controlled — it arrives from `thegn issue get <id>`,
/// `wt new --from-issue <id>` and the `issues.get` control-API verb — so it is
/// escaped, not spliced raw: an unescaped `"` would terminate the literal and
/// let the caller graft selections onto a document sent with the user's Linear
/// token. A legitimate `ABC-123` is unchanged by the escape.
fn build_get_query(identifier: &str) -> String {
    let identifier = escape_graphql_str(identifier);
    format!(
        r#"query {{ issue(id: "{identifier}") {{
                {ISSUE_FIELDS}
                comments {{ nodes {{ body user {{ name }} createdAt }} }}
            }} }}"#
    )
}

impl IssueBackend for LinearBackend {
    fn provider_id(&self) -> &'static str {
        "linear"
    }

    fn caps(&self) -> super::IssueCaps {
        super::IssueCaps::default()
    }

    fn list_issues<'a>(
        &'a self,
        filter: &'a IssueFilter,
    ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
        Box::pin(async move {
            #[derive(Serialize)]
            struct Vars {}
            let http = self.http()?;
            let mut op = http.operation();
            op.prepare().await?;
            if let Some(team_id) = self.team_id.as_deref() {
                ensure_dynamic_input(team_id)?;
            }
            let query = build_list_query(filter, self.team_id.as_deref());
            let data: IssueNodes = Self::gql(&mut op, &query, Vars {}).await?;
            data.issues
                .nodes
                .into_iter()
                .map(linear_issue_to_domain)
                .collect()
        })
    }

    fn get_issue<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<IssueDetail, IssueError>> {
        Box::pin(async move {
            // id is in "linear:ABC-123" form; the raw Linear id is the identifier.
            let identifier = id.strip_prefix("linear:").unwrap_or(id);
            #[derive(Serialize)]
            struct Vars {}
            let http = self.http()?;
            let mut op = http.operation();
            op.prepare().await?;
            ensure_dynamic_input(identifier)?;
            let query = build_get_query(identifier);
            let data: LinearIssueWithComments = Self::gql(&mut op, &query, Vars {}).await?;
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
                issue: linear_issue_to_domain(li.issue)?,
                comments,
            })
        })
    }

    fn create_issue<'a>(
        &'a self,
        draft: &'a IssueDraft,
    ) -> BoxFuture<'a, Result<Issue, IssueError>> {
        Box::pin(async move {
            // Pass every user-controlled value as a GraphQL variable rather than
            // string-splicing it into the mutation: interpolation breaks on
            // multi-line/quoted/backslash bodies (GraphQL string literals can't hold
            // raw line terminators) and would let a crafted body inject fields.
            let team_id = match (&self.team_id, draft.project_id.as_deref()) {
                (_, Some(pid)) => Some(pid),
                (Some(tid), None) => Some(tid.as_str()),
                (None, None) => None,
            };
            #[derive(Serialize)]
            struct Vars<'a> {
                title: &'a str,
                priority: i64,
                #[serde(skip_serializing_if = "Option::is_none")]
                description: Option<&'a str>,
                #[serde(rename = "teamId", skip_serializing_if = "Option::is_none")]
                team_id: Option<&'a str>,
            }
            // The selection comes from ISSUE_FIELDS — the hand-duplicated copy
            // that used to live here is how `assignees` survived the shared
            // constant (THE-72).
            let vars = Vars {
                title: &draft.title,
                priority: priority_to_int(draft.priority),
                description: draft.body.as_deref(),
                team_id,
            };
            let http = self.http()?;
            let mut op = http.operation();
            op.prepare().await?;
            let query = build_create_mutation();
            let data: IssueCreateData = Self::gql(&mut op, &query, vars).await?;
            data.issue_create
                .issue
                .map(linear_issue_to_domain)
                .transpose()?
                .ok_or_else(|| IssueError::Api("issueCreate returned no issue".into()))
        })
    }

    fn update_issue<'a>(
        &'a self,
        id: &'a str,
        patch: &'a IssuePatch,
    ) -> BoxFuture<'a, Result<Issue, IssueError>> {
        Box::pin(async move {
            // Escaped for the same reason as `build_get_query` — `issues.update`
            // is a control-API verb, so `id` is not necessarily a local user's.
            let http = self.http()?;
            let mut op = http.operation();
            op.prepare().await?;
            ensure_dynamic_input(id.strip_prefix("linear:").unwrap_or(id))?;
            if let Some(title) = patch.title.as_deref() {
                ensure_dynamic_input(title)?;
            }
            let identifier = escape_graphql_str(id.strip_prefix("linear:").unwrap_or(id));
            #[derive(Serialize)]
            struct Vars {}
            let mut fields = Vec::new();
            if let Some(p) = patch.priority {
                fields.push(format!("priority: {}", priority_to_int(p)));
            }
            if let Some(t) = &patch.title {
                fields.push(format!(r#"title: "{}""#, escape_graphql_str(t)));
            }
            // Status update requires knowing the stateId for the target state+team.
            // For simplicity we pass the status type as a string; callers that need
            // the exact stateId should use the raw Linear API directly.
            if let Some(s) = patch.status {
                let type_str = status_to_write_state_type(s);
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
                let states: StatesData = Self::gql(&mut op, &state_query, Vars {}).await?;
                if let Some(state_node) = states.workflow_states.nodes.first() {
                    let state_id = checked_workflow_state_id(&state_node.id)?;
                    fields.push(format!(r#"stateId: "{state_id}""#));
                }
            }

            if fields.is_empty() {
                // Nothing to change — fetch and return the current state.
                let query = build_get_query(id.strip_prefix("linear:").unwrap_or(id));
                let data: LinearIssueWithComments = Self::gql(&mut op, &query, Vars {}).await?;
                return linear_issue_to_domain(data.issue.issue);
            }

            let fields_str = fields.join(", ");
            let query = format!(
                r#"mutation {{
                issueUpdate(id: "{identifier}", input: {{ {fields_str} }}) {{
                    issue {{ {ISSUE_FIELDS} }}
                }}
            }}"#
            );
            let data: IssueUpdateData = Self::gql(&mut op, &query, Vars {}).await?;
            data.issue_update
                .issue
                .map(linear_issue_to_domain)
                .transpose()?
                .ok_or_else(|| IssueError::Api("issueUpdate returned no issue".into()))
        })
    }

    fn search<'a>(
        &'a self,
        query_str: &'a str,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
        Box::pin(async move {
            #[derive(Serialize)]
            struct Vars {}
            let http = self.http()?;
            let mut op = http.operation();
            op.prepare().await?;
            ensure_dynamic_input(query_str)?;
            let query = build_search_query(query_str, limit);
            let data: IssueNodes = Self::gql(&mut op, &query, Vars {}).await?;
            data.issues
                .nodes
                .into_iter()
                .map(linear_issue_to_domain)
                .collect()
        })
    }
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

    fn state(ty: &str) -> Option<LinearState> {
        serde_json::from_value(json!({ "type": ty })).unwrap()
    }

    #[test]
    fn map_state_covers_all_types_and_fallback() {
        assert_eq!(map_state(state("triage").as_ref()), IssueStatus::Backlog);
        assert_eq!(map_state(state("backlog").as_ref()), IssueStatus::Backlog);
        assert_eq!(map_state(state("unstarted").as_ref()), IssueStatus::Todo);
        assert_eq!(
            map_state(state("started").as_ref()),
            IssueStatus::InProgress
        );
        assert_eq!(map_state(state("completed").as_ref()), IssueStatus::Done);
        // Linear spells it with one `l`; "cancelled" is not a Linear type.
        assert_eq!(
            map_state(state("canceled").as_ref()),
            IssueStatus::Cancelled
        );
        assert_eq!(map_state(state("weird").as_ref()), IssueStatus::Backlog);
        assert_eq!(map_state(None), IssueStatus::Backlog);
    }

    #[test]
    fn priority_maps_and_round_trips() {
        // int → domain
        assert_eq!(map_priority(1), IssuePriority::Urgent);
        assert_eq!(map_priority(2), IssuePriority::High);
        assert_eq!(map_priority(3), IssuePriority::Medium);
        assert_eq!(map_priority(4), IssuePriority::Low);
        assert_eq!(map_priority(0), IssuePriority::None);
        assert_eq!(map_priority(99), IssuePriority::None);
        // Every named priority survives a domain→int→domain round-trip.
        for p in [
            IssuePriority::Urgent,
            IssuePriority::High,
            IssuePriority::Medium,
            IssuePriority::Low,
            IssuePriority::None,
        ] {
            assert_eq!(map_priority(priority_to_int(p)), p, "round-trip {p:?}");
        }
    }

    #[test]
    fn parse_updated_at_valid_and_invalid() {
        assert_eq!(parse_updated_at("1970-01-01T00:00:03Z"), 3000);
        assert_eq!(parse_updated_at(""), 0);
        assert_eq!(parse_updated_at("garbage"), 0);
    }

    #[test]
    fn escape_graphql_str_neutralizes_quote_backslash_newline_tab() {
        // A bare quote must be escaped so it can't terminate the literal.
        assert_eq!(escape_graphql_str(r#"a "b" c"#), r#"a \"b\" c"#);
        // A trailing backslash must double, not escape our closing quote.
        assert_eq!(escape_graphql_str(r"path\"), r"path\\");
        // Raw line terminators (illegal in a GraphQL literal) become escapes.
        assert_eq!(
            escape_graphql_str("line1\nline2\r\tx"),
            "line1\\nline2\\r\\tx"
        );
        // Backslash is handled before quote so we don't double-escape output.
        assert_eq!(escape_graphql_str(r#"\""#), r#"\\\""#);
        assert_eq!(escape_graphql_str("plain"), "plain");
    }

    #[test]
    fn workflow_state_id_is_admitted_and_escaped_before_mutation() {
        assert_eq!(
            checked_workflow_state_id(r##"state";title:"injected""##).unwrap(),
            r##"state\";title:\"injected\""##
        );
        assert!(checked_workflow_state_id("state/child").is_err());
        assert!(checked_workflow_state_id("state\nchild").is_err());
        assert!(checked_workflow_state_id(&"x".repeat(129)).is_err());
    }

    #[test]
    fn issue_to_domain_maps_all_fields() {
        let li: LinearIssue = serde_json::from_value(json!({
            "id": "uuid-1",
            "identifier": "ABC-123",
            "title": "Ship it",
            "description": "the body",
            "state": { "type": "started" },
            "priority": 2,
            "assignee": { "name": "Fox Mulder" },
            "labels": { "nodes": [{ "name": "feature" }] },
            "branchName": "abc-123-ship-it",
            "url": "https://linear.app/x/issue/ABC-123",
            "updatedAt": "1970-01-01T00:00:04Z"
        }))
        .unwrap();
        let issue = linear_issue_to_domain(li).unwrap();
        assert_eq!(issue.id, "linear:ABC-123");
        assert_eq!(issue.number, "ABC-123");
        assert_eq!(issue.provider, "linear");
        assert_eq!(issue.title, "Ship it");
        assert_eq!(issue.body.as_deref(), Some("the body"));
        assert_eq!(issue.status, IssueStatus::InProgress);
        assert_eq!(issue.priority, IssuePriority::High);
        assert_eq!(issue.assignees, vec!["Fox Mulder".to_string()]);
        assert_eq!(issue.labels, vec!["feature".to_string()]);
        assert_eq!(issue.branch_hint.as_deref(), Some("abc-123-ship-it"));
        assert_eq!(issue.updated_at_ms, 4000);
    }

    #[test]
    fn issue_to_domain_tolerates_missing_optionals() {
        let li: LinearIssue = serde_json::from_value(json!({
            "id": "uuid-2",
            "identifier": "ABC-9",
            "title": "Bare",
            "url": "https://linear.app/x/issue/ABC-9",
            "updatedAt": "not-a-date"
        }))
        .unwrap();
        let issue = linear_issue_to_domain(li).unwrap();
        assert_eq!(issue.body, None);
        assert_eq!(issue.status, IssueStatus::Backlog, "no state ⇒ backlog");
        assert_eq!(issue.priority, IssuePriority::None, "default priority 0");
        assert!(issue.assignees.is_empty());
        assert!(issue.labels.is_empty());
        assert_eq!(issue.branch_hint, None);
        assert_eq!(issue.updated_at_ms, 0);
    }

    #[test]
    fn issue_to_domain_rejects_provider_identity_or_url_before_returning() {
        let mut li: LinearIssue = serde_json::from_value(json!({
            "id": "uuid-3",
            "identifier": "ABC-9",
            "title": "Bounded",
            "url": "https://linear.app/x/issue/ABC-9",
            "updatedAt": "not-a-date"
        }))
        .unwrap();
        li.identifier = "ABC/9".into();
        assert!(linear_issue_to_domain(li).is_err());

        let mut li: LinearIssue = serde_json::from_value(json!({
            "id": "uuid-4",
            "identifier": "ABC-9",
            "title": "Bounded",
            "url": "https://linear.app/x/issue/ABC-9",
            "updatedAt": "not-a-date"
        }))
        .unwrap();
        li.url = "file:///tmp/issue".into();
        assert!(linear_issue_to_domain(li).is_err());
    }

    #[test]
    fn constructor_admits_official_linear_endpoint() {
        let backend = LinearBackend::new_with_budget(
            "linear-test-key".into(),
            None,
            std::sync::Arc::new(TrackerHttpBudget::with_permits(1)),
        );
        assert!(backend.http().is_ok());
    }

    #[tokio::test]
    async fn update_status_escapes_provider_state_id_before_write() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let queries = Arc::new(Mutex::new(Vec::new()));
        let route_calls = Arc::clone(&calls);
        let route_queries = Arc::clone(&queries);
        let server = tokio::spawn(async move {
            let app = Router::new().fallback(any(move |request: Request| {
                let route_calls = Arc::clone(&route_calls);
                let route_queries = Arc::clone(&route_queries);
                async move {
                    let bytes = axum::body::to_bytes(request.into_body(), 64 * 1024)
                        .await
                        .unwrap();
                    let payload: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    let call = route_calls.fetch_add(1, Ordering::SeqCst);
                    if call == 1 {
                        route_queries
                            .lock()
                            .unwrap()
                            .push(payload["query"].as_str().unwrap().to_owned());
                    }
                    let body = if call == 0 {
                        json!({
                            "data": {
                                "workflowStates": {
                                    "nodes": [{"id": "state\";title:\"injected"}]
                                }
                            }
                        })
                    } else {
                        json!({"data": {"issueUpdate": {"issue": null}}})
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
        let backend = LinearBackend::new_for_test(
            "linear-secret".into(),
            Some("team-1".into()),
            budget,
            &format!("http://{address}"),
        );
        let result = backend
            .update_issue(
                "linear:ABC-1",
                &IssuePatch {
                    status: Some(IssueStatus::Done),
                    ..Default::default()
                },
            )
            .await;
        assert!(matches!(result, Err(IssueError::Api(_))));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let query = queries.lock().unwrap().first().cloned().unwrap();
        assert!(query.contains(r##"stateId: "state\";title:\"injected""##));
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn update_status_uses_one_budget_across_linear_requests() {
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
                        json!({"data": {"workflowStates": {"nodes": [{"id": "state-1"}]}}})
                    } else {
                        json!({"data": {"issueUpdate": {"issue": null}}})
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
        // The first helper request is admitted; the second helper request is
        // refused by the same operation before the fake server can see it.
        budget.expire_after_responses_for_test(1);
        let backend = LinearBackend::new_for_test(
            "linear-secret".into(),
            Some("team-1".into()),
            Arc::clone(&budget),
            &format!("http://{address}"),
        );
        let result = backend
            .update_issue(
                "linear:ABC-1",
                &IssuePatch {
                    status: Some(IssueStatus::Done),
                    ..Default::default()
                },
            )
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

#[cfg(test)]
#[path = "linear_schema_tests.rs"]
mod schema_contract;
