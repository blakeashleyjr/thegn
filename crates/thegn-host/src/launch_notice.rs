//! Deliver launch-gate warnings to the running compositor as status, toast,
//! and a durable notification-inbox event.

pub(crate) const KIND: &str = "launch_degraded";

pub(crate) fn deliver(
    state: &crate::notify::NotifyState,
    worktree: &str,
    text: &str,
) -> thegn_core::notification_route::RouteDecision {
    let display = format!("launch warning: {text}");
    state.emit_status(&display);
    state.emit_toast(&display, thegn_core::notification::Priority::Notice);
    crate::notify::route(state, KIND, worktree, text, worktree)
}

pub(crate) fn post(worktree: &str, text: &str) {
    post_with(worktree, text, |job| {
        let spawn = thegn_core::thread_name::try_spawn_named("thegn-launchnt", move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
            job();
        });
        if let Err(error) = spawn {
            tracing::debug!(target: "thegn::launch_notice", %error, "best-effort launch warning inbox worker could not start");
        }
    });
}

/// Route a warning and hand persistence to the supplied worker seam. Tests
/// execute the job inline so isolated state remains active until its DB write
/// has completed.
pub(crate) fn post_with(worktree: &str, text: &str, spawn: impl FnOnce(Box<dyn FnOnce() + Send>)) {
    if !thegn_core::msg::tui_active() {
        return;
    }
    let Some(state) = crate::notify::global() else {
        return;
    };
    let decision = deliver(&state, worktree, text);
    if decision.record {
        let worktree = worktree.to_string();
        let text = text.to_string();
        spawn(Box::new(move || match thegn_core::db::Db::open() {
            Ok(db) => {
                if let Err(error) = crate::automation_events::insert_routed(
                    &db,
                    KIND,
                    &worktree,
                    &text,
                    &worktree,
                    Default::default(),
                    &decision,
                    true,
                ) {
                    tracing::debug!(target: "thegn::launch_notice", %error, "best-effort launch warning inbox write failed");
                }
            }
            Err(error) => {
                tracing::debug!(target: "thegn::launch_notice", %error, "best-effort launch warning inbox database open failed");
            }
        }));
    }
}

#[cfg(test)]
mod tests {
    // These tests touch TUI and NotifyState process globals. The repository's
    // nextest configuration runs each test in its own process.
    use super::*;
    use crate::hydrate::RefreshKind;
    use thegn_core::config::{Config, NamedCommand};
    use thegn_core::store::NotificationStore;

    fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<RefreshKind>) -> Vec<RefreshKind> {
        let mut messages = Vec::new();
        for _ in 0..32 {
            match rx.try_recv() {
                Ok(message) => messages.push(message),
                Err(_) => break,
            }
        }
        messages
    }

    fn post_inline(worktree: &str, text: &str) {
        post_with(worktree, text, |job| job());
    }

    type Setup = (
        crate::testenv::IsolatedState,
        std::sync::Arc<crate::notify::NotifyState>,
        tokio::sync::mpsc::UnboundedReceiver<RefreshKind>,
        termwiz::terminal::TerminalWaker,
        Vec<Box<dyn std::any::Any>>,
    );

    fn setup() -> Setup {
        let isolated = crate::testenv::IsolatedState::new();
        let (waker, keepalive_a, keepalive_b) =
            crate::platform::sound_process::test_support::test_waker();
        let keepalive: Vec<Box<dyn std::any::Any>> =
            vec![Box::new(keepalive_a), Box::new(keepalive_b)];
        let state = crate::notify::NotifyState::new(
            Config::default().notifications,
            String::new(),
            waker.clone(),
        );
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        state.set_toast_tx(tx);
        (isolated, state, rx, waker, keepalive)
    }

    #[test]
    fn deliver_sends_the_status_line_and_a_toast() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let (_isolated, state, mut rx, _waker, _keepalive) = setup();
        let decision = deliver(
            &state,
            "/wt/a",
            "the state database could not be opened: boom; the shell launches without it",
        );
        let messages = drain(&mut rx);
        assert!(decision.record);
        assert!(messages.iter().any(|message| match message {
            RefreshKind::PrActionStatus(text) => text.contains("boom"),
            _ => false,
        }));
        assert!(messages.iter().any(|message| match message {
            RefreshKind::Toast { message, .. } => message.contains("boom"),
            _ => false,
        }));
    }

    #[test]
    fn post_is_silent_without_the_compositor() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let (_isolated, _state, mut rx, _waker, _keepalive) = setup();
        thegn_core::msg::set_tui_active(false);
        post("/wt/a", "x");
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn launch_state_gate_warning_reaches_the_status_line() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let (_isolated, _state, mut rx, _waker, _keepalive) = setup();
        thegn_core::msg::set_tui_active(true);
        let result = crate::agent::launch_state_gate_with_notice(
            &Config::default(),
            "/wt/a",
            "shell",
            "this worktree's repository root could not be read",
            &anyhow::anyhow!("disk I/O error"),
            post_inline,
        );
        thegn_core::msg::set_tui_active(false);
        assert!(result.is_ok());
        assert!(drain(&mut rx).iter().any(|message| match message {
            RefreshKind::PrActionStatus(text) => {
                text.contains("repository root could not be read")
                    && text.contains("disk I/O error")
            }
            _ => false,
        }));
    }

    #[test]
    fn dotfiles_gate_warning_reaches_the_status_line() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let (_isolated, _state, mut rx, _waker, _keepalive) = setup();
        thegn_core::msg::set_tui_active(true);
        let result = crate::agent::dotfiles_launch_gate_with_notice(
            &Config::default(),
            "/wt/a",
            "shell",
            anyhow::anyhow!("cannot create HOME"),
            post_inline,
        );
        thegn_core::msg::set_tui_active(false);
        assert!(result.is_ok());
        assert!(drain(&mut rx).iter().any(|message| match message {
            RefreshKind::PrActionStatus(text) => text.contains("cannot create HOME"),
            _ => false,
        }));
    }

    #[test]
    fn launch_state_gate_refusal_posts_nothing() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let (_isolated, _state, mut rx, _waker, _keepalive) = setup();
        thegn_core::msg::set_tui_active(true);
        let cfg = Config {
            agents: vec![NamedCommand {
                name: "codex".into(),
                command: "codex".into(),
                hints: Vec::new(),
                provider: None,
                harness: None,
                resume: false,
                route_via_proxy: false,
                model: None,
                env: Default::default(),
                permissions: Vec::new(),
                drawer_scope: None,
                drawer_cwd: None,
            }],
            ..Config::default()
        };
        let result = crate::agent::launch_state_gate(
            &cfg,
            "/wt/a",
            "codex",
            "saved override unreadable",
            &anyhow::anyhow!("disk I/O error"),
        );
        thegn_core::msg::set_tui_active(false);
        let Err(error) = result else {
            panic!("configured entry must be refused");
        };
        assert!(format!("{error:#}").contains("refusing to launch `codex`"));
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn post_persists_only_one_inbox_row_for_a_worktree_and_reason() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let (_isolated, _state, _rx, _waker, _keepalive) = setup();
        thegn_core::msg::set_tui_active(true);
        let reason = "saved override could not be read: disk I/O error";
        post_inline("/wt/a", reason);
        post_inline("/wt/a", reason);
        thegn_core::msg::set_tui_active(false);

        let db = thegn_core::db::Db::open().unwrap();
        let matching = db
            .get_all_notifications(64)
            .unwrap()
            .into_iter()
            .filter(|notification| {
                notification.source_ref == "/wt/a"
                    && notification.worktree_path == "/wt/a"
                    && notification.message == reason
            })
            .count();
        assert_eq!(matching, 1, "duplicate pane launches share one inbox row");
    }
}
