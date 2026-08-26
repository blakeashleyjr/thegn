//! thegn-proxy — the model proxy daemon (`tgproxy`).
//!
//! The async I/O shell around `thegn_core::proxy`'s pure routing logic: a
//! dual-protocol (OpenAI + Anthropic) local endpoint with ordered failover,
//! per-lane health backoff, token-bucket rate limiting, streaming relay, and
//! per-request cost/spend attribution. Resurrected from the pre-alpha
//! `thegn-proxy` excised in `85f3d1fb`, with the excision's lessons baked in:
//! fresh DB tables, a TOML-native provider registry received from the host, and
//! SecretRef-only key custody resolved inside this process.
//!
//! What stays dead: ACP/bouncer/tool interception (the sealed-egress bridge),
//! token compression, remote-sandbox tunnels, the managed-agent dialer.

pub mod anthropic_stream;
pub mod budget;
pub mod config;
pub mod headers;
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
pub mod usage_snapshot;

use anyhow::{Context, Result};
use thegn_core::db::Db;

use crate::model::ProxyConfig;
use crate::shared::now_ms;
use crate::state::AppState;

/// Builds the shared state and serves the proxy until the process is signalled.
pub async fn run(config: ProxyConfig) -> Result<()> {
    let db = std::sync::Arc::new(std::sync::Mutex::new(Db::open().context("open thegn.db")?));
    serve(config, db).await
}

/// Serves the proxy against an explicit DB handle (used by tests).
pub async fn serve(config: ProxyConfig, db: shared::SharedDb) -> Result<()> {
    let listen = config.listen;
    let state = AppState::new(config, db, now_ms());
    let app = server::app(state);
    // The listen socket IS the single-instance lock: a second bind loses.
    let listener = match tokio::net::TcpListener::bind(listen).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            tracing::info!(%listen, "tgproxy already bound — deferring to incumbent");
            return Ok(());
        }
        Err(e) => return Err(e).with_context(|| format!("bind {listen}")),
    };
    tracing::info!(%listen, "tgproxy listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("axum serve")?;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    tracing::info!("tgproxy shutting down");
}
