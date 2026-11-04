//! Glue between resident plugin app state, host tiles, and app input routing.

use crate::keymap::{Action, KeyMap, Mode};
use crate::sequence::{Key, MatchResult};

/// Visible host overlays that own input before a plugin app.
#[derive(Clone, Copy, Default)]
pub(crate) struct HostModals {
    pub(crate) active_menu: bool,
    pub(crate) palette: bool,
    pub(crate) host_input: bool,
    pub(crate) git_input: bool,
    pub(crate) rollback: bool,
    pub(crate) wizard_ui: bool,
    pub(crate) env_wizard_ui: bool,
    pub(crate) terminal_wizard_ui: bool,
    pub(crate) workspace_picker: bool,
    pub(crate) help_overlay: bool,
    pub(crate) theme_builder: bool,
    pub(crate) monitor: bool,
    pub(crate) diff_view: bool,
    pub(crate) board: bool,
    pub(crate) pr_view: bool,
    pub(crate) bar_detail: bool,
    pub(crate) replay: bool,
    pub(crate) search: bool,
    pub(crate) search_replace: bool,
    pub(crate) onboarding: bool,
    pub(crate) which_key: bool,
    pub(crate) which_key_filter: bool,
    pub(crate) hover_popup: bool,
}

impl HostModals {
    pub(crate) fn from_flags(flags: [bool; 23]) -> Self {
        let [
            active_menu,
            palette,
            host_input,
            git_input,
            rollback,
            wizard_ui,
            env_wizard_ui,
            terminal_wizard_ui,
            workspace_picker,
            help_overlay,
            theme_builder,
            monitor,
            diff_view,
            board,
            pr_view,
            bar_detail,
            replay,
            search,
            search_replace,
            onboarding,
            which_key,
            which_key_filter,
            hover_popup,
        ] = flags;
        Self {
            active_menu,
            palette,
            host_input,
            git_input,
            rollback,
            wizard_ui,
            env_wizard_ui,
            terminal_wizard_ui,
            workspace_picker,
            help_overlay,
            theme_builder,
            monitor,
            diff_view,
            board,
            pr_view,
            bar_detail,
            replay,
            search,
            search_replace,
            onboarding,
            which_key,
            which_key_filter,
            hover_popup,
        }
    }

    pub(crate) fn any(&self) -> bool {
        self.active_menu
            || self.palette
            || self.host_input
            || self.git_input
            || self.rollback
            || self.wizard_ui
            || self.env_wizard_ui
            || self.terminal_wizard_ui
            || self.workspace_picker
            || self.help_overlay
            || self.theme_builder
            || self.monitor
            || self.diff_view
            || self.board
            || self.pr_view
            || self.bar_detail
            || self.replay
            || self.search
            || self.search_replace
            || self.onboarding
            || self.which_key
            || self.which_key_filter
            || self.hover_popup
    }
}

/// Synchronize app context, descriptors, visibility and plugin tile pumps.
pub(crate) fn sync(
    app_host: &mut crate::apps::AppHost,
    plugins: &mut crate::handlers::plugins::PluginsState,
    keymap: &mut KeyMap,
    mode: Mode,
    modal: bool,
    cfg: &thegn_core::config::Config,
) -> bool {
    let active_id = app_host.active_id().unwrap_or("work");
    plugins.update_app_context(active_id, modal);
    keymap.sync_app_context(mode, active_id, modal);
    let changed = app_host.sync_plugins(
        plugins.loading,
        plugins.app_tabs(),
        plugins.take_focus_request(),
        cfg,
    );
    let active_id = app_host.active_id().unwrap_or("work");
    plugins.update_app_context(active_id, modal);
    keymap.sync_app_context(mode, active_id, modal);
    app_host.pump_plugins() || changed
}

#[derive(PartialEq, Eq)]
pub(crate) enum KeyRoute {
    Action(Action),
    Pending,
    ToApp,
}

pub(crate) fn route_key(keymap: &mut KeyMap, mode: Mode, key: Key) -> KeyRoute {
    match keymap.dispatch_app(mode, key) {
        MatchResult::Matched(action) => KeyRoute::Action(action),
        MatchResult::Pending => KeyRoute::Pending,
        MatchResult::None => KeyRoute::ToApp,
    }
}

pub(crate) fn route_plugin_key(
    app_host: &mut crate::apps::AppHost,
    keymap: &mut KeyMap,
    mode: Mode,
    key: Key,
) -> KeyRoute {
    let route = route_key(keymap, mode, key.clone());
    if route == KeyRoute::ToApp
        && let Some(event) = crate::apps::input::to_kit(&key.code, key.mods)
    {
        app_host.handle_active_input(event);
    }
    route
}

pub(crate) fn command_active(app_host: &mut crate::apps::AppHost, action: &Action) -> bool {
    app_host.command_active(action.key())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    // The fixture has one already-discovered resident AppTab, no outstanding
    // request, and no background-sync capability: steady sync must not arm a timer.
    #[test]
    fn idle_plugin_app_adds_no_timer_and_no_wake() {
        let _channel_guard =
            crate::handlers::plugins::app_tests::channel_guard(thegn_core::channel::Channel::Dev);
        let cfg = thegn_core::config::Config::default();
        let mut plugins =
            crate::handlers::plugins::app_tests::test_state_with_app("idle", "app", "idle/app");
        plugins.loading = false;
        let shared = plugins.plugins["idle"].apps["app"].clone();
        shared
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .apply_view(thegn_core::plugin_app::AppView {
                state_scope: "default".into(),
                revision: 1,
                root: thegn_core::plugin_app::Widget::Text {
                    id: "root".into(),
                    title: "Ready".into(),
                    text: "idle".into(),
                },
                status: String::new(),
            })
            .expect("valid app view");
        let mut app_host = crate::apps::AppHost::from_config(&cfg);
        let mut keymap = crate::keymap::default_keymap();
        let mut settled = false;
        for _ in 0..4 {
            if !sync(
                &mut app_host,
                &mut plugins,
                &mut keymap,
                Mode::Normal,
                false,
                &cfg,
            ) {
                settled = true;
                break;
            }
        }
        assert!(settled, "plugin app sync must settle within four turns");
        app_host.set_active(crate::apps::ActiveApp::Tile(0));
        assert!(
            app_host.active_is_plugin(),
            "the resident app is the active tile"
        );
        let mut active_settled = false;
        for _ in 0..4 {
            if !sync(
                &mut app_host,
                &mut plugins,
                &mut keymap,
                Mode::Normal,
                false,
                &cfg,
            ) {
                active_settled = true;
                break;
            }
        }
        assert!(active_settled, "active plugin app context must settle");
        for _ in 0..3 {
            assert!(!sync(
                &mut app_host,
                &mut plugins,
                &mut keymap,
                Mode::Normal,
                false,
                &cfg
            ));
            assert_eq!(plugins.next_app_deadline(), None);
            assert_eq!(
                crate::idle_poll::fold_deadline(
                    crate::idle_poll::poll_timeout(None, false, false, false),
                    plugins.next_app_deadline(),
                    Instant::now()
                ),
                None
            );
            assert_eq!(
                shared
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .test_pending_request_count(),
                0
            );
        }

        {
            let mut app = shared.lock().unwrap_or_else(|error| error.into_inner());
            app.session = Some(1);
            app.track_test_pending(thegn_core::plugin_app::AppEventParams {
                surface: "idle/app".into(),
                session: "test-session".into(),
                state_scope: "default".into(),
                revision: 1,
                focus_ticket: None,
                event: thegn_core::plugin_app::AppEvent::Invoke {
                    id: "refresh".into(),
                },
            });
        }
        let timeout = crate::idle_poll::fold_deadline(
            Some(Duration::from_secs(15)),
            plugins.next_app_deadline(),
            Instant::now(),
        );
        assert!(timeout.is_some_and(|remaining| remaining <= Duration::from_secs(15)));
        assert!(plugins.next_app_deadline().is_some());
        drop(_channel_guard);
    }

    #[test]
    fn sync_does_not_steal_observe_input_routing() {
        let mut observe =
            crate::apps::AppHost::new(vec![crate::apps::AppSlot::new("observe", "Observe")]);
        observe.set_active(crate::apps::ActiveApp::Tile(0));
        assert!(!observe.active_is_plugin());
        let work = crate::apps::AppHost::new(Vec::new());
        assert!(!work.active_is_plugin());
    }
}
