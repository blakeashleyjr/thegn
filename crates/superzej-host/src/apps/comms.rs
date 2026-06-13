//! The `comms` app tab — switchboard's `CommsUi`, hosted inside superzej.
//!
//! For now this drives a seeded in-process mock so the tab is hermetic and
//! self-contained (no daemon, no sockets). Switching to the real daemon
//! (`sw_rpc::ensure_daemon` on a `spawn_blocking`, config-driven via
//! `[apps.comms]`) is a follow-up — the [`AppTile`] surface is identical either
//! way, so only this constructor changes.

use std::sync::Arc;

use sw_rpc::{CommsClient, MockClient};
use sw_tui::CommsUi;
use sz_kit::{AppTile, ChangeHook};

/// Construct the comms tile. Async because seeding the mock awaits; called from
/// the host loop on first focus (lazy-load), so the await is free.
pub async fn build(rt: tokio::runtime::Handle, on_change: ChangeHook) -> Box<dyn AppTile> {
    let client: Arc<dyn CommsClient> = seeded_mock().await;
    Box::new(CommsUi::new(client, rt, Some(on_change)))
}

/// A few conversations so the tab renders populated. Mirrors the standalone
/// binary's `--mock` seed (conversations only — enough to navigate).
async fn seeded_mock() -> Arc<MockClient> {
    use sw_core::model::{EntityId, Timestamp};
    use sw_rpc::proto::ConversationSummary;
    use ulid::Ulid;

    let mock = MockClient::new();
    for (name, provider) in [
        ("general", "irc"),
        ("engineering", "irc"),
        ("standup", "teams"),
    ] {
        mock.add_conversation(ConversationSummary {
            entity: EntityId(Ulid::new()),
            name: Some(name.into()),
            provider: provider.into(),
            last_activity: Timestamp::now(),
            event_count: 1,
        })
        .await;
    }
    mock
}
