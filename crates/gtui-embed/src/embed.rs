//! `ObserveTile` — the host-facing [`sz_kit::AppTile`] for the Observe app.
//!
//! It constructs the [`ObserveApp`] view-model (which spawns the off-loop query
//! engine), then each frame: `pump()` drains engine results, `render()` lays the
//! dashboard onto the tile area and dispatches each panel to its `gtui_render`
//! renderer. A panic in app code is caught and shown in-panel — the host stays
//! healthy.

use std::collections::HashMap;
use std::panic;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use gtui_app::app::{ObserveApp, PanelState};
use gtui_core::dashboard::{Dashboard, builtin_host_dashboard};
use gtui_core::datasource::{DataSource, TimeRange};
use gtui_core::frame::Frame;
use gtui_query::host::HostSource;
use gtui_query::loki::LokiSource;
use gtui_query::prometheus::PrometheusSource;
use superzej_core::config_observe::ObserveConfig;
use sz_kit::input::{InputEvent, InputResult, Key};
use sz_kit::ratatui::buffer::Buffer;
use sz_kit::ratatui::layout::Rect;
use sz_kit::ratatui::style::{Color, Style};
use sz_kit::ratatui::widgets::{Block, Borders, Paragraph, Widget};
use sz_kit::tile::{AppTile, ChangeHook};

pub struct ObserveTile {
    app: ObserveApp,
    is_panicked: bool,
    needs_redraw: bool,
}

impl ObserveTile {
    /// Build the tile. `hook` wakes the host loop when the engine has new data;
    /// `cfg` supplies the dashboard source + refresh cadence; `rt` is the host's
    /// tokio runtime handle the query engine runs on.
    pub fn new(hook: ChangeHook, cfg: &ObserveConfig, rt: tokio::runtime::Handle) -> Self {
        let dashboard = load_dashboard(cfg);

        // The built-in host datasource is always present (local metrics, zero
        // config). Prometheus/Loki register only when an endpoint is configured;
        // tokens resolve through superzej's `env:`/`file:` indirection.
        let mut sources: HashMap<String, Arc<dyn DataSource>> = HashMap::new();
        sources.insert("host".to_string(), Arc::new(HostSource::new()));
        if !cfg.prometheus.base_url.trim().is_empty() {
            let token =
                superzej_core::config::expand_env_ref(&cfg.prometheus.token).unwrap_or_default();
            sources.insert(
                "prometheus".to_string(),
                Arc::new(PrometheusSource::new(
                    cfg.prometheus.base_url.clone(),
                    token,
                )),
            );
        }
        if !cfg.loki.base_url.trim().is_empty() {
            let token = superzej_core::config::expand_env_ref(&cfg.loki.token).unwrap_or_default();
            sources.insert(
                "loki".to_string(),
                Arc::new(LokiSource::new(cfg.loki.base_url.clone(), token)),
            );
        }

        let to = Utc::now();
        let time_range = TimeRange {
            from: to - chrono::Duration::minutes(15),
            to,
        };
        let refresh = Duration::from_secs(cfg.refresh_interval_secs.max(1));

        let app = ObserveApp::new(rt, dashboard, sources, time_range, refresh, hook);
        Self {
            app,
            is_panicked: false,
            needs_redraw: true,
        }
    }

    /// Scale the query window by `factor` (ending now), clamped to [1m, 24h].
    fn adjust_window(&mut self, factor: f64) {
        let cur = (self.app.time_range.to - self.app.time_range.from)
            .num_seconds()
            .max(60);
        let secs = ((cur as f64 * factor) as i64).clamp(60, 24 * 3600);
        let to = Utc::now();
        let from = to - chrono::Duration::seconds(secs);
        self.app.set_time_range(TimeRange { from, to });
    }
}

/// Compact humanized duration ("45s", "15m", "2h") for the status line.
fn fmt_dur(secs: i64) -> String {
    if secs >= 3600 {
        format!("{}h", secs / 3600)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

impl AppTile for ObserveTile {
    fn id(&self) -> &'static str {
        "observe"
    }

    fn title(&self) -> String {
        if self.is_panicked {
            "Observe (Crashed)".to_string()
        } else {
            "Observe".to_string()
        }
    }

    fn pump(&mut self) -> bool {
        if self.is_panicked {
            return false;
        }
        let result = panic::catch_unwind(panic::AssertUnwindSafe(|| self.app.tick()));
        match result {
            Ok(changed) => {
                if changed {
                    self.needs_redraw = true;
                }
                changed
            }
            Err(_) => {
                self.is_panicked = true;
                self.needs_redraw = true;
                true
            }
        }
    }

    fn wants_redraw(&self) -> bool {
        self.needs_redraw
    }

    fn handle_input(&mut self, event: InputEvent) -> InputResult {
        if self.is_panicked {
            return InputResult::Ignored;
        }
        let InputEvent::Key { key, .. } = event else {
            return InputResult::Ignored;
        };
        match key {
            // Leave the app tab (host returns focus to `work`).
            Key::Escape => return InputResult::Exit,
            // Manual refresh — non-blocking nudge to the engine.
            Key::Char('r') | Key::Char('R') => self.app.request_requery(),
            // Narrow / widen the query window (re-queries immediately).
            Key::Char('[') => self.adjust_window(0.5),
            Key::Char(']') => self.adjust_window(2.0),
            _ => return InputResult::Ignored,
        }
        self.needs_redraw = true;
        InputResult::Consumed
    }

    fn status_line(&self) -> Option<String> {
        if self.is_panicked {
            return Some("observe · crashed (host healthy)".to_string());
        }
        let secs = (self.app.time_range.to - self.app.time_range.from)
            .num_seconds()
            .max(0);
        Some(format!(
            "observe · window {} · [r] refresh  [ [ / ] ] window  [esc] back",
            fmt_dur(secs)
        ))
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        self.needs_redraw = false;

        if self.is_panicked {
            block_msg(
                area,
                buf,
                "Observe (Panic)",
                "The Observe panel crashed. Host remains healthy.",
                Color::Red,
            );
            return;
        }

        let app = &self.app;
        let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            render_dashboard(app, area, buf);
        }));
        if result.is_err() {
            self.is_panicked = true;
        }
    }
}

/// Load the dashboard named by `cfg`: the built-in host dashboard when no path
/// is set, else a TOML file — falling back to the built-in on any read/parse
/// error so the tab never fails to open.
fn load_dashboard(cfg: &ObserveConfig) -> Dashboard {
    let path = cfg.dashboard_path.trim();
    if path.is_empty() {
        return builtin_host_dashboard();
    }
    let expanded = expand_tilde(path);
    match std::fs::read_to_string(&expanded)
        .ok()
        .and_then(|s| toml::from_str::<Dashboard>(&s).ok())
    {
        Some(d) => d,
        None => builtin_host_dashboard(),
    }
}

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}/{rest}");
    }
    path.to_string()
}

/// Lay the dashboard onto `area` and dispatch each panel to its renderer.
fn render_dashboard(app: &ObserveApp, area: Rect, buf: &mut Buffer) {
    let panels = &app.dashboard.panels;
    if panels.is_empty() {
        block_msg(area, buf, "Observe", "No panels in dashboard", Color::Gray);
        return;
    }
    let rows = gtui_render::layout::grid_rows(panels);
    for panel in panels {
        let rect = gtui_render::layout::panel_rect(area, &panel.grid_pos, rows);
        if rect.width < 2 || rect.height < 2 {
            continue;
        }
        render_panel(
            &panel.panel_type,
            &panel.title,
            app.panel_state(panel.id),
            rect,
            buf,
        );
    }
}

/// Draw one panel: its renderer when data is `Ready`, else a loading/error/empty
/// placeholder titled with the panel name.
fn render_panel(kind: &str, title: &str, state: &PanelState, rect: Rect, buf: &mut Buffer) {
    let frame = match state {
        PanelState::Loading => {
            block_msg(rect, buf, title, "loading…", Color::DarkGray);
            return;
        }
        PanelState::Error(e) => {
            block_msg(rect, buf, title, e, Color::Red);
            return;
        }
        PanelState::Ready(frames) => match frames.first() {
            Some(f) => f,
            None => {
                block_msg(rect, buf, title, "no data", Color::DarkGray);
                return;
            }
        },
    };
    dispatch_renderer(kind, title, frame, rect, buf);
}

/// Panel-type → `gtui_render` widget dispatch. Unknown types draw a hint.
fn dispatch_renderer(kind: &str, title: &str, frame: &Frame, rect: Rect, buf: &mut Buffer) {
    use gtui_render::{logs, stat, table, timeseries};
    match kind {
        "timeseries" => timeseries::TimeseriesRenderer::render(frame, title, bounds_for(frame))
            .render(rect, buf),
        "stat" => stat::StatRenderer::render(frame, title).render(rect, buf),
        "table" => table::TableRenderer::render(frame, title).render(rect, buf),
        "logs" => logs::LogsRenderer::render(frame, title).render(rect, buf),
        other => block_msg(
            rect,
            buf,
            title,
            &format!("unknown panel type '{other}'"),
            Color::Yellow,
        ),
    }
}

/// Compute `[x_min, x_max, y_min, y_max]` bounds for a timeseries frame: X is the
/// sample index, Y is the value field's range with 10% padding. Falls back to a
/// unit box for empty/flat data so the canvas never divides by zero.
fn bounds_for(frame: &Frame) -> [f64; 4] {
    let values: Vec<f64> = frame
        .fields
        .iter()
        .find(|f| f.ty == gtui_core::frame::FieldType::Float64)
        .map(|f| f.floats())
        .unwrap_or_default();
    let x_max = (values.len().saturating_sub(1)).max(1) as f64;
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for &v in &values {
        lo = lo.min(v);
        hi = hi.max(v);
    }
    if !lo.is_finite() || !hi.is_finite() {
        return [0.0, x_max, 0.0, 1.0];
    }
    if (hi - lo).abs() < f64::EPSILON {
        hi = lo + 1.0;
    }
    let pad = (hi - lo) * 0.1;
    [0.0, x_max, lo - pad, hi + pad]
}

/// A bordered block with a centered message — the loading/error/empty state.
fn block_msg(rect: Rect, buf: &mut Buffer, title: &str, msg: &str, color: Color) {
    let w = Paragraph::new(msg.to_string())
        .style(Style::default().fg(color))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title.to_string()),
        );
    w.render(rect, buf);
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtui_core::frame::{Field, FieldType};

    #[test]
    fn bounds_for_pads_a_normal_series() {
        let f = Frame::new(vec![Field::new("v", FieldType::Float64, vec![10.0, 20.0])]);
        let b = bounds_for(&f);
        assert_eq!(b[0], 0.0);
        assert_eq!(b[1], 1.0); // x_max = len-1
        assert!(b[2] < 10.0 && b[3] > 20.0); // padded past the data
    }

    #[test]
    fn bounds_for_handles_empty_and_flat() {
        let empty = Frame::new(vec![Field::new("v", FieldType::Float64, vec![])]);
        assert_eq!(bounds_for(&empty), [0.0, 1.0, 0.0, 1.0]);
        let flat = Frame::new(vec![Field::new(
            "v",
            FieldType::Float64,
            vec![5.0, 5.0, 5.0],
        )]);
        let b = bounds_for(&flat);
        assert!(b[3] > b[2], "flat series still yields a non-zero y range");
    }

    #[test]
    fn dispatch_unknown_type_does_not_panic() {
        let f = Frame::new(vec![Field::new("v", FieldType::Float64, vec![1.0])]);
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 5));
        dispatch_renderer("nope", "P", &f, buf.area, &mut buf);
        // A hint was drawn (top-left border of the block).
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), "┌");
    }

    #[test]
    fn render_panel_shows_loading_then_data() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 5));
        render_panel("stat", "CPU", &PanelState::Loading, buf.area, &mut buf);
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), "┌");

        let f = Frame::new(vec![Field::new("v", FieldType::Float64, vec![42.0])]);
        let mut buf2 = Buffer::empty(Rect::new(0, 0, 20, 5));
        render_panel(
            "stat",
            "CPU",
            &PanelState::Ready(vec![f]),
            buf2.area,
            &mut buf2,
        );
        // StatRenderer drew the value "42.00".
        assert_eq!(buf2.cell((1, 1)).unwrap().symbol(), "4");
    }

    #[test]
    fn fmt_dur_humanizes() {
        assert_eq!(fmt_dur(45), "45s");
        assert_eq!(fmt_dur(900), "15m");
        assert_eq!(fmt_dur(7200), "2h");
    }
}
