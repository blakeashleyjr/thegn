//! Config-reload hooks: the swap groups the UI loop runs when the config
//! watcher publishes a new [`Config`], gated on the
//! [`ReloadPlan`](thegn_core::config::reload_class::ReloadPlan) so a hook runs
//! only when one of its keys changed (or the diff was incomplete).
//!
//! Extracted from the reload handler in `run.rs`. Every hook runs ON the loop
//! inside the existing `WakeSource::Config` wake: it rebuilds models or queues
//! work, never does I/O. The one off-loop job (a weather re-poll) is returned in
//! [`ReloadOutcome::weather_poll`] for the caller to spawn, which keeps this
//! module runtime-free and unit-testable.

use crate::chrome::FrameModel;
use crate::handlers::sidebar_persist::SidebarState;
use thegn_core::config::reload_class::{ReloadHook, ReloadPlan};
use thegn_core::config::{Config, FocusDetail, WeatherConfig};

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
}

/// Run the extracted hooks whose keys changed.
pub(crate) fn apply(plan: &ReloadPlan, ctx: &mut ReloadCtx<'_>) -> ReloadOutcome {
    let mut out = ReloadOutcome::default();
    if plan.runs(ReloadHook::Sidebar) {
        sidebar(plan, ctx);
        out.dirty = true;
    }
    // The accent lives in `[theme]` but is cached beside the bar configs.
    if plan.runs(ReloadHook::Bars) || plan.runs(ReloadHook::Theme) {
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
    out
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
    use super::{ReloadCtx, WeatherPoll, apply, releases_focus_override};
    use crate::chrome::FrameModel;
    use crate::handlers::sidebar_persist::SidebarState;
    use thegn_core::config::reload_class::{ReloadHook, plan};
    use thegn_core::config::{Config, FocusDetail};
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
            }
        }

        fn reload(&mut self, old: &Config, new: &Config) -> super::ReloadOutcome {
            let plan = plan(old, new);
            let mut ctx = ReloadCtx {
                model: &mut self.model,
                sb: &mut self.sb,
                session: &self.session,
                panel_ui: &mut self.panel_ui,
                bar_detail: &mut self.bar_detail,
                new_cfg: new,
                locale: None,
            };
            apply(&plan, &mut ctx)
        }
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
