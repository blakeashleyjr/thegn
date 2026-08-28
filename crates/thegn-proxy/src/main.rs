//! `tgproxy` — the model proxy daemon entry point. Loads its resolved config
//! (handed over by the host via `THEGN_MODEL_PROXY_CONFIG`, a temp file holding
//! the serialized `[model_proxy]` section — SecretRefs travel by reference and
//! are resolved here), then serves until signalled. The listen socket is the
//! single-instance lock, so a duplicate launch exits 0.

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let config = thegn_proxy::config::from_env()?;
    tracing::info!(
        listen = %config.listen,
        routes = config.routes.len(),
        "tgproxy starting"
    );
    thegn_proxy::run(config).await
}

/// Installs a tracing subscriber driven by `THEGN_LOG` (e.g. `THEGN_LOG=info`).
/// Defaults to `warn` when unset. Response/prompt bodies are never logged.
fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_env("THEGN_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = fmt() // best-effort: try_init fails only when a subscriber is already set
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
