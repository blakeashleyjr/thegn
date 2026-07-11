//! thegn-proxy — the LLM proxy daemon (`tgproxy`).
//!
//! The async I/O shell around `thegn_core::proxy`'s pure routing logic. It is
//! the single chokepoint all agent model traffic crosses: a dual-protocol
//! (OpenAI + Anthropic) router with ordered failover, per-class health backoff,
//! token-bucket rate limiting, streaming relay, cost/spend attribution, per-agent
//! virtual keys, and budget enforcement. Port of the Go `model-proxy` (group U)
//! extended with the V budget machinery; the Claude-Max OAuth subscription path
//! and the AR gateway layer are out of milestone-1 scope.

pub mod anthropic_stream;
pub mod budget;
pub mod config;
pub mod health;
pub mod metrics;
pub mod model;
pub mod relay;
pub mod reset;
pub mod router;
pub mod server;
pub mod shared;
pub mod state;
pub mod upstream;

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use thegn_core::db::Db;

use crate::model::ProxyConfig;
use crate::shared::now_ms;
use crate::state::AppState;

/// Builds the shared state and serves the proxy until the process is signalled.
pub async fn run(config: ProxyConfig) -> Result<()> {
    let db = Arc::new(Mutex::new(Db::open().context("open thegn.db")?));
    serve(config, db).await
}

/// Serves the proxy against an explicit DB handle (used by tests).
pub async fn serve(config: ProxyConfig, db: shared::SharedDb) -> Result<()> {
    let listen = config.listen;
    let state = AppState::new(config, db, now_ms());
    let app = server::app(state);
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("bind {listen}"))?;
    tracing::info!(%listen, "tgproxy listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("axum serve")?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("tgproxy shutting down");
}
