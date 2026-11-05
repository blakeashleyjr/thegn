//! Synthetic fixture and trace preview through the production app renderer.
use super::trace::Trace;
use anyhow::{Context, Result, bail};
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tg_kit::ratatui::{buffer::Buffer, layout::Rect};
use tg_kit::{AppTile, InputEvent, InputResult, Key, Modifiers, Theme};
use thegn_core::plugin_app::AppView;
use thegn_plugin_ui::UiEngine;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, Args)]
pub struct Options {
    /// Synthetic fixture JSON (a `{"view": AppView, "inputs": [...]}` document); "gallery" selects the bundled widget gallery.
    #[arg(long, required_unless_present_any = ["replay", "plugin_dir"], conflicts_with = "replay")]
    fixture: Option<PathBuf>,
    /// Replay a full synthetic trace recorded with `--fixture ... --record ... --include-content`. Redacted traces cannot be replayed.
    #[arg(long)]
    replay: Option<PathBuf>,
    /// Write a NEW private (0600, create-new) trace after preview exits; an existing file is never modified.
    #[arg(long, conflicts_with = "replay")]
    record: Option<PathBuf>,
    /// Include frame payloads in the recorded trace (synthetic fixtures only).
    #[arg(long, requires = "fixture")]
    include_content: bool,
    /// Render a JSON screen snapshot without opening a terminal.
    #[arg(long)]
    headless: bool,
    /// Terminal size as COLSxROWS.
    #[arg(long, default_value = "120x36", value_parser = dimensions)]
    size: (u16, u16),
    /// Start the resident app plugin whose plugin.toml is in this directory and preview its first app view (needs --headless in this release).
    #[arg(long, value_name = "DIR", conflicts_with_all = ["fixture", "replay", "record", "include_content"], requires = "headless")]
    plugin_dir: Option<PathBuf>,
    /// With --plugin-dir: how long to wait for the plugin's first valid app view, in milliseconds (100..=30000).
    #[arg(long, default_value_t = super::live::DEFAULT_WAIT_MS, value_parser = super::live::wait_ms, requires = "plugin_dir")]
    wait_ms: u64,
}
fn dimensions(value: &str) -> std::result::Result<(u16, u16), String> {
    let (w, h) = value.split_once('x').ok_or("size must be COLSxROWS")?;
    let (w, h) = (
        w.parse::<u16>().map_err(|_| "invalid width")?,
        h.parse::<u16>().map_err(|_| "invalid height")?,
    );
    if w == 0 || h == 0 || w > 512 || h > 256 {
        return Err("size must fit 1..512 columns and 1..256 rows".into());
    }
    Ok((w, h))
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScriptInput {
    #[serde(default)]
    key: String,
    #[serde(default)]
    ctrl: bool,
    #[serde(default)]
    alt: bool,
    #[serde(default)]
    shift: bool,
    #[serde(default)]
    paste: Option<String>,
    #[serde(default)]
    resize: Option<(u16, u16)>,
}
impl ScriptInput {
    fn from_event(event: &InputEvent) -> Option<Self> {
        match event {
            InputEvent::Paste(text) => Some(Self {
                paste: Some(text.clone()),
                ..Default::default()
            }),
            InputEvent::Resize(cols, rows) => Some(Self {
                resize: Some((*cols, *rows)),
                ..Default::default()
            }),
            InputEvent::Key { key, modifiers } => {
                let key = match key {
                    Key::Char(c) => c.to_string(),
                    Key::Enter => "enter".into(),
                    Key::Escape => "escape".into(),
                    Key::Tab => "tab".into(),
                    Key::BackTab => "back_tab".into(),
                    Key::Backspace => "backspace".into(),
                    Key::Delete => "delete".into(),
                    Key::Left => "left".into(),
                    Key::Right => "right".into(),
                    Key::Up => "up".into(),
                    Key::Down => "down".into(),
                    Key::Home => "home".into(),
                    Key::End => "end".into(),
                    Key::PageUp => "page_up".into(),
                    Key::PageDown => "page_down".into(),
                    Key::Function(n) => format!("f{n}"),
                };
                Some(Self {
                    key,
                    ctrl: modifiers.ctrl,
                    alt: modifiers.alt,
                    shift: modifiers.shift,
                    paste: None,
                    resize: None,
                })
            }
            _ => None,
        }
    }
    fn event(&self) -> Result<InputEvent> {
        if let Some((cols, rows)) = self.resize {
            if self.paste.is_some()
                || !self.key.is_empty()
                || cols == 0
                || rows == 0
                || cols > 512
                || rows > 256
            {
                bail!("invalid or ambiguous fixture resize");
            }
            return Ok(InputEvent::Resize(cols, rows));
        }
        if let Some(text) = &self.paste {
            if !self.key.is_empty() {
                bail!("fixture input cannot combine a key and paste");
            }
            if text.len() > 32768 {
                bail!("fixture paste exceeds field budget");
            }
            return Ok(InputEvent::Paste(text.clone()));
        }
        let key = match self.key.as_str() {
            "enter" => Key::Enter,
            "escape" => Key::Escape,
            "tab" => Key::Tab,
            "back_tab" => Key::BackTab,
            "backspace" => Key::Backspace,
            "delete" => Key::Delete,
            "left" => Key::Left,
            "right" => Key::Right,
            "up" => Key::Up,
            "down" => Key::Down,
            "home" => Key::Home,
            "end" => Key::End,
            "page_up" => Key::PageUp,
            "page_down" => Key::PageDown,
            other
                if other.starts_with('f')
                    && other.len() > 1
                    && other[1..].bytes().all(|c| c.is_ascii_digit()) =>
            {
                let n = other[1..]
                    .parse::<u8>()
                    .ok()
                    .filter(|n| (1..=35).contains(n))
                    .context("invalid fixture function key")?;
                Key::Function(n)
            }
            other => {
                let mut chars = other.chars();
                let c = chars
                    .next()
                    .filter(|c| !c.is_control())
                    .ok_or_else(|| anyhow::anyhow!("invalid fixture key"))?;
                if chars.next().is_some() {
                    bail!("fixture key must be a named key or one character");
                }
                Key::Char(c)
            }
        };
        Ok(InputEvent::Key {
            key,
            modifiers: Modifiers {
                ctrl: self.ctrl,
                alt: self.alt,
                shift: self.shift,
            },
        })
    }
}
#[derive(Deserialize)]
struct Fixture {
    view: AppView,
    #[serde(default)]
    inputs: Vec<ScriptInput>,
}

fn apply_script(ui: &mut UiEngine, input: &ScriptInput, area: &mut Rect) -> Result<()> {
    match input.event()? {
        InputEvent::Resize(cols, rows) => {
            *area = Rect::new(0, 0, cols, rows);
            ui.prepare(*area);
        }
        event => {
            ui.input(event);
        }
    }
    Ok(())
}

fn read_json(path: &Path) -> Result<Value> {
    use std::io::Read;
    let mut bytes = Vec::new();
    thegn_core::fsperm::open_regular_file_nofollow(path)?
        .take((1 << 20) + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 1 << 20 {
        bail!("preview input exceeds one MiB");
    }
    serde_json::from_slice(&bytes).context("invalid preview JSON")
}
fn record(
    trace: &Arc<Mutex<Trace>>,
    direction: &str,
    session: &str,
    started: Instant,
    frame: &Value,
) {
    if let Err(error) = trace.lock().unwrap_or_else(|e| e.into_inner()).push(
        direction,
        session,
        started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        frame,
    ) {
        tracing::warn!(target: "thegn::plugin::preview", %error, "trace capture stopped at its bound");
    }
}
struct FixtureTile {
    ui: UiEngine,
    dirty: bool,
    trace: Arc<Mutex<Trace>>,
    started: Instant,
}
impl AppTile for FixtureTile {
    fn id(&self) -> &str {
        "preview"
    }
    fn title(&self) -> String {
        "Plugin preview".into()
    }
    fn pump(&mut self) -> bool {
        self.dirty
    }
    fn wants_redraw(&self) -> bool {
        self.dirty
    }
    fn handle_input(&mut self, event: InputEvent) -> InputResult {
        if matches!(&event, InputEvent::Key { key: Key::Char('q'), modifiers } if modifiers.ctrl) {
            return InputResult::Exit;
        }
        if let Some(input) = ScriptInput::from_event(&event) {
            record(
                &self.trace,
                "outgoing",
                "fixture",
                self.started,
                &json!({"method":"on_event","params":{"input":input}}),
            );
        }
        self.dirty = true;
        self.ui.input(event).result
    }
    fn render(&mut self, area: Rect, buffer: &mut Buffer) {
        self.ui.prepare(area);
        self.ui.paint(buffer);
        self.dirty = false;
    }
    fn cursor(&self) -> Option<(u16, u16)> {
        self.ui.cursor()
    }
    fn status_line(&self) -> Option<String> {
        self.ui.status_line()
    }
    fn on_resize(&mut self, w: u16, h: u16) {
        if let Some(input) = ScriptInput::from_event(&InputEvent::Resize(w, h)) {
            record(
                &self.trace,
                "outgoing",
                "fixture",
                self.started,
                &json!({"method":"on_event","params":{"input":input}}),
            );
        }
        self.ui.prepare(Rect::new(0, 0, w, h));
        self.dirty = true;
    }
}
fn synthetic(options: &Options, trace: Arc<Mutex<Trace>>) -> Result<Box<dyn AppTile>> {
    let started = Instant::now();
    let mut ui = UiEngine::new(Theme::prism());
    let mut area = Rect::new(0, 0, options.size.0, options.size.1);
    if let Some(path) = &options.fixture {
        let value = if is_gallery_selector(path) {
            serde_json::from_str(include_str!("gallery.json"))?
        } else {
            read_json(path)?
        };
        let fixture: Fixture = serde_json::from_value(value)?;
        fixture.view.validate().map_err(anyhow::Error::msg)?;
        record(
            &trace,
            "incoming",
            "fixture",
            started,
            &json!({"method":"update","params":{"app":fixture.view}}),
        );
        ui.replace_view(fixture.view).map_err(anyhow::Error::msg)?;
        ui.prepare(area);
        for input in fixture.inputs {
            record(
                &trace,
                "outgoing",
                "fixture",
                started,
                &json!({"method":"on_event","params":{"input":input}}),
            );
            apply_script(&mut ui, &input, &mut area)?;
        }
    } else if let Some(path) = &options.replay {
        for entry in super::trace::read(path)? {
            if let Some(view) = entry.frame.pointer("/params/app") {
                let view: AppView = serde_json::from_value(view.clone())?;
                view.validate().map_err(anyhow::Error::msg)?;
                ui.replace_view(view).map_err(anyhow::Error::msg)?;
                ui.prepare(area);
            } else if let Some(input) = entry.frame.pointer("/params/input") {
                apply_script(
                    &mut ui,
                    &serde_json::from_value::<ScriptInput>(input.clone())?,
                    &mut area,
                )?;
            }
        }
        if ui.view().is_none() {
            bail!("trace contains no app view");
        }
    } else {
        bail!("provide --fixture or --replay");
    }
    Ok(Box::new(FixtureTile {
        ui,
        dirty: true,
        trace,
        started,
    }))
}

pub(super) fn snapshot_value(tile: &mut dyn AppTile, size: (u16, u16)) -> Value {
    tile.on_resize(size.0, size.1);
    tile.pump();
    let area = Rect::new(0, 0, size.0, size.1);
    let mut buffer = Buffer::empty(area);
    tile.render(area, &mut buffer);
    let lines: Vec<String> = buffer
        .content
        .chunks(size.0 as usize)
        .map(|row| visible_row(row).trim_end().to_string())
        .collect();
    json!({
        "cols": size.0,
        "rows": size.1,
        "lines": lines,
        "cursor": tile.cursor(),
        "status": tile.status_line(),
    })
}

pub(super) fn tile_for_view(view: AppView, size: (u16, u16)) -> Result<Box<dyn AppTile>> {
    view.validate().map_err(anyhow::Error::msg)?;
    let mut ui = UiEngine::new(Theme::prism());
    ui.replace_view(view).map_err(anyhow::Error::msg)?;
    ui.prepare(Rect::new(0, 0, size.0, size.1));
    Ok(Box::new(FixtureTile {
        ui,
        dirty: true,
        trace: Arc::new(Mutex::new(Trace::default())),
        started: Instant::now(),
    }))
}

fn is_gallery_selector(path: &Path) -> bool {
    path.as_os_str() == std::ffi::OsStr::new("gallery")
}

fn visible_row(row: &[tg_kit::ratatui::buffer::Cell]) -> String {
    let mut text = String::new();
    let mut skip_continuation = false;
    let mut skipped_cells = 0;
    for cell in row {
        if skip_continuation {
            skip_continuation = false;
            skipped_cells += 1;
            continue;
        }
        let symbol = cell.symbol();
        text.push_str(symbol);
        skip_continuation = UnicodeWidthStr::width(symbol) > 1;
    }
    // Keep the snapshot's cell count stable while omitting Ratatui's duplicate
    // continuation-cell symbol for a wide grapheme.
    for _ in 0..skipped_cells {
        text.push(' ');
    }
    text
}

fn snapshot(tile: &mut dyn AppTile, size: (u16, u16)) -> Result<()> {
    crate::cmd::emit_json(&snapshot_value(tile, size))
}

pub(super) fn run(options: Options) -> Result<()> {
    if let Some(dir) = &options.plugin_dir {
        return super::live::run_headless(
            dir,
            options.size,
            std::time::Duration::from_millis(options.wait_ms),
        );
    }
    let trace = Arc::new(Mutex::new(if options.record.is_some() {
        Trace::new(options.include_content)
    } else {
        Trace::default()
    }));
    let mut tile = synthetic(&options, trace.clone())?;
    let result = if options.headless {
        snapshot(tile.as_mut(), options.size)
    } else {
        tg_kit::standalone::run(move |_hook| Ok(tile))
    };
    if let Some(path) = &options.record {
        trace.lock().unwrap_or_else(|e| e.into_inner()).save(path)?;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use serde_json::json;

    #[derive(clap::Parser)]
    struct PreviewArgs {
        #[command(flatten)]
        options: Options,
    }

    #[test]
    fn live_options_conflict_with_fixture_replay_and_record_and_require_headless() {
        assert!(PreviewArgs::try_parse_from(["p", "--plugin-dir", "d", "--headless"]).is_ok());
        assert!(PreviewArgs::try_parse_from(["p", "--plugin-dir", "d"]).is_err());
        assert!(
            PreviewArgs::try_parse_from([
                "p",
                "--plugin-dir",
                "d",
                "--headless",
                "--fixture",
                "gallery"
            ])
            .is_err()
        );
        assert!(
            PreviewArgs::try_parse_from(["p", "--plugin-dir", "d", "--headless", "--replay", "t"])
                .is_err()
        );
        assert!(
            PreviewArgs::try_parse_from(["p", "--plugin-dir", "d", "--headless", "--record", "r"])
                .is_err()
        );
        assert!(PreviewArgs::try_parse_from(["p", "--wait-ms", "500"]).is_err());
    }

    #[test]
    fn live_options_are_required_when_no_fixture_or_replay_is_given() {
        assert!(PreviewArgs::try_parse_from(["p", "--headless"]).is_err());
        assert!(PreviewArgs::try_parse_from(["p", "--fixture", "gallery"]).is_ok());
        assert!(PreviewArgs::try_parse_from(["p", "--replay", "t"]).is_ok());
    }

    fn options_for_fixture(path: PathBuf) -> Options {
        Options {
            fixture: Some(path),
            replay: None,
            record: None,
            include_content: false,
            headless: true,
            size: (120, 36),
            plugin_dir: None,
            wait_ms: super::super::live::DEFAULT_WAIT_MS,
        }
    }

    #[test]
    fn interactive_fixture_record_replays_the_same_cells() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("fixture.jsonl");
        let options = Options {
            fixture: Some("gallery".into()),
            replay: None,
            record: None,
            include_content: true,
            headless: true,
            size: (120, 36),
            plugin_dir: None,
            wait_ms: super::super::live::DEFAULT_WAIT_MS,
        };
        let trace = Arc::new(Mutex::new(Trace::new(true)));
        let mut original = synthetic(&options, trace.clone()).unwrap();
        original.on_resize(120, 36);
        original.handle_input(InputEvent::key(Key::Tab));
        original.on_resize(45, 12);
        original.handle_input(InputEvent::key(Key::Char('!')));
        original.on_resize(120, 36);
        trace.lock().unwrap().save(&path).unwrap();
        let replay_options = Options {
            fixture: None,
            replay: Some(path),
            ..options
        };
        let mut replay =
            synthetic(&replay_options, Arc::new(Mutex::new(Trace::default()))).unwrap();
        let area = Rect::new(0, 0, 120, 36);
        let mut left = Buffer::empty(area);
        let mut right = Buffer::empty(area);
        original.render(area, &mut left);
        replay.render(area, &mut right);
        assert_eq!(left, right);
        assert_eq!(original.cursor(), replay.cursor());
    }

    #[test]
    fn dimensions_are_bounded() {
        assert_eq!(dimensions("80x24").unwrap(), (80, 24));
        assert!(dimensions("0x24").is_err());
        assert!(dimensions("900x24").is_err());
        assert!(dimensions("80x257").is_err());
    }

    #[test]
    fn wide_grapheme_snapshot_skips_ratatui_continuation_cells() {
        let area = Rect::new(0, 0, 4, 1);
        let mut buffer = Buffer::empty(area);
        buffer.set_string(0, 0, "漢x", tg_kit::ratatui::style::Style::default());
        assert_eq!(visible_row(&buffer.content), "漢x  ");
    }

    #[test]
    fn gallery_selector_does_not_capture_a_dot_relative_file() {
        assert!(is_gallery_selector(Path::new("gallery")));
        assert!(!is_gallery_selector(Path::new("./gallery")));
        assert!(!is_gallery_selector(Path::new("/tmp/gallery")));
    }

    #[test]
    fn gallery_uses_valid_production_schema() {
        let fixture: Fixture = serde_json::from_str(include_str!("gallery.json")).unwrap();
        fixture.view.validate().unwrap();
        let mut ui = UiEngine::new(Theme::prism());
        ui.replace_view(fixture.view).unwrap();
        for size in [(120, 36), (24, 8), (1, 1)] {
            let area = Rect::new(0, 0, size.0, size.1);
            ui.prepare(area);
            let mut buffer = Buffer::empty(area);
            ui.paint(&mut buffer);
        }
    }

    #[test]
    fn fixture_keys_reject_ambiguous_or_oversized_input() {
        assert!(
            ScriptInput {
                key: "left".into(),
                ..Default::default()
            }
            .event()
            .is_ok()
        );
        assert!(
            ScriptInput {
                key: "two characters".into(),
                ..Default::default()
            }
            .event()
            .is_err()
        );
        assert!(
            ScriptInput {
                paste: Some("x".repeat(32769)),
                ..Default::default()
            }
            .event()
            .is_err()
        );
        assert!(
            ScriptInput {
                key: "enter".into(),
                paste: Some("text".into()),
                ..Default::default()
            }
            .event()
            .is_err()
        );
        assert!(
            ScriptInput {
                resize: Some((0, 24)),
                ..Default::default()
            }
            .event()
            .is_err()
        );
    }

    #[test]
    fn headless_snapshot_has_rows_lines_cursor_and_status() {
        let mut tile = synthetic(
            &Options {
                fixture: Some("gallery".into()),
                replay: None,
                record: None,
                include_content: false,
                headless: true,
                size: (120, 36),
                plugin_dir: None,
                wait_ms: super::super::live::DEFAULT_WAIT_MS,
            },
            Arc::new(Mutex::new(Trace::default())),
        )
        .unwrap();
        let value = snapshot_value(tile.as_mut(), (120, 36));
        assert_eq!(value["lines"].as_array().unwrap().len(), 36);
        assert_eq!(value["cols"], 120);
        let lines = value["lines"].as_array().unwrap();
        assert!(
            lines
                .iter()
                .any(|line| line.as_str().unwrap().contains("Locations")
                    || line.as_str().unwrap().contains("Grapheme editor"))
        );
        assert!(
            value["status"]
                .as_str()
                .unwrap()
                .starts_with("Synthetic fixture")
        );
        assert!(value.get("cursor").is_some());
    }

    #[test]
    fn fixture_requires_valid_view_before_rendering() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bad.json");
        std::fs::write(&path, json!({"view":{"state_scope":"bad","revision":1,"status":"","root":{"kind":"text","id":"","title":"Bad","text":"no"}},"inputs":[]} ).to_string()).unwrap();
        assert!(
            synthetic(
                &options_for_fixture(path),
                Arc::new(Mutex::new(Trace::default()))
            )
            .is_err()
        );
    }

    #[test]
    fn replay_without_a_view_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("input-only.jsonl");
        let mut trace = Trace::new(true);
        trace
            .push(
                "outgoing",
                "fixture",
                0,
                &json!({"method":"on_event","params":{"input":{"key":"tab"}}}),
            )
            .unwrap();
        trace.save(&path).unwrap();
        let options = Options {
            fixture: None,
            replay: Some(path),
            record: None,
            include_content: false,
            headless: true,
            size: (40, 10),
            plugin_dir: None,
            wait_ms: super::super::live::DEFAULT_WAIT_MS,
        };
        let error = match synthetic(&options, Arc::new(Mutex::new(Trace::default()))) {
            Ok(_) => panic!("viewless replay should fail"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("trace contains no app view"));
    }

    #[test]
    fn run_refuses_to_overwrite_an_existing_recording() {
        let tmp = tempfile::tempdir().unwrap();
        let existing = tmp.path().join("existing.jsonl");
        let bytes = b"keep this recording";
        std::fs::write(&existing, bytes).unwrap();
        let result = run(Options {
            fixture: Some("gallery".into()),
            replay: None,
            record: Some(existing.clone()),
            include_content: true,
            headless: true,
            size: (40, 10),
            plugin_dir: None,
            wait_ms: super::super::live::DEFAULT_WAIT_MS,
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read(existing).unwrap(), bytes);
    }
}
