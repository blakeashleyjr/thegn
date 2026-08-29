//! Off-loop provider for user themes.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use notify::{Event, EventKind, RecursiveMode, Watcher, recommended_watcher};
use termwiz::terminal::TerminalWaker;
use tokio::sync::mpsc as tokio_mpsc;

use thegn_core::theme_user::UserTheme;

pub const MAX_THEME_FILE_BYTES: usize = 64 * 1024;
const MAX_THEME_FILES: usize = 256;

#[derive(Debug)]
enum Request {
    Scan,
    Import(PathBuf),
    Save(UserTheme),
    Apply { preset: String, theme: UserTheme },
}

#[derive(Debug)]
enum Work {
    Request(Request),
    Changed,
}

#[derive(Debug)]
pub(crate) enum ThemeStoreResult {
    Catalog {
        themes: Vec<UserTheme>,
        warnings: Vec<String>,
    },
    Imported(Result<UserTheme, String>),
    Saved(Result<UserTheme, String>),
    Applied(Result<UserTheme, String>),
}

pub(crate) struct ThemeStore {
    request: mpsc::Sender<Work>,
    results: tokio_mpsc::UnboundedReceiver<ThemeStoreResult>,
}

impl ThemeStore {
    pub(crate) fn spawn(waker: TerminalWaker, config_path: PathBuf) -> Self {
        let (request, request_rx) = mpsc::channel();
        let (result_tx, results) = tokio_mpsc::unbounded_channel();
        let themes_dir = thegn_core::util::xdg_config_home().join("thegn/themes");
        let watcher_tx = request.clone();
        std::thread::Builder::new()
            .name("theme-store".into())
            .spawn(move || {
                worker(
                    request_rx,
                    result_tx,
                    waker,
                    themes_dir,
                    config_path,
                    watcher_tx,
                )
            })
            .expect("theme store worker thread");
        Self { request, results }
    }

    pub(crate) fn scan(&self) {
        let _ = self.request.send(Work::Request(Request::Scan));
    }

    pub(crate) fn import(&self, path: PathBuf) {
        let _ = self.request.send(Work::Request(Request::Import(path)));
    }

    pub(crate) fn save(&self, theme: UserTheme) {
        let _ = self.request.send(Work::Request(Request::Save(theme)));
    }

    pub(crate) fn apply(&self, preset: String, theme: UserTheme) {
        let _ = self
            .request
            .send(Work::Request(Request::Apply { preset, theme }));
    }

    pub(crate) fn try_recv(&mut self) -> Option<ThemeStoreResult> {
        self.results.try_recv().ok()
    }
}

fn worker(
    request_rx: mpsc::Receiver<Work>,
    result_tx: tokio_mpsc::UnboundedSender<ThemeStoreResult>,
    waker: TerminalWaker,
    themes_dir: PathBuf,
    config_path: PathBuf,
    watcher_tx: mpsc::Sender<Work>,
) {
    let mut watcher = recommended_watcher(move |result: notify::Result<Event>| {
        if result.is_ok_and(|event| {
            matches!(
                event.kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
            )
        }) {
            let _ = watcher_tx.send(Work::Changed);
        }
    })
    .ok();
    if let Some(w) = watcher.as_mut() {
        let _ = w.watch(&themes_dir, RecursiveMode::NonRecursive);
    }

    publish_catalog(&themes_dir, &result_tx, &waker);
    while let Ok(work) = request_rx.recv() {
        match work {
            Work::Changed => {
                while request_rx.recv_timeout(Duration::from_millis(200)).is_ok() {}
                publish_catalog(&themes_dir, &result_tx, &waker);
            }
            Work::Request(Request::Scan) => publish_catalog(&themes_dir, &result_tx, &waker),
            Work::Request(Request::Import(path)) => {
                let result = read_bounded(&path).and_then(|bytes| {
                    thegn_core::theme_import::import_gogh(&bytes).map_err(|e| e.to_string())
                });
                publish(&result_tx, &waker, ThemeStoreResult::Imported(result));
            }
            Work::Request(Request::Save(theme)) => {
                let result = write_theme(&themes_dir, &theme).map(|_| theme);
                publish(&result_tx, &waker, ThemeStoreResult::Saved(result));
            }
            Work::Request(Request::Apply { preset, theme }) => {
                let result = write_theme(&themes_dir, &theme)
                    .and_then(|_| write_config(&config_path, &preset, &theme))
                    .map(|_| theme);
                publish(&result_tx, &waker, ThemeStoreResult::Applied(result));
            }
        }
    }
}

fn publish_catalog(
    dir: &Path,
    tx: &tokio_mpsc::UnboundedSender<ThemeStoreResult>,
    waker: &TerminalWaker,
) {
    let (themes, warnings) = scan_dir(dir);
    publish(tx, waker, ThemeStoreResult::Catalog { themes, warnings });
}

fn publish(
    tx: &tokio_mpsc::UnboundedSender<ThemeStoreResult>,
    waker: &TerminalWaker,
    result: ThemeStoreResult,
) {
    let _ = tx.send(result);
    let _ = waker.wake();
}

fn scan_dir(dir: &Path) -> (Vec<UserTheme>, Vec<String>) {
    let mut themes = Vec::new();
    let mut warnings = Vec::new();
    if let Err(e) = std::fs::create_dir_all(dir) {
        warnings.push(format!("theme directory unavailable: {e}"));
        return (themes, warnings);
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        warnings.push("theme directory could not be read".into());
        return (themes, warnings);
    };
    for entry in entries.flatten().take(MAX_THEME_FILES) {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("toml")
            || !entry.file_type().is_ok_and(|kind| kind.is_file())
        {
            continue;
        }
        match read_bounded(&path).and_then(|bytes| {
            let text = std::str::from_utf8(&bytes).map_err(|_| "not UTF-8".to_string())?;
            UserTheme::from_toml(text).map_err(|e| e.to_string())
        }) {
            Ok(theme) => themes.push(theme),
            Err(e) => warnings.push(format!("{}: {e}", path.display())),
        }
    }
    themes.sort_by(|a, b| a.meta.name.cmp(&b.meta.name));
    (themes, warnings)
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !metadata.file_type().is_file() {
        return Err("theme source is not a regular file".into());
    }
    let size = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
    if size > MAX_THEME_FILE_BYTES {
        return Err(format!(
            "theme file is {size} bytes; maximum is {MAX_THEME_FILE_BYTES}"
        ));
    }
    std::fs::read(path).map_err(|e| e.to_string())
}

fn valid_slug(name: &str) -> Result<String, String> {
    let slug = thegn_core::util::slugify(name);
    if slug.is_empty() || slug.len() > 80 {
        return Err("theme name must produce a non-empty slug of at most 80 characters".into());
    }
    Ok(slug)
}

fn write_theme(dir: &Path, theme: &UserTheme) -> Result<(), String> {
    theme.validate().map_err(|e| e.to_string())?;
    let slug = valid_slug(&theme.meta.name)?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let destination = dir.join(format!("{slug}.toml"));
    let temporary = destination.with_extension("toml.tmp");
    let text = theme.to_toml().map_err(|e| e.to_string())?;
    std::fs::write(&temporary, text).map_err(|e| e.to_string())?;
    std::fs::rename(&temporary, &destination).map_err(|e| e.to_string())
}

fn write_config(path: &Path, preset: &str, theme: &UserTheme) -> Result<(), String> {
    use toml_edit::{DocumentMut, Item, Table, value};
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    let mut doc = text
        .parse::<DocumentMut>()
        .map_err(|e| format!("parse {}: {e}", path.display()))?;
    let root = doc.as_table_mut();
    let theme_table = root
        .entry("theme")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| "[theme] is not a table".to_string())?;
    theme_table.insert("preset", value(preset));
    theme_table.insert("accent", value(&theme.colors.accent));
    theme_table.insert("focus_border", value(&theme.colors.focus));
    let colors = theme_table
        .entry("colors")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| "[theme.colors] is not a table".to_string())?;
    for (key, val) in [
        ("bg0", &theme.colors.bg0),
        ("bg1", &theme.colors.bg1),
        ("panel", &theme.colors.panel),
        ("panel2", &theme.colors.panel2),
        ("raise", &theme.colors.raise),
        ("border", &theme.colors.border),
        ("text", &theme.colors.text),
        ("dim", &theme.colors.dim),
        ("faint", &theme.colors.faint),
        ("ghost", &theme.colors.ghost),
    ] {
        colors.insert(key, value(val));
    }
    let hues = theme_table
        .entry("hues")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| "[theme.hues] is not a table".to_string())?;
    for (key, val) in [
        ("teal", &theme.hues.teal),
        ("magenta", &theme.hues.magenta),
        ("purple", &theme.hues.purple),
        ("green", &theme.hues.green),
        ("amber", &theme.hues.amber),
        ("red", &theme.hues.red),
        ("blue", &theme.hues.blue),
        ("orange", &theme.hues.orange),
    ] {
        hues.insert(key, value(val));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let temporary = path.with_extension("toml.tmp");
    std::fs::write(&temporary, doc.to_string()).map_err(|e| e.to_string())?;
    std::fs::rename(&temporary, path).map_err(|e| e.to_string())
}
