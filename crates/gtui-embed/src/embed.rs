//! `ObserveTile` — the host-facing [`tg_kit::AppTile`] for the Observe app.
//!
//! It constructs the [`ObserveApp`] view-model (which spawns the off-loop query
//! engine), then each frame: `pump()` drains engine results, `render()` lays the
//! dashboard onto the tile area and dispatches each panel to its `gtui_render`
//! renderer. A panic in app code is caught and shown in-panel — the host stays
//! healthy.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::{ErrorKind, Read};
use std::panic;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use gtui_app::app::{ObserveApp, PanelState};
use gtui_core::dashboard::{Dashboard, builtin_host_dashboard};
use gtui_core::datasource::{DataSource, TimeRange};
use gtui_core::frame::{Coverage, FieldType, Frame};
use gtui_query::host::HostSource;
use gtui_query::loki::LokiSource;
use gtui_query::prometheus::PrometheusSource;
use gtui_render::layout::GRID_COLS;
use tg_kit::input::{InputEvent, InputResult, Key};
use tg_kit::ratatui::buffer::Buffer;
use tg_kit::ratatui::layout::Rect;
use tg_kit::ratatui::style::Style;
use tg_kit::ratatui::widgets::{Block, Borders, Paragraph, Widget};
use tg_kit::theme::{Theme, Tok};
use tg_kit::tile::{AppTile, ChangeHook};
use thegn_core::config_observe::ObserveConfig;

pub struct ObserveTile {
    app: ObserveApp,
    theme: Theme,
    is_panicked: bool,
    needs_redraw: bool,
}

impl ObserveTile {
    /// Build the tile. `hook` wakes the host loop when the engine has new data;
    /// `cfg` supplies the dashboard source + refresh cadence; `rt` is the host's
    /// tokio runtime handle the query engine runs on.
    pub fn new(
        hook: ChangeHook,
        cfg: &ObserveConfig,
        rt: tokio::runtime::Handle,
        theme: Theme,
        sampler_thread_start: impl FnOnce() + Send + 'static,
    ) -> Result<Self, ObserveTileError> {
        let mut source_ids = HashSet::from(["host".to_string()]);
        if !cfg.prometheus.base_url.trim().is_empty() {
            source_ids.insert("prometheus".to_string());
        }
        if !cfg.loki.base_url.trim().is_empty() {
            source_ids.insert("loki".to_string());
        }
        let dashboard = load_dashboard(cfg, &source_ids)?;
        // The built-in host datasource is always present (local metrics, zero
        // config). Prometheus/Loki register only when an endpoint is configured;
        // tokens resolve through thegn's `env:`/`file:` indirection.
        let mut sources: HashMap<String, Arc<dyn DataSource>> = HashMap::new();
        sources.insert(
            "host".to_string(),
            Arc::new(HostSource::try_new_with(sampler_thread_start)?),
        );
        if !cfg.prometheus.base_url.trim().is_empty() {
            let token =
                thegn_core::config::expand_env_ref(&cfg.prometheus.token).unwrap_or_default();
            sources.insert(
                "prometheus".to_string(),
                Arc::new(PrometheusSource::new(
                    cfg.prometheus.base_url.clone(),
                    token,
                )),
            );
        }
        if !cfg.loki.base_url.trim().is_empty() {
            let token = thegn_core::config::expand_env_ref(&cfg.loki.token).unwrap_or_default();
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
        let refresh = refresh_period(cfg.refresh_interval_secs);

        let app = ObserveApp::new(rt, dashboard, sources, time_range, refresh, hook);
        Ok(Self {
            app,
            theme,
            is_panicked: false,
            needs_redraw: true,
        })
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

    fn on_visible(&mut self, visible: bool) {
        self.app.set_visible(visible);
        if visible {
            self.needs_redraw = true;
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
                Tok::Red,
                &self.theme,
            );
            return;
        }

        let app = &self.app;
        let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            render_dashboard(app, area, buf, &self.theme);
        }));
        if result.is_err() {
            self.is_panicked = true;
        }
    }
}

/// Maximum custom dashboard size accepted at the load boundary (1 MiB).
pub const MAX_DASHBOARD_BYTES: u64 = 1024 * 1024;
/// Maximum number of panels accepted in a custom dashboard.
const MAX_DASHBOARD_PANELS: usize = 64;
/// Maximum number of query targets in one panel.
const MAX_PANEL_TARGETS: usize = 16;
/// Maximum bytes in a target expression.
const MAX_EXPR_BYTES: usize = 4096;
/// Maximum bytes in dashboard and panel labels, descriptions, and ref ids.
const MAX_LABEL_BYTES: usize = 256;
/// Refresh cadence is clamped to [1 second, one hour].
const MAX_REFRESH_SECS: u64 = 3600;
/// Maximum vertical extent of a panel grid.
const MAX_DASHBOARD_ROWS: u32 = 10_000;

fn refresh_period(secs: u64) -> Duration {
    Duration::from_secs(secs.clamp(1, MAX_REFRESH_SECS))
}

#[derive(Debug, PartialEq, Eq)]
pub enum DashboardLoadError {
    Missing(String),
    Denied(String),
    TooLarge(String),
    InvalidToml(String),
    Invalid(String),
    UnknownSource(String),
    AmbiguousSource,
}

impl fmt::Display for DashboardLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(path) => write!(f, "dashboard missing: {path}"),
            Self::Denied(path) => write!(f, "dashboard denied: {path}"),
            Self::TooLarge(path) => write!(
                f,
                "dashboard too large (limit {MAX_DASHBOARD_BYTES} bytes): {path}"
            ),
            Self::InvalidToml(path) => write!(f, "dashboard invalid TOML: {path}"),
            Self::Invalid(reason) => write!(f, "dashboard invalid: {reason}"),
            Self::UnknownSource(id) => write!(f, "dashboard references unknown datasource: {id}"),
            Self::AmbiguousSource => write!(
                f,
                "dashboard datasource required when multiple sources are configured"
            ),
        }
    }
}

impl std::error::Error for DashboardLoadError {}

#[derive(Debug)]
pub enum ObserveTileError {
    Dashboard(DashboardLoadError),
    Source(gtui_query::host::HostSourceSpawnError),
}

impl fmt::Display for ObserveTileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dashboard(error) => fmt::Display::fmt(error, f),
            Self::Source(error) => fmt::Display::fmt(error, f),
        }
    }
}

impl std::error::Error for ObserveTileError {}

impl From<gtui_query::host::HostSourceSpawnError> for ObserveTileError {
    fn from(error: gtui_query::host::HostSourceSpawnError) -> Self {
        Self::Source(error)
    }
}

impl From<DashboardLoadError> for ObserveTileError {
    fn from(error: DashboardLoadError) -> Self {
        Self::Dashboard(error)
    }
}

/// Load and validate the exact snapshot that will be handed to the query engine.
fn load_dashboard(
    cfg: &ObserveConfig,
    source_ids: &HashSet<String>,
) -> Result<Dashboard, DashboardLoadError> {
    let path = cfg.dashboard_path.trim();
    if path.is_empty() {
        return Ok(builtin_host_dashboard());
    }
    let expanded = expand_tilde(path);
    let metadata = std::fs::metadata(&expanded)
        .map_err(|error| classify_open_error(&expanded, error.kind()))?;
    if !metadata.is_file() {
        return Err(DashboardLoadError::Invalid("not a regular file".into()));
    }
    let file = std::fs::File::open(&expanded)
        .map_err(|error| classify_open_error(&expanded, error.kind()))?;
    let mut bytes = Vec::new();
    file.take(MAX_DASHBOARD_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| classify_open_error(&expanded, error.kind()))?;
    if bytes.len() as u64 > MAX_DASHBOARD_BYTES {
        return Err(DashboardLoadError::TooLarge(expanded));
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| DashboardLoadError::InvalidToml(expanded.clone()))?;
    let parsed: toml::Value =
        toml::from_str(text).map_err(|_| DashboardLoadError::InvalidToml(expanded.clone()))?;
    let dashboard: Dashboard = parsed
        .try_into()
        .map_err(|_| DashboardLoadError::Invalid("schema".into()))?;
    validate_dashboard(&dashboard, source_ids)?;
    Ok(dashboard)
}

fn classify_open_error(path: &str, kind: ErrorKind) -> DashboardLoadError {
    match kind {
        ErrorKind::NotFound => DashboardLoadError::Missing(path.to_string()),
        ErrorKind::PermissionDenied => DashboardLoadError::Denied(path.to_string()),
        _ => DashboardLoadError::Invalid(format!("cannot read {path}")),
    }
}

fn validate_dashboard(
    dashboard: &Dashboard,
    source_ids: &HashSet<String>,
) -> Result<(), DashboardLoadError> {
    if dashboard.panels.len() > MAX_DASHBOARD_PANELS {
        return Err(DashboardLoadError::Invalid(format!(
            "panel count exceeds {MAX_DASHBOARD_PANELS}"
        )));
    }
    if dashboard.title.len() > MAX_LABEL_BYTES
        || dashboard
            .description
            .as_ref()
            .is_some_and(|description| description.len() > MAX_LABEL_BYTES)
    {
        return Err(DashboardLoadError::Invalid(format!(
            "dashboard title or description exceeds {MAX_LABEL_BYTES} bytes"
        )));
    }
    let mut ids = HashSet::new();
    for panel in &dashboard.panels {
        if !ids.insert(panel.id) {
            return Err(DashboardLoadError::Invalid(format!(
                "duplicate panel id {}",
                panel.id
            )));
        }
        if panel.title.len() > MAX_LABEL_BYTES {
            return Err(DashboardLoadError::Invalid(format!(
                "panel {} title exceeds {MAX_LABEL_BYTES} bytes",
                panel.id
            )));
        }
        let right = panel.grid_pos.x.checked_add(panel.grid_pos.w);
        let bottom = panel.grid_pos.y.checked_add(panel.grid_pos.h);
        if right.is_none_or(|right| right > GRID_COLS)
            || bottom.is_none_or(|bottom| bottom > MAX_DASHBOARD_ROWS)
            || panel.grid_pos.w == 0
            || panel.grid_pos.h == 0
        {
            return Err(DashboardLoadError::Invalid(format!(
                "panel {} has invalid grid bounds",
                panel.id
            )));
        }
        if !matches!(
            panel.panel_type.as_str(),
            "timeseries" | "stat" | "table" | "logs"
        ) {
            return Err(DashboardLoadError::Invalid(format!(
                "panel {} has unsupported renderer",
                panel.id
            )));
        }
        if panel.datasource.is_empty() {
            if source_ids.len() != 1 {
                return Err(DashboardLoadError::AmbiguousSource);
            }
        } else if !source_ids.contains(&panel.datasource) {
            return Err(DashboardLoadError::UnknownSource(panel.datasource.clone()));
        }
        if panel.targets.len() > MAX_PANEL_TARGETS {
            return Err(DashboardLoadError::Invalid(format!(
                "panel {} target count exceeds {MAX_PANEL_TARGETS}",
                panel.id
            )));
        }
        let mut ref_ids = HashSet::new();
        for target in &panel.targets {
            if target.expr.len() > MAX_EXPR_BYTES {
                return Err(DashboardLoadError::Invalid(format!(
                    "panel {} expression exceeds {MAX_EXPR_BYTES} bytes",
                    panel.id
                )));
            }
            if target.ref_id.len() > MAX_LABEL_BYTES {
                return Err(DashboardLoadError::Invalid(format!(
                    "panel {} ref_id exceeds {MAX_LABEL_BYTES} bytes",
                    panel.id
                )));
            }
            if !ref_ids.insert(&target.ref_id) {
                return Err(DashboardLoadError::Invalid(format!(
                    "panel {} has duplicate ref_id",
                    panel.id
                )));
            }
        }
    }
    Ok(())
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
fn render_dashboard(app: &ObserveApp, area: Rect, buf: &mut Buffer, theme: &Theme) {
    let panels = &app.dashboard.panels;
    if panels.is_empty() {
        block_msg(
            area,
            buf,
            "Observe",
            "No panels in dashboard",
            Tok::Dim,
            theme,
        );
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
            theme,
            app.panel_window(panel.id).map(|window| {
                [
                    window.from.timestamp() as f64
                        + f64::from(window.from.timestamp_subsec_nanos()) / 1_000_000_000.0,
                    window.to.timestamp() as f64
                        + f64::from(window.to.timestamp_subsec_nanos()) / 1_000_000_000.0,
                ]
            }),
        );
    }
}

/// Draw one panel: its renderer when data is `Ready`, else a loading/error/empty
/// placeholder titled with the panel name.
fn render_panel(
    kind: &str,
    title: &str,
    state: &PanelState,
    rect: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    window: Option<[f64; 2]>,
) {
    match state {
        PanelState::Loading => {
            block_msg(rect, buf, title, "loading…", Tok::Faint, theme);
        }
        PanelState::Error(e) => {
            block_msg(rect, buf, title, e, Tok::Red, theme);
        }
        PanelState::Ready(targets) => {
            render_targets(kind, title, targets, rect, buf, theme, window);
        }
    }
}

/// Render each target and each returned series in stable datasource order.
fn render_targets(
    kind: &str,
    title: &str,
    targets: &[gtui_app::engine::TargetUpdate],
    rect: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    window: Option<[f64; 2]>,
) {
    let mut entries = Vec::new();
    for target in targets {
        match &target.result {
            Err(message) => entries.push((
                format!("{title} · {}", target.ref_id),
                None,
                Some(message.as_str()),
            )),
            Ok(frames) if frames.is_empty() => entries.push((
                format!("{title} · {}", target.ref_id),
                None,
                Some("no data"),
            )),
            Ok(frames) => {
                for (index, frame) in frames.iter().enumerate().take(32) {
                    let mut label = if frames.len() == 1 {
                        format!("{title} · {}", target.ref_id)
                    } else {
                        let series = frame
                            .fields
                            .iter()
                            .find(|field| field.ty != FieldType::Time)
                            .map(|field| field.name.as_str())
                            .filter(|name| !name.is_empty())
                            .map(str::to_owned)
                            .unwrap_or_else(|| (index + 1).to_string());
                        format!("{title} · {} · {series}", target.ref_id)
                    };
                    if let Coverage::Partial { .. } = frame.coverage {
                        label.push_str(" · partial");
                    }
                    if matches!(
                        frame.coverage,
                        Coverage::Partial {
                            retained_from: None
                        }
                    ) {
                        entries.push((label, None, Some("no samples retained yet")));
                    } else {
                        entries.push((label, Some(frame), None));
                    }
                }
                if frames.len() > 32 {
                    entries.push((
                        format!("{title} · {}", target.ref_id),
                        None,
                        Some("series limit is 32"),
                    ));
                }
            }
        }
    }
    if entries.is_empty() {
        block_msg(rect, buf, title, "no data", Tok::Faint, theme);
        return;
    }
    for (index, (label, frame, message)) in entries.iter().enumerate() {
        let y0 = rect.y + (u32::from(rect.height) * index as u32 / entries.len() as u32) as u16;
        let y1 =
            rect.y + (u32::from(rect.height) * (index + 1) as u32 / entries.len() as u32) as u16;
        let height = y1.saturating_sub(y0);
        if height == 0 {
            continue;
        }
        let item_rect = Rect::new(rect.x, y0, rect.width, height);
        if let Some(message) = message {
            block_msg(item_rect, buf, label, message, Tok::Faint, theme);
        } else if let Some(frame) = frame {
            dispatch_renderer(kind, label, frame, item_rect, buf, theme, window);
        }
    }
}

/// Panel-type → `gtui_render` widget dispatch. Unknown types draw a hint.
fn dispatch_renderer(
    kind: &str,
    title: &str,
    frame: &Frame,
    rect: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    window: Option<[f64; 2]>,
) {
    use gtui_render::{logs, stat, table, timeseries};
    let colors = render_colors(theme);
    match kind {
        "timeseries" => timeseries::TimeseriesRenderer::render_in_window(
            frame,
            title,
            bounds_for(frame),
            window,
            rect.width as usize,
            &colors,
        )
        .render(rect, buf),
        "stat" => stat::StatRenderer::render(frame, title, &colors).render(rect, buf),
        "table" => table::TableRenderer::render(
            frame,
            title,
            rect.height as usize,
            rect.width as usize,
            &colors,
        )
        .render(rect, buf),
        "logs" => logs::LogsRenderer::render(
            frame,
            title,
            rect.height as usize,
            rect.width as usize,
            &colors,
        )
        .render(rect, buf),
        other => block_msg(
            rect,
            buf,
            title,
            &format!("unknown panel type '{other}'"),
            Tok::Amber,
            theme,
        ),
    }
}

fn render_colors(theme: &Theme) -> gtui_render::RenderColors {
    use gtui_render::RenderColors;
    RenderColors {
        value: theme.color(Tok::Accent),
        header: theme.color(Tok::Amber),
        // Logs use the host's normal text token so readable log content keeps
        // the same contrast as other panel values.
        text: theme.color(Tok::Text),
        series: theme.color(Tok::Green),
        border: theme.color(Tok::Border),
        title: theme.color(Tok::Text),
    }
}

/// Compute fallback `[x_min, x_max, y_min, y_max]` bounds: X uses sample indices
/// when no matching time field exists, and Y uses the value range with 10%
/// padding. Empty/flat data falls back to a unit box so the canvas never divides
/// by zero.
fn bounds_for(frame: &Frame) -> [f64; 4] {
    let values: &[f64] = frame
        .fields
        .iter()
        .find(|f| f.ty == gtui_core::frame::FieldType::Float64)
        .map_or(&[][..], |f| f.floats());
    let x_max = (values.len().saturating_sub(1)).max(1) as f64;
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for &v in values {
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
fn block_msg(rect: Rect, buf: &mut Buffer, title: &str, msg: &str, token: Tok, theme: &Theme) {
    let color = theme.color(token);
    let w = Paragraph::new(msg.to_string())
        .style(Style::default().fg(color))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.color(Tok::Border)))
                .title_style(Style::default().fg(theme.color(Tok::Text)))
                .title(title.to_string()),
        );
    w.render(rect, buf);
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtui_core::frame::{Coverage, Field, FieldType};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_FILE: AtomicUsize = AtomicUsize::new(0);

    fn dashboard_file(contents: &[u8]) -> String {
        let dir = std::env::temp_dir().join(format!(
            "gtui-dashboard-{}-{}.toml",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir(&dir).unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.join("dashboard.toml");
        std::fs::write(&path, contents).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn remove_dashboard_file(path: &str) {
        std::fs::remove_dir_all(std::path::Path::new(path).parent().unwrap()).unwrap();
    }

    fn config_for(path: String) -> ObserveConfig {
        ObserveConfig {
            dashboard_path: path,
            ..ObserveConfig::default()
        }
    }

    fn valid_toml(id: u32, datasource: &str) -> String {
        format!(
            "title = \"test\"\n[[panels]]\nid = {id}\ntitle = \"p\"\ntype = \"stat\"\ndatasource = \"{datasource}\"\n[panels.grid_pos]\nx = 0\ny = 0\nw = 24\nh = 8\n[[panels.targets]]\nexpr = \"x\"\nref_id = \"A\"\n"
        )
    }

    fn make_source_ids(names: &[&str]) -> HashSet<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn custom_dashboard_load_failures_are_distinct_and_never_fall_back() {
        let source_ids = make_source_ids(&["host"]);
        let missing =
            std::env::temp_dir().join(format!("missing-dashboard-{}.toml", std::process::id()));
        let mut cfg = config_for(missing.to_string_lossy().into_owned());
        assert!(matches!(
            load_dashboard(&cfg, &source_ids),
            Err(DashboardLoadError::Missing(_))
        ));
        // This is a classifier-only check: load_dashboard exercises the same
        // classification through filesystem errors, but chmod-000 is unreliable
        // under root and across platforms.
        assert!(matches!(
            classify_open_error("private/path", ErrorKind::PermissionDenied),
            DashboardLoadError::Denied(_)
        ));

        let oversized_path = dashboard_file(&vec![b' '; MAX_DASHBOARD_BYTES as usize + 1]);
        cfg.dashboard_path = oversized_path.clone();
        assert!(matches!(
            load_dashboard(&cfg, &source_ids),
            Err(DashboardLoadError::TooLarge(_))
        ));
        remove_dashboard_file(&oversized_path);

        let invalid_path = dashboard_file(b"not = [toml");
        cfg.dashboard_path = invalid_path.clone();
        assert!(matches!(
            load_dashboard(&cfg, &source_ids),
            Err(DashboardLoadError::InvalidToml(_))
        ));
        remove_dashboard_file(&invalid_path);

        let schema_path = dashboard_file(b"title = \"missing panels\"");
        cfg.dashboard_path = schema_path.clone();
        assert!(matches!(
            load_dashboard(&cfg, &source_ids),
            Err(DashboardLoadError::Invalid(reason)) if reason == "schema"
        ));
        remove_dashboard_file(&schema_path);

        let duplicate_path = dashboard_file(
            format!("{}{}", valid_toml(1, "host"), valid_toml(1, "host")).as_bytes(),
        );
        cfg.dashboard_path = duplicate_path.clone();
        assert!(
            matches!(load_dashboard(&cfg, &source_ids), Err(DashboardLoadError::Invalid(message)) if message.contains("duplicate panel id"))
        );
        remove_dashboard_file(&duplicate_path);

        let unknown_path = dashboard_file(valid_toml(1, "typo").as_bytes());
        cfg.dashboard_path = unknown_path.clone();
        assert!(
            matches!(load_dashboard(&cfg, &source_ids), Err(DashboardLoadError::UnknownSource(id)) if id == "typo")
        );
        remove_dashboard_file(&unknown_path);

        let ambiguous_path = dashboard_file(valid_toml(1, "").as_bytes());
        cfg.dashboard_path = ambiguous_path.clone();
        assert!(matches!(
            load_dashboard(&cfg, &make_source_ids(&["host", "prometheus"])),
            Err(DashboardLoadError::AmbiguousSource)
        ));
        remove_dashboard_file(&ambiguous_path);
    }

    #[test]
    fn custom_dashboard_rejects_unbounded_targets_labels_and_expressions() {
        let source_ids = make_source_ids(&["host"]);
        let too_many_targets = format!(
            "{}{}",
            valid_toml(1, "host"),
            (0..MAX_PANEL_TARGETS)
                .map(|index| format!(
                    "[[panels.targets]]\nexpr = \"x{index}\"\nref_id = \"{index}\"\n"
                ))
                .collect::<String>()
        );
        let oversized_expr = valid_toml(1, "host").replace(
            "expr = \"x\"",
            &format!("expr = \"{}\"", "x".repeat(MAX_EXPR_BYTES + 1)),
        );
        let oversized_title = valid_toml(1, "host").replace(
            "title = \"p\"",
            &format!("title = \"{}\"", "x".repeat(MAX_LABEL_BYTES + 1)),
        );
        let duplicate_ref = valid_toml(1, "host").replace(
            "ref_id = \"A\"",
            "ref_id = \"A\"\n[[panels.targets]]\nexpr = \"y\"\nref_id = \"A\"",
        );

        for (toml, expected) in [
            (too_many_targets, "target count"),
            (oversized_expr, "expression"),
            (oversized_title, "title"),
            (duplicate_ref, "duplicate ref_id"),
        ] {
            let path = dashboard_file(toml.as_bytes());
            let result = load_dashboard(&config_for(path.clone()), &source_ids);
            assert!(
                matches!(result, Err(DashboardLoadError::Invalid(reason)) if reason.contains(expected))
            );
            remove_dashboard_file(&path);
        }
    }

    #[cfg(unix)]
    #[test]
    fn denied_dashboard_load_is_visible_when_filesystem_denies_open() {
        use std::os::unix::fs::DirBuilderExt;

        let dir = std::env::temp_dir().join(format!(
            "gtui-dashboard-denied-{}-{}",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        let path = dir.join("dashboard.toml");
        std::fs::write(&path, valid_toml(1, "host")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&path).is_ok() {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            std::fs::remove_dir_all(&dir).unwrap();
            return;
        }
        assert!(matches!(
            load_dashboard(
                &config_for(path.to_string_lossy().into_owned()),
                &make_source_ids(&["host"])
            ),
            Err(DashboardLoadError::Denied(_))
        ));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refresh_period_clamps_both_ends() {
        assert_eq!(refresh_period(0), Duration::from_secs(1));
        assert_eq!(
            refresh_period(u64::MAX),
            Duration::from_secs(MAX_REFRESH_SECS)
        );
    }

    #[test]
    fn custom_dashboard_rejects_non_regular_files_before_open() {
        let cfg = config_for(std::env::temp_dir().to_string_lossy().into_owned());
        assert!(matches!(
            load_dashboard(&cfg, &make_source_ids(&["host"])),
            Err(DashboardLoadError::Invalid(reason)) if reason == "not a regular file"
        ));
    }

    #[test]
    fn error_placeholder_uses_the_supplied_semantic_red() {
        let mut theme = Theme::prism();
        theme.red = (1, 2, 3);
        theme.border = (4, 5, 6);
        theme.text = (7, 8, 9);
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 5));
        render_panel(
            "stat",
            "CPU",
            &PanelState::Error("failed".into()),
            buf.area,
            &mut buf,
            &theme,
            None,
        );
        assert!(
            buf.content
                .iter()
                .any(|cell| cell.fg == theme.color(Tok::Red))
        );
        assert_eq!(buf.cell((0, 0)).unwrap().fg, theme.color(Tok::Border));
        assert_eq!(buf.cell((2, 0)).unwrap().fg, theme.color(Tok::Text));
    }

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
        dispatch_renderer("nope", "P", &f, buf.area, &mut buf, &Theme::prism(), None);
        // A hint was drawn (top-left border of the block).
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), "┌");
    }

    #[test]
    fn render_panel_shows_loading_then_data() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 5));
        render_panel(
            "stat",
            "CPU",
            &PanelState::Loading,
            buf.area,
            &mut buf,
            &Theme::prism(),
            None,
        );
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), "┌");

        let f = Frame::new(vec![Field::new("v", FieldType::Float64, vec![42.0])]);
        let mut buf2 = Buffer::empty(Rect::new(0, 0, 20, 5));
        render_panel(
            "stat",
            "CPU",
            &PanelState::Ready(vec![gtui_app::engine::TargetUpdate {
                ref_id: "A".into(),
                result: Ok(vec![f]),
            }]),
            buf2.area,
            &mut buf2,
            &Theme::prism(),
            None,
        );
        // StatRenderer drew the value "42.00".
        assert_eq!(buf2.cell((1, 1)).unwrap().symbol(), "4");
    }

    #[test]
    fn mixed_target_results_render_identity_success_errors_and_empty_results() {
        let frame = Frame::new(vec![Field::new("v", FieldType::Float64, vec![42.0])]);
        let targets = vec![
            gtui_app::engine::TargetUpdate {
                ref_id: "A".into(),
                result: Ok(vec![frame]),
            },
            gtui_app::engine::TargetUpdate {
                ref_id: "B".into(),
                result: Err("source failed".into()),
            },
            gtui_app::engine::TargetUpdate {
                ref_id: "C".into(),
                result: Ok(Vec::new()),
            },
        ];
        for kind in ["timeseries", "stat", "table", "logs"] {
            let mut buf = Buffer::empty(Rect::new(0, 0, 40, 18));
            render_targets(
                kind,
                "Panel",
                &targets,
                buf.area,
                &mut buf,
                &Theme::prism(),
                None,
            );
            let mut rendered = String::new();
            for y in 0..buf.area.height {
                for x in 0..buf.area.width {
                    rendered.push_str(buf.cell((x, y)).unwrap().symbol());
                }
            }
            assert!(rendered.contains("A"), "{kind} omitted successful target");
            assert!(rendered.contains("B"), "{kind} omitted failed target");
            assert!(rendered.contains("C"), "{kind} omitted empty target");
            assert!(
                rendered.contains("source failed"),
                "{kind} omitted target error"
            );
        }
    }

    #[test]
    fn multi_series_target_titles_use_derived_series_names() {
        let frames = ["up{job=\"a\"}", "up{job=\"b\"}"]
            .into_iter()
            .map(|name| {
                Frame::new(vec![
                    Field::new("time", FieldType::Time, vec![1.0]),
                    Field::new(name, FieldType::Float64, vec![42.0]),
                ])
            })
            .collect();
        let targets = vec![gtui_app::engine::TargetUpdate {
            ref_id: "A".into(),
            result: Ok(frames),
        }];
        let area = Rect::new(0, 0, 70, 12);
        let mut buf = Buffer::empty(area);
        render_targets(
            "stat",
            "Panel",
            &targets,
            area,
            &mut buf,
            &Theme::prism(),
            None,
        );
        let rendered = buf
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");
        assert!(rendered.contains("A · up{job=\"a\"}"));
        assert!(rendered.contains("A · up{job=\"b\"}"));
    }

    #[test]
    fn requested_window_keeps_early_samples_in_the_left_half() {
        let frame = Frame::new(vec![
            Field::new("time", FieldType::Time, vec![0.0, 1.0]),
            Field::new("v", FieldType::Float64, vec![0.0, 1.0]),
        ]);
        let targets = vec![gtui_app::engine::TargetUpdate {
            ref_id: "A".into(),
            result: Ok(vec![frame]),
        }];
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 10));
        let theme = Theme::prism();
        render_targets(
            "timeseries",
            "Panel",
            &targets,
            buf.area,
            &mut buf,
            &theme,
            Some([0.0, 2.0]),
        );
        let series = theme.color(Tok::Green);
        assert!(buf.content.iter().any(|cell| cell.fg == series));
        assert!(!buf.content.iter().enumerate().any(|(index, cell)| {
            let x = index as u16 % buf.area.width;
            x >= 20 && cell.fg == series
        }));
    }

    #[test]
    fn each_renderer_uses_host_content_and_border_tokens() {
        let mut theme = Theme::prism();
        theme.accent = (12, 34, 56);
        theme.amber = (23, 45, 67);
        theme.green = (34, 56, 78);
        theme.text = (45, 67, 89);
        theme.border = (56, 78, 90);
        let cases = [
            (
                "stat",
                Field::new("v", FieldType::Float64, vec![1.0, 2.0]),
                Tok::Accent,
            ),
            (
                "table",
                Field::new("v", FieldType::Float64, vec![1.0]),
                Tok::Amber,
            ),
            (
                "logs",
                Field::new_str("line", vec!["log line".into()]),
                Tok::Text,
            ),
            (
                "timeseries",
                Field::new("time", FieldType::Time, vec![0.0, 1.0]),
                Tok::Green,
            ),
        ];
        for (kind, field, token) in cases {
            let mut fields = vec![field];
            if kind == "timeseries" {
                fields.push(Field::new("v", FieldType::Float64, vec![0.0, 1.0]));
            }
            let frame = Frame::new(fields);
            let area = Rect::new(0, 0, 40, 10);
            let mut buf = Buffer::empty(area);
            render_targets(
                kind,
                "Panel",
                &[gtui_app::engine::TargetUpdate {
                    ref_id: "A".into(),
                    result: Ok(vec![frame]),
                }],
                area,
                &mut buf,
                &theme,
                None,
            );
            assert!(
                buf.content.iter().any(|cell| cell.fg == theme.color(token)),
                "{kind} omitted {token:?}"
            );
            assert!(
                buf.content
                    .iter()
                    .any(|cell| cell.fg == theme.color(Tok::Border)),
                "{kind} omitted border token"
            );
        }
    }

    #[test]
    fn render_targets_marks_partial_frames_and_explains_empty_retention() {
        let partial = Frame::new(vec![Field::new("v", FieldType::Float64, vec![42.0])])
            .with_coverage(Coverage::Partial {
                retained_from: Some(1000.0),
            });
        let complete = Frame::new(vec![Field::new("v", FieldType::Float64, vec![7.0])]);
        let empty_partial = Frame::new(vec![
            Field::new("time", FieldType::Time, Vec::new()),
            Field::new("v", FieldType::Float64, Vec::new()),
        ])
        .with_coverage(Coverage::Partial {
            retained_from: None,
        });
        let targets = vec![
            gtui_app::engine::TargetUpdate {
                ref_id: "A".into(),
                result: Ok(vec![partial]),
            },
            gtui_app::engine::TargetUpdate {
                ref_id: "B".into(),
                result: Ok(vec![complete]),
            },
            gtui_app::engine::TargetUpdate {
                ref_id: "C".into(),
                result: Ok(vec![empty_partial]),
            },
        ];
        let mut buf = Buffer::empty(Rect::new(0, 0, 48, 18));
        render_targets(
            "timeseries",
            "Panel",
            &targets,
            buf.area,
            &mut buf,
            &Theme::prism(),
            None,
        );
        let mut rendered = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                rendered.push_str(buf.cell((x, y)).unwrap().symbol());
            }
        }
        assert!(rendered.contains("A · partial"));
        assert!(rendered.contains("B"));
        assert!(!rendered.contains("B · partial"));
        assert!(rendered.contains("C · partial"));
        assert!(rendered.contains("no samples retained yet"));
    }

    #[test]
    fn fmt_dur_humanizes() {
        assert_eq!(fmt_dur(45), "45s");
        assert_eq!(fmt_dur(900), "15m");
        assert_eq!(fmt_dur(7200), "2h");
    }
}
