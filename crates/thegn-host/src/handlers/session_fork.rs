//! UI action for requesting a live daemon-session fork.
//!
//! The handler only validates the focused pane and starts the async control
//! request. The daemon writes the normal adopt intent, so placement returns
//! through the existing intent drain and never blocks the render loop.

use crate::panes::Panes;
use termwiz::terminal::TerminalWaker;
use thegn_core::config::Config;
use thegn_svc::control::ForkSpec;

/// Request a sibling fork for the focused pane. Returns the immediate status
/// to show in the compositor.
pub(crate) fn request(panes: &Panes, focused: u32, cfg: &Config, waker: &TerminalWaker) -> String {
    let Some(pane) = panes.table.get(&focused) else {
        return "Fork: focused pane is not a live session".into();
    };
    let Some(provider) = pane.provider_session() else {
        return "Fork: focused pane is not daemon-backed".into();
    };
    if provider.provider != "daemon" {
        return "Fork: focused pane is not a daemon session".into();
    }
    let session = provider.session;
    let daemon = cfg.daemon.clone();
    let wake = waker.clone();
    let shown = session.clone();
    let log_source = shown.clone();
    tokio::spawn(async move {
        let result = async {
            let client = crate::daemon::client::connect_daemon(&daemon)
                .await
                .ok_or_else(|| anyhow::anyhow!("pane daemon is not reachable"))?;
            client
                .fork(&ForkSpec {
                    session,
                    harness: None,
                    agent: None,
                    cwd: None,
                    worktree: None,
                    scrollback: false,
                    adopt: true,
                    tab: false,
                })
                .await
        }
        .await;
        match result {
            Ok(info) => tracing::debug!(
                target: "thegn::daemon",
                source = %log_source,
                child = %info.id,
                "UI session fork requested"
            ),
            Err(error) => tracing::warn!(
                target: "thegn::daemon",
                source = %log_source,
                "UI session fork failed: {error}"
            ),
        }
        let _ = wake.wake(); // best-effort: wake the compositor for the next status refresh
    });
    format!("Forking daemon session {shown}…")
}

#[cfg(test)]
mod tests {
    #[test]
    fn action_is_named_for_help_and_palette() {
        assert_eq!(
            crate::keymap::Action::from_key("fork-session")
                .expect("registered action")
                .key(),
            "fork-session"
        );
    }
}
