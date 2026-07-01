use chrono::{DateTime, Utc};
use std::future::Future;
use std::pin::Pin;

use crate::frame::Frame;

#[derive(Debug, Clone)]
pub struct TimeRange {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct Query {
    pub ref_id: String,
    pub expr: String,
    pub time_range: TimeRange,
}

#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error("network error: {0}")]
    Network(String),
    #[error("auth error: {0}")]
    Auth(String),
    #[error("syntax error: {0}")]
    Syntax(String),
    #[error("timeout")]
    Timeout,
    #[error("other error: {0}")]
    Other(String),
}

pub trait DataSource: Send + Sync {
    fn query(
        &self,
        queries: Vec<Query>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Frame>, QueryError>> + Send>>;
}
