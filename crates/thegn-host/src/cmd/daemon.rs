//! `thegn daemon stop` — the operator verb over the pane daemon's control
//! socket. The daemon itself is run by the hidden bare `thegn daemon` (spawned
//! lazily by the first attach / `thegn serve`); this subcommand only *stops* a
//! running one, driving `POST /v1/daemon/shutdown` (admin scope; local
//! unix-socket peers hold implicit admin). With no daemon running it degrades
//! to a clear message rather than an error.

use anyhow::Result;
use clap::Subcommand;
use thegn_core::config::Config;
use thegn_core::outln;

#[derive(Subcommand, Clone)]
pub enum DaemonAction {
    /// Stop the running pane daemon gracefully (admin).
    Stop,
}

pub fn run(cfg: &Config, action: DaemonAction) -> Result<()> {
    match action {
        DaemonAction::Stop => stop(cfg),
    }
}

fn stop(cfg: &Config) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        // No daemon is not an error — stopping an already-stopped daemon is a
        // no-op success, and the connect helper's message names the situation.
        let client = match super::session::connect(cfg).await {
            Ok(c) => c,
            Err(_) => {
                outln!("no thegn pane daemon is running");
                return Ok(());
            }
        };
        client.call_raw("POST", "/v1/daemon/shutdown", None).await?;
        outln!("pane daemon shutting down");
        Ok(())
    })
}
