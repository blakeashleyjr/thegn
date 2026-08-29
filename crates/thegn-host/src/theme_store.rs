//! Off-loop provider for user themes.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use notify::{Event, EventKind, RecursiveMode, Watcher, recommended_watcher};
use termwiz::terminal::TerminalWaker;
use tokio::sync::mpsc as tokio_mpsc;

use thegn_core::theme_user::UserTheme;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ThemeOverrides {
    pub colors: thegn_core::config::ThemeColors,
    pub hues: thegn_core::config::ThemeHues,
    pub accent: Option<String>,
    pub focus_border: Option<String>,
}

pub const MAX_THEME_FILE_BYTES: usize = 64 * 1024;
const MAX_THEME_FILES: usize = 256;

#[derive(Debug)]
enum Request {
    Scan,
    Import(PathBuf),
    Save(UserTheme),
    Apply {
        preset: String,
        theme: UserTheme,
        overrides: Option<ThemeOverrides>,
    },
}

#[derive(Debug)]
enum Work {
    Request(Box<Request>),
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
                crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
                worker(
                    request_rx,
                    result_tx,
                    Some(waker),
                    themes_dir,
                    config_path,
                    watcher_tx,
                )
            })
            .expect("theme store worker thread");
        Self { request, results }
    }

    pub(crate) fn scan(&self) {
        let _ = self.request.send(Work::Request(Box::new(Request::Scan)));
    }

    pub(crate) fn import(&self, path: PathBuf) {
        let _ = self
            .request
            .send(Work::Request(Box::new(Request::Import(path))));
    }

    pub(crate) fn save(&self, theme: UserTheme) {
        let _ = self
            .request
            .send(Work::Request(Box::new(Request::Save(theme))));
    }

    pub(crate) fn apply(
        &self,
        preset: String,
        theme: UserTheme,
        overrides: Option<ThemeOverrides>,
    ) {
        let _ = self.request.send(Work::Request(Box::new(Request::Apply {
            preset,
            theme,
            overrides,
        })));
    }

    pub(crate) fn try_recv(&mut self) -> Option<ThemeStoreResult> {
        self.results.try_recv().ok()
    }
}

fn worker(
    request_rx: mpsc::Receiver<Work>,
    result_tx: tokio_mpsc::UnboundedSender<ThemeStoreResult>,
    waker: Option<TerminalWaker>,
    themes_dir: PathBuf,
    config_path: PathBuf,
    watcher_tx: mpsc::Sender<Work>,
) {
    if let Err(error) = std::fs::create_dir_all(&themes_dir) {
        tracing::warn!(target: "thegn::theme", error = %error, "theme directory unavailable");
    }
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

    publish_catalog(&themes_dir, &result_tx, waker.as_ref());
    while let Ok(work) = request_rx.recv() {
        match work {
            Work::Changed => {
                // Only watcher notifications are coalesced. User requests are
                // queued for processing after the one catalog refresh so a
                // Save/Import/Apply can never disappear during debounce.
                let deadline = std::time::Instant::now() + Duration::from_millis(200);
                let mut deferred = Vec::new();
                while let Some(remaining) =
                    deadline.checked_duration_since(std::time::Instant::now())
                {
                    match request_rx.recv_timeout(remaining) {
                        Ok(Work::Changed) => {}
                        Ok(request) => deferred.push(request),
                        Err(mpsc::RecvTimeoutError::Timeout) => break,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                publish_catalog(&themes_dir, &result_tx, waker.as_ref());
                for request in deferred {
                    process_request(
                        request,
                        &themes_dir,
                        &config_path,
                        &result_tx,
                        waker.as_ref(),
                    );
                }
            }
            Work::Request(request) => process_request(
                Work::Request(request),
                &themes_dir,
                &config_path,
                &result_tx,
                waker.as_ref(),
            ),
        }
    }
}

fn process_request(
    work: Work,
    themes_dir: &Path,
    config_path: &Path,
    result_tx: &tokio_mpsc::UnboundedSender<ThemeStoreResult>,
    waker: Option<&TerminalWaker>,
) {
    let Work::Request(request) = work else {
        return;
    };
    match *request {
        Request::Scan => publish_catalog(themes_dir, result_tx, waker),
        Request::Import(path) => {
            let result = read_bounded(&path).and_then(|bytes| {
                thegn_core::theme_import::import_gogh(&bytes).map_err(|e| e.to_string())
            });
            publish(result_tx, waker, ThemeStoreResult::Imported(result));
        }
        Request::Save(theme) => {
            let result = write_theme(themes_dir, &theme).map(|_| theme);
            publish(result_tx, waker, ThemeStoreResult::Saved(result));
        }
        Request::Apply {
            preset,
            theme,
            overrides,
        } => {
            let result =
                write_theme_selection(config_path, &preset, overrides.as_ref()).map(|_| theme);
            publish(result_tx, waker, ThemeStoreResult::Applied(result));
        }
    }
}

fn publish_catalog(
    dir: &Path,
    tx: &tokio_mpsc::UnboundedSender<ThemeStoreResult>,
    waker: Option<&TerminalWaker>,
) {
    let (themes, warnings) = scan_dir(dir);
    publish(tx, waker, ThemeStoreResult::Catalog { themes, warnings });
}

fn publish(
    tx: &tokio_mpsc::UnboundedSender<ThemeStoreResult>,
    waker: Option<&TerminalWaker>,
    result: ThemeStoreResult,
) {
    let _ = tx.send(result);
    if let Some(waker) = waker {
        let _ = waker.wake();
    }
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
    let mut entries = entries.flatten().collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.path());
    for entry in entries.into_iter().take(MAX_THEME_FILES) {
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
            Ok(theme) => {
                if thegn_core::theme::PRESETS.contains(&theme.meta.name.as_str()) {
                    warnings.push(format!(
                        "{}: user theme `{}` is shadowed by built-in preset",
                        path.display(),
                        theme.meta.name
                    ));
                }
                themes.push(theme);
            }
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

pub(crate) fn write_theme_selection(
    path: &Path,
    preset: &str,
    overrides: Option<&ThemeOverrides>,
) -> Result<(), String> {
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
    let Some(overrides) = overrides else {
        return write_config_document(path, &doc);
    };
    if let Some(accent) = &overrides.accent {
        theme_table.insert("accent", value(accent));
    }
    if let Some(focus) = &overrides.focus_border {
        theme_table.insert("focus_border", value(focus));
    }
    let mut insert_colors = |colors: &mut Table| {
        for (key, val) in [
            ("bg0", &overrides.colors.bg0),
            ("bg1", &overrides.colors.bg1),
            ("panel", &overrides.colors.panel),
            ("panel2", &overrides.colors.panel2),
            ("raise", &overrides.colors.raise),
            ("border", &overrides.colors.border),
            ("text", &overrides.colors.text),
            ("dim", &overrides.colors.dim),
            ("faint", &overrides.colors.faint),
            ("ghost", &overrides.colors.ghost),
        ] {
            if let Some(val) = val {
                colors.insert(key, value(val));
            }
        }
    };
    if overrides.colors.bg0.is_some()
        || overrides.colors.bg1.is_some()
        || overrides.colors.panel.is_some()
        || overrides.colors.panel2.is_some()
        || overrides.colors.raise.is_some()
        || overrides.colors.border.is_some()
        || overrides.colors.text.is_some()
        || overrides.colors.dim.is_some()
        || overrides.colors.faint.is_some()
        || overrides.colors.ghost.is_some()
    {
        let colors = theme_table
            .entry("colors")
            .or_insert_with(|| Item::Table(Table::new()))
            .as_table_mut()
            .ok_or_else(|| "[theme.colors] is not a table".to_string())?;
        insert_colors(colors);
    }
    let hue_values = [
        ("teal", &overrides.hues.teal),
        ("magenta", &overrides.hues.magenta),
        ("purple", &overrides.hues.purple),
        ("green", &overrides.hues.green),
        ("amber", &overrides.hues.amber),
        ("red", &overrides.hues.red),
        ("blue", &overrides.hues.blue),
        ("orange", &overrides.hues.orange),
    ];
    if hue_values.iter().any(|(_, value)| value.is_some()) {
        let hues = theme_table
            .entry("hues")
            .or_insert_with(|| Item::Table(Table::new()))
            .as_table_mut()
            .ok_or_else(|| "[theme.hues] is not a table".to_string())?;
        for (key, val) in hue_values {
            if let Some(val) = val {
                hues.insert(key, value(val));
            }
        }
    }
    write_config_document(path, &doc)
}

fn write_config_document(path: &Path, doc: &toml_edit::DocumentMut) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let temporary = path.with_extension("toml.tmp");
    std::fs::write(&temporary, doc.to_string()).map_err(|e| e.to_string())?;
    std::fs::rename(&temporary, path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir(label: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("thegn-theme-{label}-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn theme(name: &str) -> UserTheme {
        let cfg = thegn_core::config::Config::default();
        UserTheme::from_palette(name, &cfg.palette())
    }

    #[test]
    fn selection_and_token_edit_preserve_comments_and_existing_overrides() {
        let dir = temp_dir("config");
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "# keep this comment\n[theme]\npreset = \"prism\"\n[theme.colors]\n# keep bg0\nbg0 = \"#010203\"\n",
        )
        .unwrap();

        write_theme_selection(&path, "storm", None).unwrap();
        let selected = std::fs::read_to_string(&path).unwrap();
        assert!(selected.contains("# keep this comment"));
        assert!(selected.contains("preset = \"storm\""));
        assert!(selected.contains("bg0 = \"#010203\""));

        let mut overrides = ThemeOverrides::default();
        overrides.colors.text = Some("#abcdef".into());
        write_theme_selection(&path, "local-paper", Some(&overrides)).unwrap();
        let edited = std::fs::read_to_string(&path).unwrap();
        assert!(edited.contains("preset = \"local-paper\""));
        assert!(edited.contains("text = \"#abcdef\""));
        assert!(edited.contains("bg0 = \"#010203\""));
        assert!(edited.contains("# keep bg0"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn watcher_debounce_preserves_save_import_and_apply_results() {
        let dir = temp_dir("worker");
        let themes_dir = dir.join("themes");
        let import_path = dir.join("import.yml");
        let mut gogh = String::from(
            "name: imported\nbackground: '#101010'\nforeground: '#f0f0f0'\ncursor: '#00ff00'\n",
        );
        for index in 1..=16 {
            gogh.push_str(&format!("color_{index:02}: '#010101'\n"));
        }
        std::fs::write(&import_path, gogh).unwrap();
        let config_path = dir.join("config.toml");
        std::fs::write(&config_path, "[theme]\npreset = \"prism\"\n").unwrap();
        let (request_tx, request_rx) = mpsc::channel();
        let (watcher_tx, _watcher_rx) = mpsc::channel();
        let (result_tx, mut result_rx) = tokio_mpsc::unbounded_channel();
        let saved = theme("saved");
        let applied = theme("applied");
        let worker = std::thread::spawn(move || {
            worker(
                request_rx,
                result_tx,
                None,
                themes_dir,
                config_path,
                watcher_tx,
            );
        });
        request_tx.send(Work::Changed).unwrap();
        request_tx
            .send(Work::Request(Box::new(Request::Save(saved))))
            .unwrap();
        request_tx
            .send(Work::Request(Box::new(Request::Import(import_path))))
            .unwrap();
        request_tx
            .send(Work::Request(Box::new(Request::Apply {
                preset: "applied".into(),
                theme: applied,
                overrides: None,
            })))
            .unwrap();
        drop(request_tx);
        worker.join().unwrap();

        let mut imported = 0;
        let mut saved = 0;
        let mut applied = 0;
        while let Ok(result) = result_rx.try_recv() {
            match result {
                ThemeStoreResult::Imported(Ok(_)) => imported += 1,
                ThemeStoreResult::Saved(Ok(_)) => saved += 1,
                ThemeStoreResult::Applied(Ok(_)) => applied += 1,
                _ => {}
            }
        }
        assert_eq!((imported, saved, applied), (1, 1, 1));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn valid_builtin_collision_is_reported_with_path_and_name() {
        let dir = temp_dir("collision");
        let path = dir.join("shadow.toml");
        std::fs::write(
            &path,
            theme(thegn_core::theme::PRESETS[0]).to_toml().unwrap(),
        )
        .unwrap();
        let (_, warnings) = scan_dir(&dir);
        assert!(warnings.iter().any(|warning| {
            warning.contains(path.to_string_lossy().as_ref())
                && warning.contains(thegn_core::theme::PRESETS[0])
        }));
        let _ = std::fs::remove_dir_all(dir);
    }
}
