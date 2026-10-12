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
use axum::{Router, extract::Query, routing::get};
use serde::Deserialize;

pub const IDENTITY_PROTOCOL: u32 = thegn_core::proxy::attribution::IDENTITY_PROTOCOL_VERSION;

/// Builds the shared state and serves the proxy until the process is signalled.
pub async fn run(config: ProxyConfig) -> Result<()> {
    let secret_path = std::env::var_os("THEGN_MODEL_PROXY_SECRET_FILE")
        .context("THEGN_MODEL_PROXY_SECRET_FILE is required")?;
    let secret = read_secret(std::path::Path::new(&secret_path))?;
    budget::install_attribution_secret(secret.clone())
        .map_err(|_| anyhow::anyhow!("proxy attribution secret was already installed"))?;
    let db = std::sync::Arc::new(std::sync::Mutex::new(Db::open().context("open thegn.db")?));
    serve_with_secret(config, db, secret).await
}

/// Serves the proxy against an explicit DB handle (used by tests).
pub async fn serve(config: ProxyConfig, db: shared::SharedDb) -> Result<()> {
    serve_with_secret(config, db, Vec::new()).await
}

async fn serve_with_secret(
    config: ProxyConfig,
    db: shared::SharedDb,
    secret: Vec<u8>,
) -> Result<()> {
    let listen = config.listen;
    let state = AppState::new(config, db, now_ms());
    let app = server::app(state)
        .merge(Router::new().route("/thegn/identity", get(identity).with_state(secret)));
    // The listen socket IS the single-instance lock: a second bind loses.
    let listener = match tokio::net::TcpListener::bind(listen).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => return Err(e).with_context(|| format!("tgproxy listen address {listen} is already occupied; verify the existing daemon identity or choose another port")),
        Err(e) => return Err(e).with_context(|| format!("bind {listen}")),
    };
    tracing::info!(%listen, "tgproxy listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("axum serve")?;
    Ok(())
}

#[derive(Deserialize)]
struct IdentityQuery {
    challenge: String,
}

async fn identity(
    Query(query): Query<IdentityQuery>,
    axum::extract::State(secret): axum::extract::State<Vec<u8>>,
) -> axum::Json<serde_json::Value> {
    axum::Json(
        serde_json::json!({"protocol": IDENTITY_PROTOCOL, "proof": identity_proof(&secret, &query.challenge)}),
    )
}

fn identity_proof(secret: &[u8], challenge: &str) -> String {
    thegn_core::proxy::attribution::identity_proof(secret, challenge)
}

fn read_secret(path: &std::path::Path) -> Result<Vec<u8>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read proxy secret {}", path.display()))?;
    thegn_core::proxy::attribution::parse_secret_hex(raw.trim())
        .context("proxy secret file must contain 32 bytes encoded as hex")
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await; // best-effort: signal error just ends the wait
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
        let _ = tokio::signal::ctrl_c().await; // best-effort: signal error just ends the wait
    }
    tracing::info!("tgproxy shutting down");
}
