//! `tgproxy` — the LLM proxy daemon entry point. Loads config from the
//! environment, then serves until signalled. Designed to run standalone (point
//! an agent's `OPENAI_BASE_URL`/`ANTHROPIC_BASE_URL` at it) and, in production,
//! as a `PinSupervisor`-managed pinned program inside thegn.

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

/// Installs a tracing subscriber driven by `THEGN_LOG` (e.g.
/// `THEGN_LOG=info`). No-op friendly: defaults to `warn` when unset.
fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_env("THEGN_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
