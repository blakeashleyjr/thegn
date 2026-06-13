//! The `agent` app tab — Termite Agent's `AgentUi`, hosted inside superzej.
//!
//! Synchronous runtime (provider streams are blocking iterators on a worker
//! thread), so the tokio handle is unused; the tile's ChangeHook wakes the
//! host loop. Provider selection is env-driven (see termite-cli): with no
//! `TERMITE_*`/API-key env it surfaces a provider error in-transcript, which is
//! the correct end-to-end behavior.

use sz_kit::{AppTile, ChangeHook};
use termite_cli::agent_ui::AgentUi;

pub async fn build(_rt: tokio::runtime::Handle, on_change: ChangeHook) -> Box<dyn AppTile> {
    Box::new(AgentUi::new("superzej", on_change))
}
