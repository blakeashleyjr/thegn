//! HTTP adapters for the cache-first CI read capabilities.
//!
//! The provider/cache policy belongs to [`super::ControlApi`]; these handlers
//! only authenticate, decode query parameters, and serialize the bounded wire
//! response.

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use thegn_core::control::Verb;

use super::http::ControlState;
use super::http::authed_target;

#[derive(Debug, Default, Deserialize)]
pub(super) struct CiRunsQuery {
    #[serde(default)]
    pub worktree: String,
    pub limit: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct CiLogsQuery {
    #[serde(default)]
    pub worktree: String,
    pub run: String,
    pub job: String,
    pub tail_lines: Option<usize>,
}

pub(super) async fn ci_runs(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Query(query): Query<CiRunsQuery>,
) -> Response {
    if let Err(response) = authed_target(&state, &headers, Verb::CiRuns, &query.worktree) {
        return response;
    }
    match state
        .api
        .ci_runs(&query.worktree, query.limit.unwrap_or(0))
        .await
    {
        Ok(reply) => axum::Json(reply).into_response(),
        Err(error) => error.into_response(),
    }
}

pub(super) async fn ci_logs(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Query(query): Query<CiLogsQuery>,
) -> Response {
    if let Err(response) = authed_target(&state, &headers, Verb::CiLogs, &query.worktree) {
        return response;
    }
    if query.run.is_empty() || query.job.is_empty() {
        return super::http::bad_request("ci logs requires `run` and `job`");
    }
    match state
        .api
        .ci_logs(&query.worktree, &query.run, &query.job, query.tail_lines)
        .await
    {
        Ok(reply) => axum::Json(reply).into_response(),
        Err(error) => error.into_response(),
    }
}
