//! The `chat` app tab — Termite Chat's `ChatUi`, hosted inside superzej.
//!
//! Streaming runs on tokio tasks (reqwest-eventsource), so the tile takes the
//! host runtime handle; results fold in via the ChangeHook. Provider/model
//! come from the env (`TERMITE_*`); with no API key a submit surfaces the
//! error in-transcript — correct end-to-end behavior.

use sz_kit::{AppTile, ChangeHook, Theme};
use termite_chat::config::Config;
use termite_chat::session::{
    SessionRepository, SqliteSessionRepository, StoreConfig, open_repository,
};
use termite_chat::tile::ChatUi;

pub async fn build(
    rt: tokio::runtime::Handle,
    on_change: ChangeHook,
    theme: Theme,
) -> Box<dyn AppTile> {
    let cfg = Config::from_env();
    let client = reqwest::Client::new();
    Box::new(ChatUi::new(
        cfg,
        client,
        session_repo(),
        rt,
        on_change,
        theme,
    ))
}

/// The configured session store, falling back to an in-memory one so the tab
/// always opens even if the on-disk store can't be created.
fn session_repo() -> Box<dyn SessionRepository> {
    if let Ok(cfg) = StoreConfig::from_env()
        && let Ok(repo) = open_repository(cfg)
    {
        return repo;
    }
    Box::new(SqliteSessionRepository::in_memory().expect("in-memory session store"))
}
