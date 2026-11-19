//! The Todoist inbox backend (b163, THE-2215).
//!
//! Endpoint (Todoist unified API v1; REST v2 is deprecated):
//! `GET {base_url}/api/v1/tasks/filter?query=<filter>&limit=<n>[&cursor=<c>]`
//! with `Authorization: Bearer <token>`. The reply is
//! `{"results":[{"id","content","priority"(1..4, 4 = p1),"labels":[..],
//! "due":{"date":..}|null, ..}],"next_cursor":<string|null>}`. This shape was
//! written from the published docs and NOT verified live (unverified until
//! live step I-2).
//!
//! Transport policy mirrors `issue/http.rs`: 20 s operation deadline, 5 s
//! connect, 1 MiB body cap, no redirects, no ambient proxy. The token is
//! resolved per fetch and never appears in an error string.

use super::{InboxBackend, InboxCaps, InboxError};
use futures_util::future::BoxFuture;
use serde::Deserialize;
use std::time::Duration;
use thegn_core::config::InboxAccount;
use thegn_core::inbox::{InboxItem, InboxPage, PageMode};
use thegn_core::secretref::BareAs;

const DEFAULT_BASE: &str = "https://api.todoist.com";
const OPERATION_TIMEOUT: Duration = Duration::from_secs(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_BODY_BYTES: usize = 1024 * 1024;
const PAGE_LIMIT: usize = 200;
/// A hard stop on pagination so a lying cursor cannot loop forever.
const MAX_PAGES: usize = 20;

pub(super) struct TodoistBackend {
    name: String,
    token: String,
    filter: String,
    base: String,
}

impl TodoistBackend {
    pub(super) fn new(account: &InboxAccount) -> Result<Self, InboxError> {
        let base = account.base_url.trim().trim_end_matches('/');
        let base = if base.is_empty() { DEFAULT_BASE } else { base };
        Ok(Self {
            name: account.name.clone(),
            token: account.token.clone(),
            filter: account.filter.clone(),
            base: base.to_string(),
        })
    }

    async fn fetch_inner(&self, max_items: usize) -> Result<InboxPage, InboxError> {
        let token = crate::secret::resolve(
            &self.token,
            BareAs::EnvName,
            &format!("inbox:{}", self.name),
        )
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| InboxError::Auth("token could not be resolved".into()))?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(OPERATION_TIMEOUT)
            .build()
            .map_err(|_| InboxError::Network("http client configuration failed".into()))?;
        let mut items = Vec::new();
        let mut truncated = false;
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let limit = PAGE_LIMIT.min(max_items.max(1) + 1);
            let mut query = vec![("query", self.filter.clone()), ("limit", limit.to_string())];
            if let Some(c) = &cursor {
                query.push(("cursor", c.clone()));
            }
            let response = client
                .get(format!("{}/api/v1/tasks/filter", self.base))
                .bearer_auth(&token)
                .query(&query)
                .send()
                .await
                .map_err(classify_send_error)?;
            let status = response.status();
            if status == 401 || status == 403 {
                return Err(InboxError::Auth(format!("HTTP {}", status.as_u16())));
            }
            if status == 429 {
                let wait = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok());
                return Err(InboxError::RateLimited(wait));
            }
            if status.is_server_error() {
                return Err(InboxError::Network(format!("HTTP {}", status.as_u16())));
            }
            if !status.is_success() {
                return Err(InboxError::Api(format!("HTTP {}", status.as_u16())));
            }
            let body = read_capped(response).await?;
            let page: TaskPage = serde_json::from_slice(&body)
                .map_err(|_| InboxError::Parse("unexpected Todoist response shape".into()))?;
            for task in page.results {
                if items.len() >= max_items {
                    truncated = true;
                    break;
                }
                items.push(task.into_item());
            }
            if truncated {
                break;
            }
            match page.next_cursor.filter(|c| !c.is_empty()) {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        if !truncated && cursor.is_some() && items.len() >= max_items {
            truncated = true;
        }
        Ok(InboxPage {
            items,
            removed: Vec::new(),
            cursor: None,
            mode: PageMode::Snapshot,
            truncated,
        })
    }
}

fn classify_send_error(error: reqwest::Error) -> InboxError {
    if error.is_timeout() {
        InboxError::Timeout
    } else {
        // Deliberately not `error.to_string()`: it can embed the URL.
        InboxError::Network("connection failed".into())
    }
}

async fn read_capped(mut response: reqwest::Response) -> Result<Vec<u8>, InboxError> {
    if response
        .content_length()
        .is_some_and(|n| n > MAX_BODY_BYTES as u64)
    {
        return Err(InboxError::BodyLimit);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(classify_send_error)? {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(InboxError::BodyLimit);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[derive(Deserialize)]
struct TaskPage {
    #[serde(default)]
    results: Vec<Task>,
    #[serde(default)]
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
struct Task {
    id: serde_json::Value,
    #[serde(default)]
    content: String,
    #[serde(default)]
    priority: Option<u8>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    due: Option<Due>,
}

#[derive(Deserialize)]
struct Due {
    #[serde(default)]
    date: Option<String>,
}

impl Task {
    fn into_item(self) -> InboxItem {
        let id = match &self.id {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        InboxItem {
            url: format!("https://app.todoist.com/app/task/{id}"),
            id,
            title: self.content,
            from: String::new(),
            labels: self.labels,
            due: self.due.and_then(|d| d.date),
            updated_at: None,
            // API priority 4 is p1 (urgent); higher is more urgent, as in InboxItem.
            priority: self.priority,
        }
    }
}

impl InboxBackend for TodoistBackend {
    fn kind(&self) -> &'static str {
        "todoist"
    }
    fn caps(&self) -> InboxCaps {
        InboxCaps::default()
    }
    fn fetch<'a>(
        &'a self,
        _cursor: &'a str,
        max_items: usize,
    ) -> BoxFuture<'a, Result<InboxPage, InboxError>> {
        Box::pin(self.fetch_inner(max_items))
    }
}

#[cfg(test)]
#[path = "todoist_tests.rs"]
mod tests;
