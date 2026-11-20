//! Config-reload hooks: the swap groups the UI loop runs when the config
//! watcher publishes a new [`Config`], gated on the
//! [`ReloadPlan`] so a hook runs
//! only when one of its keys changed (or the diff was incomplete).
//!
//! Extracted from the reload handler in `run.rs`. Every hook runs ON the loop
//! inside the existing `WakeSource::Config` wake: it rebuilds models or queues
//! work, never does I/O. Work that touches loop-owned workers or process-wide
//! state (a weather re-poll, the refresh ticker's cadences, the push workers,
//! the media watcher, the automation runtime, the panel width atomics) is
//! returned in [`ReloadOutcome`] for the caller to apply, which keeps this
//! module runtime-free and unit-testable.
//!
//! A capped or failed diff makes [`ReloadPlan::runs`] true for every hook, so
//! every hook runs exactly as the old unconditional handler did.

use crate::chrome::FrameModel;
use crate::handlers::sidebar_persist::SidebarState;
use std::sync::Arc;
use thegn_core::config::reload_class::{ReloadHook, ReloadPlan};
use thegn_core::config::{Config, FocusDetail, MetricsConfig, NotificationsConfig, WeatherConfig};
use thegn_core::help::HelpRegistry;

/// The loop state the extracted hooks touch.
pub(crate) struct ReloadCtx<'a> {
    pub(crate) model: &'a mut FrameModel,
    pub(crate) sb: &'a mut SidebarState,
    pub(crate) session: &'a crate::session::Session,
    pub(crate) panel_ui: &'a mut crate::panel::PanelUi,
    pub(crate) bar_detail: &'a mut Option<crate::detail::DetailOverlay>,
    /// The freshly published config.
    pub(crate) new_cfg: &'a Config,
    /// The process locale (`CalendarDocs::env_locale`), read by the caller.
    pub(crate) locale: Option<String>,
    /// The loop's help registry (embeds the effective keymap page).
    pub(crate) help_registry: &'a mut Arc<HelpRegistry>,
    /// The user's nudged panel width, which wins over `[panel] width`.
    pub(crate) panel_cols_pref: Option<usize>,
}

/// `[panel] width` / `half_ratio` for `layout::set_panel_width_cfg`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PanelWidthCfg {
    pub(crate) normal_cols: Option<usize>,
    pub(crate) half_ratio: Option<f32>,
}

/// An off-loop weather poll the caller spawns
/// (`hydrate_weather::spawn_poll_with_generation`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WeatherPoll {
    pub(crate) cfg: WeatherConfig,
    pub(crate) locale: Option<String>,
}

/// What the hooks ask of the loop after they ran.
#[derive(Debug, Default)]
pub(crate) struct ReloadOutcome {
    /// Re-poll the weather now (the location/provider/units moved).
    pub(crate) weather_poll: Option<WeatherPoll>,
    /// Bar chrome changed: recompose the bars.
    pub(crate) bars_dirty: bool,
    /// Something visible changed: paint a frame.
    pub(crate) dirty: bool,
    /// Re-arm the refresh ticker from the new config
    /// (`ScheduleOwner::reconfigure`, a no-op when the cadences, i.e.
    /// `ScheduleConfig::from_config`, did not move).
    pub(crate) reschedule: bool,
    /// Republish notification routing + rebuild the push workers
    /// (`install_push_workers`) from this effective config.
    pub(crate) notifications: Option<NotificationsConfig>,
    /// Restart the media watcher and cancel in-flight album art.
    pub(crate) restart_media: bool,
    /// Reinstall the automation runtime (`automation_runtime::install`).
    pub(crate) automations: bool,
    /// Store the panel width config (`layout::set_panel_width_cfg`).
    pub(crate) panel_width: Option<PanelWidthCfg>,
    /// Re-arm the `[metrics]` supervisor from this reloaded section: the
    /// caller builds a fresh `metrics_slot` (reusing the loop waker) and
    /// replaces its inbox, which drops the old consumer. The old supervisor
    /// sees `consumer_gone` before its next target or publish and exits; it
    /// may still sleep up to its old `interval_secs` first, but it publishes
    /// nothing after the swap (no stop flag, no join on the loop).
    pub(crate) metrics: Option<MetricsConfig>,
    /// Re-apply the `[monitor]` ladder and per-tab defaults to the loop's
    /// `MonitorPrefs` (`MonitorPrefs::reapply_config`); the next monitor open
    /// sees them, an open monitor keeps its state.
    pub(crate) monitor: bool,
}

/// Every changed key in the plan, whatever its class.
fn changed_keys(plan: &ReloadPlan) -> impl Iterator<Item = &str> {
    plan.live
        .values()
        .flatten()
        .chain(&plan.restart)
        .chain(&plan.next_use)
        .chain(&plan.unclassified)
        .map(String::as_str)
}

/// The refresh ticker's cadences read `[bars]` clock/date formats, `[usage]`
/// (Bars), `[calendar]` and `[weather]` as well as the Schedule rows.
pub(crate) fn schedule_runs(plan: &ReloadPlan) -> bool {
    [
        ReloadHook::Schedule,
        ReloadHook::Bars,
        ReloadHook::Calendar,
        ReloadHook::Weather,
    ]
    .into_iter()
    .any(|hook| plan.runs(hook))
}

/// The panel reads `[panel]`, the keymap (keys cheatsheet) and the section
/// gates `pr_queue.enabled` (Schedule), `media.enabled` (Media) and
/// `usage.enabled` (Bars).
pub(crate) fn panel_runs(plan: &ReloadPlan) -> bool {
    [
        ReloadHook::Panel,
        ReloadHook::Keymap,
        ReloadHook::Schedule,
        ReloadHook::Media,
        ReloadHook::Bars,
    ]
    .into_iter()
    .any(|hook| plan.runs(hook))
}

/// The help registry embeds the effective keymap page.
pub(crate) fn help_runs(plan: &ReloadPlan) -> bool {
    plan.runs(ReloadHook::Help) || plan.runs(ReloadHook::Keymap)
}

/// The effective notification config folds the active profile's overlay
/// (`Config::effective_notifications`), so a `profile` switch or a
/// `profiles.<name>.notifications` edit re-routes too.
pub(crate) fn notifications_runs(plan: &ReloadPlan) -> bool {
    plan.runs(ReloadHook::Notifications)
        || changed_keys(plan).any(|key| key == "profile" || key.starts_with("profiles."))
}

/// App tabs follow `[apps]`, the plugin specs (plugin tabs) and `[observe]`
/// (`AppHost::reconcile` rebuilds the observe slot when it moved).
pub(crate) fn app_host_runs(plan: &ReloadPlan) -> bool {
    plan.runs(ReloadHook::AppHost)
        || plan.runs(ReloadHook::Plugins)
        || changed_keys(plan).any(|key| key == "observe" || key.starts_with("observe."))
}

/// The plugin host restarts when the plugin list changed. The plugin
/// dispatcher also holds the whole config (`PluginsState::new`) for the
/// operations it runs on a plugin's behalf, which read NextUse keys afresh,
/// so a NextUse (or unclassified) edit restarts it too; a Live-only edit
/// (theme, sidebar, bars, ...) no longer does.
pub(crate) fn plugins_restart_runs(plan: &ReloadPlan) -> bool {
    plan.runs(ReloadHook::Plugins) || !plan.next_use.is_empty() || !plan.unclassified.is_empty()
}

/// The automation runtime holds the whole config (executor) and publishes the notification
/// route snapshot, and `effective_automations` folds the profile overlay.
pub(crate) fn automations_run(plan: &ReloadPlan) -> bool {
    plan.runs(ReloadHook::Automation)
        || notifications_runs(plan)
        || !plan.next_use.is_empty()
        || !plan.unclassified.is_empty()
}

/// Is any push sink configured in this effective notification config? Feeds
/// `FrameModel::notification_delivery_configured` (the Monitor delivery row).
pub(crate) fn delivery_configured(notifications: &NotificationsConfig) -> bool {
    notifications
        .push
        .effective_sinks()
        .iter()
        .any(thegn_core::config_push::PushSinkConfig::is_configured)
}

/// Run the extracted hooks whose keys changed.
pub(crate) fn apply(plan: &ReloadPlan, ctx: &mut ReloadCtx<'_>) -> ReloadOutcome {
    let mut out = ReloadOutcome::default();
    if plan.runs(ReloadHook::Sidebar) {
        sidebar(plan, ctx);
        out.dirty = true;
    }
    // The accent lives in `[theme]` but is cached beside the bar configs; the
    // clock/date formats are Schedule rows but render from the bar configs.
    if plan.runs(ReloadHook::Bars)
        || plan.runs(ReloadHook::Theme)
        || plan.runs(ReloadHook::Schedule)
    {
        bars(ctx);
        out.bars_dirty = true;
        out.dirty = true;
    }
    if plan.runs(ReloadHook::Weather) {
        weather(ctx, &mut out);
    }
    if plan.runs(ReloadHook::Calendar) {
        calendar(ctx);
        out.dirty = true;
    }
    out.reschedule = schedule_runs(plan);
    if notifications_runs(plan) {
        notifications(ctx, &mut out);
    }
    if panel_runs(plan) {
        panel(ctx, &mut out);
    }
    if help_runs(plan) {
        help(ctx);
        out.dirty = true;
    }
    if plan.runs(ReloadHook::Media) {
        media(ctx, &mut out);
    }
    out.automations = automations_run(plan);
    if plan.runs(ReloadHook::Metrics) {
        out.metrics = Some(ctx.new_cfg.metrics.clone());
    }
    out.monitor = plan.runs(ReloadHook::Monitor);
    out
}

/// `[notifications]`: hand the effective config to the caller's push-worker
/// rebuild and refresh the cached "a sink is configured" bit (read by the
/// Monitor delivery row; computing it per frame cloned the whole config).
fn notifications(ctx: &mut ReloadCtx<'_>, out: &mut ReloadOutcome) {
    let effective = ctx.new_cfg.effective_notifications(None);
    ctx.model.notification_delivery_configured = delivery_configured(&effective);
    out.notifications = Some(effective);
    out.dirty = true;
}

/// `[panel]`: section order/gates, the keys cheatsheet (follows the keymap)
/// and the width config (the user's nudged width wins).
fn panel(ctx: &mut ReloadCtx<'_>, out: &mut ReloadOutcome) {
    let new = ctx.new_cfg;
    ctx.panel_ui.apply_config(new);
    ctx.panel_ui.docs.cfg_keys = crate::keyhint::cheatsheet_groups(new);
    out.panel_width = Some(PanelWidthCfg {
        normal_cols: ctx.panel_cols_pref.or(new.panel.width),
        half_ratio: new.panel.half_ratio,
    });
    out.dirty = true;
}

/// The help registry embeds the effective keymap page: rebuild it and point
/// the panel's help view at the new one.
fn help(ctx: &mut ReloadCtx<'_>) {
    *ctx.help_registry = Arc::new(crate::help::pages::registry_logged(ctx.new_cfg));
    ctx.panel_ui.help.reg = Some(Arc::clone(ctx.help_registry));
}

/// `[media]`: the art toggle applies here; the watcher restart and the
/// in-flight art cancel are the caller's (they own the watcher handle).
fn media(ctx: &mut ReloadCtx<'_>, out: &mut ReloadOutcome) {
    ctx.panel_ui
        .media
        .set_art_enabled(ctx.new_cfg.media.show_art);
    ctx.panel_ui.media.cancel_art_request();
    out.restart_media = true;
    out.dirty = true;
}

/// Should a reload drop the `i` runtime override of
/// `[ui] sidebar_focus_detail`?
///
/// The override is persisted in `ui_state` and wins over config, so without
/// this an edit to `sidebar_focus_detail` never applied, and an edit to the
/// detail fields was invisible while the override was `off` (the detail tier
/// is hidden on every row). An explicit config edit of those keys is the newer
/// intent, so it releases the override.
fn releases_focus_override(current: Option<FocusDetail>, changed: &[String]) -> bool {
    let Some(current) = current else {
        return false;
    };
    changed.iter().any(|key| key == "ui.sidebar_focus_detail")
        || (current == FocusDetail::Off
            && changed
                .iter()
                .any(|key| key.starts_with("ui.sidebar_detail_")))
}

/// `[ui] sidebar_*`: sort/sections/freeze, the display struct, then one row
/// rebuild (which syncs the display into the model).
fn sidebar(plan: &ReloadPlan, ctx: &mut ReloadCtx<'_>) {
    let ui = &ctx.new_cfg.ui;
    ctx.sb.view.workspace_sort = ui.sidebar_workspace_sort;
    ctx.sb.view.terminals_section = ui.sidebar_terminals_section;
    ctx.sb.view.lead_enabled = crate::lead::enabled(ctx.new_cfg);
    crate::sidebar_freeze::set_enabled(ctx.sb, ui.sidebar_freeze_sort, &ctx.model.sidebar_status);
    ctx.sb.view.display = crate::sidebar_view::SidebarDisplay::from_ui(ui);
    let changed = plan
        .live
        .get(&ReloadHook::Sidebar)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if releases_focus_override(ctx.sb.focus_detail_override, changed) {
        ctx.sb.focus_detail_override = None;
        ctx.sb.unpersist("sidebar_focus_detail");
    }
    // `workspace_sort`/`terminals_section` are only read inside `build_rows`,
    // and nothing else on the reload path rebuilds rows — without this, the
    // edit sits invisible until some unrelated hydration happens to rebuild
    // (an idle repo's fast path never does). `rebuild` ends in `sync`, which
    // copies the new display (detail fields included) into the model.
    ctx.sb.rebuild(ctx.model, ctx.session);
}

/// `[bars]`, `[stats]`, `[usage]` and the `[theme]` accent.
fn bars(ctx: &mut ReloadCtx<'_>) {
    let new = ctx.new_cfg;
    ctx.model.accent = new.accent_rgb();
    ctx.model.bars = new.bars.clone();
    ctx.model.stats_icons = new.stats.clone();
    ctx.model.usage_cfg = new.usage.clone();
}

/// `[weather]`: a moved provider/location/units leaves the old reading on
/// screen until the next scheduled poll (>= 10 min), so clear it and ask for
/// one poll now.
fn weather(ctx: &mut ReloadCtx<'_>, out: &mut ReloadOutcome) {
    let new = &ctx.new_cfg.weather;
    if crate::hydrate_weather::reload_needs_poll(&ctx.model.weather_cfg, new, ctx.locale.as_deref())
    {
        ctx.model.weather = None;
        out.bars_dirty = true;
        out.dirty |= crate::detail::retick_open(ctx.bar_detail, None);
        out.weather_poll = Some(WeatherPoll {
            cfg: new.clone(),
            locale: ctx.locale.clone(),
        });
    }
    ctx.model.weather_cfg = new.clone();
}

/// `[calendar]`: new clocks/zones/week start take effect on the next open.
/// Cached events are carried over while the home zone is unchanged — they
/// are keyed by date, not by config.
fn calendar(ctx: &mut ReloadCtx<'_>) {
    let docs = &mut ctx.panel_ui.docs;
    let prev = std::mem::take(&mut docs.calendar);
    docs.calendar = crate::calendar_docs::CalendarDocs::from_config(
        &ctx.new_cfg.calendar,
        ctx.locale.as_deref(),
    );
    if docs.calendar.home == prev.home {
        docs.calendar.events = prev.events;
        docs.calendar.loaded = prev.loaded;
        docs.calendar.errors = prev.errors;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PanelWidthCfg, ReloadCtx, WeatherPoll, app_host_runs, apply, automations_run,
        delivery_configured, notifications_runs, plugins_restart_runs, releases_focus_override,
    };
    use crate::chrome::FrameModel;
    use crate::handlers::sidebar_persist::SidebarState;
    use crate::hydrate_schedule::ScheduleConfig;
    use std::sync::Arc;
    use thegn_core::config::reload_class::{ReloadHook, ReloadPlan, plan, plan_keys};
    use thegn_core::config::{Config, FocusDetail};
    use thegn_core::help::HelpRegistry;
    use thegn_core::weather::{Sky, Units, WeatherSnapshot};

    fn cfg(toml_text: &str) -> Config {
        toml::from_str(toml_text).expect("config parses")
    }

    fn snapshot() -> WeatherSnapshot {
        WeatherSnapshot {
            provider: "wttr_in".into(),
            place: "Oslo".into(),
            sky: Sky::Partly,
            description: "Partly cloudy".into(),
            temp: 7.5,
            feels_like: 3.0,
            hi: 9.0,
            lo: 2.0,
            humidity_pct: 81,
            wind: 24.0,
            units: Units::Metric,
            fetched_at: 1_760_000_000,
            forecast: Vec::new(),
        }
    }

    struct Fixture {
        model: FrameModel,
        sb: SidebarState,
        session: crate::session::Session,
        panel_ui: crate::panel::PanelUi,
        bar_detail: Option<crate::detail::DetailOverlay>,
        help_registry: Arc<HelpRegistry>,
    }

    impl Fixture {
        fn new(old: &Config) -> Self {
            let model = FrameModel {
                weather_cfg: old.weather.clone(),
                sidebar_display: crate::sidebar_view::SidebarDisplay::from_ui(&old.ui),
                ..FrameModel::default()
            };
            let mut sb = SidebarState::default();
            sb.view.display = crate::sidebar_view::SidebarDisplay::from_ui(&old.ui);
            Self {
                model,
                sb,
                session: crate::session::Session::default(),
                panel_ui: crate::panel::PanelUi::default(),
                bar_detail: None,
                help_registry: Arc::new(crate::help::pages::registry_logged(old)),
            }
        }

        fn reload(&mut self, old: &Config, new: &Config) -> super::ReloadOutcome {
            self.apply_plan(&plan(old, new), new)
        }

        fn apply_plan(&mut self, plan: &ReloadPlan, new: &Config) -> super::ReloadOutcome {
            let mut ctx = ReloadCtx {
                model: &mut self.model,
                sb: &mut self.sb,
                session: &self.session,
                panel_ui: &mut self.panel_ui,
                bar_detail: &mut self.bar_detail,
                new_cfg: new,
                locale: None,
                help_registry: &mut self.help_registry,
                panel_cols_pref: None,
            };
            apply(plan, &mut ctx)
        }
    }

    /// One applied-check per Live hook: the representative key's edit (old
    /// config is the default) and an assertion that `apply` produced that
    /// hook's effect. A hook whose effect is the caller's (the outcome
    /// requests) is checked through the request it returns.
    type Check = fn(&Fixture, &super::ReloadOutcome, &Config, &Config) -> bool;

    #[test]
    fn every_live_hook_applies_its_representative_key() {
        let rows: &[(ReloadHook, &str, Check)] = &[
            (
                ReloadHook::Sidebar,
                "[ui]\nsidebar_detail_fields = [\"stash\", \"branch\"]\n",
                |fx, _, _, new| {
                    fx.model.sidebar_display.detail_fields == new.ui.detail_entries_resolved()
                },
            ),
            (
                ReloadHook::Weather,
                "[weather]\nenabled = true\nlocation = \"Reykjavik\"\n",
                |fx, _, _, _| fx.model.weather_cfg.location == "Reykjavik",
            ),
            (
                ReloadHook::Theme,
                "[theme]\naccent = \"#123456\"\n",
                |fx, out, _, new| out.bars_dirty && fx.model.accent == new.accent_rgb(),
            ),
            (
                ReloadHook::Bars,
                "[usage]\nenabled = false\n",
                |fx, out, old, new| {
                    // `[usage]` re-arms its poll through the schedule too.
                    out.bars_dirty
                        && !fx.model.usage_cfg.enabled
                        && out.reschedule
                        && ScheduleConfig::from_config(old) != ScheduleConfig::from_config(new)
                },
            ),
            (
                ReloadHook::Calendar,
                "[calendar]\nweek_start = \"sunday\"\n",
                |fx, out, _, _| out.dirty && fx.panel_ui.docs.calendar.loaded.contains(&(2026, 11)),
            ),
            (
                ReloadHook::Schedule,
                "[ci]\npoll_interval_secs = 900\n",
                |_, out, old, new| {
                    out.reschedule
                        && ScheduleConfig::from_config(old).ci_every_slots
                            != ScheduleConfig::from_config(new).ci_every_slots
                },
            ),
            (
                ReloadHook::Schedule,
                "[bars]\nclock_format = \"%H:%M:%S\"\n",
                |fx, out, _, new| {
                    out.bars_dirty
                        && fx.model.bars.clock_format == new.bars.clock_format
                        && out.reschedule
                        && ScheduleConfig::from_config(new).clock_period_secs == 1
                },
            ),
            (
                ReloadHook::Notifications,
                "[notifications.push]\ntopic = \"thegn-reload-test\"\n",
                |fx, out, _, _| {
                    fx.model.notification_delivery_configured
                        && out.notifications.as_ref().is_some_and(delivery_configured)
                },
            ),
            (
                ReloadHook::Panel,
                "[panel]\nsections = [\"pr\", \"changes\"]\n",
                |fx, out, _, new| {
                    fx.panel_ui.order == crate::panel::resolve_order(new)
                        && fx.panel_ui.order.first() == Some(&crate::panel::Section::Pr)
                        && out.panel_width
                            == Some(PanelWidthCfg {
                                normal_cols: new.panel.width,
                                half_ratio: new.panel.half_ratio,
                            })
                },
            ),
            (
                ReloadHook::Keymap,
                "keymap_preset = \"vscode\"\n",
                |fx, out, _, _| {
                    // Help + cheatsheet follow the keymap: the panel points at
                    // the rebuilt registry (the loop below checks it is new).
                    out.panel_width.is_some()
                        && fx
                            .panel_ui
                            .help
                            .reg
                            .as_ref()
                            .is_some_and(|reg| Arc::ptr_eq(reg, &fx.help_registry))
                },
            ),
            (
                ReloadHook::Media,
                "[media]\nshow_art = false\n",
                |fx, out, _, _| out.restart_media && !fx.panel_ui.media.art_visible(),
            ),
            (
                ReloadHook::Automation,
                "[automations]\nenabled = true\n",
                |_, out, _, _| out.automations,
            ),
            (
                ReloadHook::Metrics,
                "[metrics]\ninterval_secs = 9.0\n\n[[metrics.targets]]\nname = \"local\"\nurl = \"http://127.0.0.1:9100/metrics\"\n",
                |_, out, _, new| {
                    out.metrics.as_ref().is_some_and(|m| {
                        m.interval_secs == new.metrics.interval_secs
                            && m.targets.len() == new.metrics.targets.len()
                            && m.targets.len() == 1
                    }) && !out.monitor
                },
            ),
            (
                ReloadHook::Monitor,
                "[monitor]\ndefault_style = \"line\"\n",
                |_, out, _, _| out.monitor && out.metrics.is_none(),
            ),
        ];
        for (hook, edit, check) in rows {
            let old = Config::default();
            let new = cfg(edit);
            let plan = plan(&old, &new);
            assert!(
                plan.runs(*hook),
                "{edit:?} must classify under {hook:?}: {plan:?}"
            );
            let mut fx = Fixture::new(&old);
            // Seed from the old config as the live panel does: the default
            // docs carry a UTC home, which differs from the effective (local)
            // home zone on any non-UTC machine and would make the calendar
            // edit a home change that drops the loaded months.
            fx.panel_ui.docs.calendar =
                crate::calendar_docs::CalendarDocs::from_config(&old.calendar, None);
            fx.panel_ui.docs.calendar.loaded.insert((2026, 11));
            let help_before = Arc::clone(&fx.help_registry);
            let out = fx.apply_plan(&plan, &new);
            assert!(
                check(&fx, &out, &old, &new),
                "{hook:?} did not apply {edit:?}: {out:?}"
            );
            assert_eq!(
                !Arc::ptr_eq(&fx.help_registry, &help_before),
                *hook == ReloadHook::Keymap,
                "{hook:?}: the help registry rebuilds exactly for keymap edits"
            );
        }
    }

    #[test]
    fn a_restart_only_edit_requests_no_hook_work() {
        let old = Config::default();
        let new = cfg("[daemon]\nenabled = false\n");
        let mut fx = Fixture::new(&old);
        let out = fx.reload(&old, &new);
        assert!(!out.reschedule);
        assert!(out.notifications.is_none());
        assert!(!out.restart_media);
        assert!(!out.automations);
        assert!(out.panel_width.is_none());
        assert!(out.metrics.is_none());
        assert!(!out.monitor);
        assert!(!out.dirty);
    }

    #[test]
    fn unrelated_edits_leave_metrics_and_monitor_alone() {
        let old = Config::default();
        for edit in [
            "[theme]\naccent = \"#123456\"\n",
            "[monitor]\nproc_rows = 3\n",
            "[monitor]\nprocesses = false\n",
        ] {
            let new = cfg(edit);
            let mut fx = Fixture::new(&old);
            let out = fx.reload(&old, &new);
            assert!(out.metrics.is_none(), "{edit:?}");
            assert!(!out.monitor, "{edit:?}");
        }
    }

    #[test]
    fn removing_every_metrics_target_hands_back_an_empty_section() {
        let old =
            cfg("[[metrics.targets]]\nname = \"local\"\nurl = \"http://127.0.0.1:9100/metrics\"\n");
        let new = Config::default();
        let mut fx = Fixture::new(&old);
        let out = fx.reload(&old, &new);
        assert!(out.metrics.as_ref().is_some_and(|m| m.targets.is_empty()));
    }

    #[test]
    fn a_truncated_plan_runs_every_hook() {
        let old = Config::default();
        let new = Config::default();
        let plan = plan_keys(vec![
            thegn_core::config::reload_class::TRUNCATED.to_string(),
        ]);
        let mut fx = Fixture::new(&old);
        let help_before = Arc::clone(&fx.help_registry);
        let out = fx.apply_plan(&plan, &new);
        assert!(out.reschedule);
        assert!(out.notifications.is_some());
        assert!(out.restart_media);
        assert!(out.automations);
        assert!(out.panel_width.is_some());
        assert!(out.metrics.is_some());
        assert!(out.monitor);
        assert!(out.bars_dirty && out.dirty);
        assert!(!Arc::ptr_eq(&fx.help_registry, &help_before));
        assert!(app_host_runs(&plan));
        assert!(plugins_restart_runs(&plan));
    }

    #[test]
    fn gate_predicates() {
        let keys = |list: &[&str]| plan_keys(list.iter().map(|k| k.to_string()).collect());
        // Profile overlays fold into the effective notification config.
        assert!(notifications_runs(&keys(&["profile"])));
        assert!(notifications_runs(&keys(&[
            "profiles.work.notifications.active_mode"
        ])));
        assert!(notifications_runs(&keys(&["notifications.active_mode"])));
        assert!(!notifications_runs(&keys(&["theme.accent"])));
        // App tabs follow apps, plugins and observe.
        assert!(app_host_runs(&keys(&["apps.default_tab"])));
        assert!(app_host_runs(&keys(&["plugins"])));
        assert!(app_host_runs(&keys(&["observe.enabled"])));
        assert!(!app_host_runs(&keys(&["theme.accent"])));
        // The plugin host restarts for plugin specs and NextUse edits (its
        // dispatcher holds the config), never for a Live-only edit.
        assert!(plugins_restart_runs(&keys(&["plugins"])));
        assert!(plugins_restart_runs(&keys(&["base_branch"])));
        assert!(!plugins_restart_runs(&keys(&[
            "theme.accent",
            "ui.sidebar_width"
        ])));
        assert!(!plugins_restart_runs(&keys(&["daemon.enabled"])));
        assert!(automations_run(&keys(&["base_branch"])));
        assert!(!automations_run(&keys(&["theme.accent"])));
        assert!(!delivery_configured(&Config::default().notifications));
    }

    #[test]
    fn weather_location_change_polls_once_and_clears_the_reading() {
        let old = cfg("[weather]\nenabled = true\nlocation = \"Oslo\"\n");
        let new = cfg("[weather]\nenabled = true\nlocation = \"Reykjavik\"\n");
        let mut fx = Fixture::new(&old);
        fx.model.weather = Some(snapshot());
        let out = fx.reload(&old, &new);
        assert_eq!(
            out.weather_poll,
            Some(WeatherPoll {
                cfg: new.weather.clone(),
                locale: None,
            })
        );
        assert!(out.bars_dirty);
        assert!(fx.model.weather.is_none(), "stale reading cleared");
        assert_eq!(fx.model.weather_cfg.location, "Reykjavik");
    }

    #[test]
    fn unchanged_weather_does_not_poll() {
        let old = cfg("[weather]\nenabled = true\nlocation = \"Oslo\"\n[bars]\n");
        let new =
            cfg("[weather]\nenabled = true\nlocation = \"Oslo\"\n[daemon]\nenabled = false\n");
        let mut fx = Fixture::new(&old);
        fx.model.weather = Some(snapshot());
        let out = fx.reload(&old, &new);
        assert!(out.weather_poll.is_none());
        assert!(fx.model.weather.is_some(), "reading kept");
        assert!(!out.dirty, "a restart-only key runs no extracted hook");
    }

    #[test]
    fn changed_detail_fields_reach_the_model_after_reload() {
        let old = Config::default();
        let new = cfg("[ui]\nsidebar_detail_fields = [\"stash\", \"branch\"]\n");
        let mut fx = Fixture::new(&old);
        let before = fx.model.sidebar_display.detail_fields.clone();
        let out = fx.reload(&old, &new);
        assert!(out.dirty);
        let after = fx.model.sidebar_display.detail_fields.clone();
        assert_ne!(after, before);
        assert_eq!(after, new.ui.detail_entries_resolved());
        assert_eq!(fx.sb.view.display.detail_fields, after);
    }

    #[test]
    fn theme_accent_reaches_the_model() {
        let old = Config::default();
        let new = cfg("[theme]\naccent = \"#123456\"\n");
        let mut fx = Fixture::new(&old);
        let out = fx.reload(&old, &new);
        assert!(out.bars_dirty);
        assert_eq!(fx.model.accent, new.accent_rgb());
    }

    #[test]
    fn calendar_reload_keeps_events_when_home_is_unchanged() {
        let old = Config::default();
        let new = cfg("[calendar]\nweek_start = \"sunday\"\n");
        let plan = plan(&old, &new);
        let mut fx = Fixture::new(&old);
        // Seed from the old config as the live panel does: the default docs
        // carry a UTC home, which differs from the effective (local) home
        // zone on any non-UTC machine and would make this a home change.
        fx.panel_ui.docs.calendar =
            crate::calendar_docs::CalendarDocs::from_config(&old.calendar, None);
        fx.panel_ui.docs.calendar.loaded.insert((2026, 11));
        let mut ctx = ReloadCtx {
            model: &mut fx.model,
            sb: &mut fx.sb,
            session: &fx.session,
            panel_ui: &mut fx.panel_ui,
            bar_detail: &mut fx.bar_detail,
            new_cfg: &new,
            locale: None,
            help_registry: &mut fx.help_registry,
            panel_cols_pref: None,
        };
        assert!(plan.runs(ReloadHook::Calendar), "{plan:?}");
        let out = apply(&plan, &mut ctx);
        assert!(out.dirty);
        assert!(fx.panel_ui.docs.calendar.loaded.contains(&(2026, 11)));
    }

    #[test]
    fn focus_override_release_rules() {
        let keys = |list: &[&str]| list.iter().map(|k| k.to_string()).collect::<Vec<String>>();
        assert!(!releases_focus_override(
            None,
            &keys(&["ui.sidebar_focus_detail"])
        ));
        assert!(releases_focus_override(
            Some(FocusDetail::Cursor),
            &keys(&["ui.sidebar_focus_detail"])
        ));
        assert!(releases_focus_override(
            Some(FocusDetail::Off),
            &keys(&["ui.sidebar_detail_fields"])
        ));
        assert!(!releases_focus_override(
            Some(FocusDetail::Cursor),
            &keys(&["ui.sidebar_detail_fields"])
        ));
        assert!(!releases_focus_override(
            Some(FocusDetail::Off),
            &keys(&["ui.sidebar_width"])
        ));
    }
}
