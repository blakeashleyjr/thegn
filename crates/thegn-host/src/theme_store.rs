//! Off-loop provider for user themes.

use std::collections::{BinaryHeap, HashMap};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
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
        overrides: Option<Box<ThemeOverrides>>,
    },
}

#[derive(Debug)]
enum Work {
    Request(Box<Request>),
    Changed,
    Shutdown,
}

struct WorkerState {
    shutdown: Arc<AtomicBool>,
    active: Arc<Mutex<String>>,
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
    request: Option<mpsc::Sender<Work>>,
    results: tokio_mpsc::UnboundedReceiver<ThemeStoreResult>,
    shutdown: Arc<AtomicBool>,
    active: Arc<Mutex<String>>,
    worker: Option<JoinHandle<()>>,
}

impl ThemeStore {
    pub(crate) fn spawn(waker: TerminalWaker, config_path: PathBuf, active_preset: &str) -> Self {
        let themes_dir = thegn_core::util::xdg_config_home().join("thegn/themes");
        Self::start(
            Some(waker),
            themes_dir,
            config_path,
            active_preset.to_owned(),
        )
    }

    fn start(
        waker: Option<TerminalWaker>,
        themes_dir: PathBuf,
        config_path: PathBuf,
        active_preset: String,
    ) -> Self {
        let (request, request_rx) = mpsc::channel();
        let (result_tx, results) = tokio_mpsc::unbounded_channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let active = Arc::new(Mutex::new(active_preset));
        let worker_active = Arc::clone(&active);
        let watcher_tx = request.clone();
        let worker_shutdown = Arc::clone(&shutdown);
        let worker = std::thread::Builder::new()
            .name("theme-store".into())
            .spawn(move || {
                crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
                worker(
                    request_rx,
                    result_tx,
                    waker,
                    themes_dir,
                    config_path,
                    watcher_tx,
                    WorkerState {
                        shutdown: worker_shutdown,
                        active: worker_active,
                    },
                )
            });
        Self::from_worker_spawn(worker, request, results, shutdown, active)
    }

    fn from_worker_spawn(
        worker: std::io::Result<JoinHandle<()>>,
        request: mpsc::Sender<Work>,
        results: tokio_mpsc::UnboundedReceiver<ThemeStoreResult>,
        shutdown: Arc<AtomicBool>,
        active: Arc<Mutex<String>>,
    ) -> Self {
        match worker {
            Ok(worker) => Self::new_running(request, results, shutdown, active, worker),
            Err(error) => {
                tracing::warn!(target: "thegn::theme", error = %error, "theme store worker unavailable");
                drop(request);
                Self::new_degraded(results, shutdown, active)
            }
        }
    }

    fn new_running(
        request: mpsc::Sender<Work>,
        results: tokio_mpsc::UnboundedReceiver<ThemeStoreResult>,
        shutdown: Arc<AtomicBool>,
        active: Arc<Mutex<String>>,
        worker: JoinHandle<()>,
    ) -> Self {
        Self {
            request: Some(request),
            results,
            shutdown,
            active,
            worker: Some(worker),
        }
    }

    fn new_degraded(
        results: tokio_mpsc::UnboundedReceiver<ThemeStoreResult>,
        shutdown: Arc<AtomicBool>,
        active: Arc<Mutex<String>>,
    ) -> Self {
        Self {
            request: None,
            results,
            shutdown,
            active,
            worker: None,
        }
    }

    pub(crate) fn scan(&self) -> Result<(), String> {
        self.send(Request::Scan)
    }

    pub(crate) fn import(&self, path: PathBuf) -> Result<(), String> {
        self.send(Request::Import(path))
    }

    pub(crate) fn save(&self, theme: UserTheme) -> Result<(), String> {
        self.send(Request::Save(theme))
    }

    pub(crate) fn apply(
        &self,
        preset: String,
        theme: UserTheme,
        overrides: Option<Box<ThemeOverrides>>,
    ) -> Result<(), String> {
        self.set_active(&preset);
        self.send(Request::Apply {
            preset,
            theme,
            overrides,
        })
    }

    pub(crate) fn set_active(&self, preset: &str) {
        match self.active.lock() {
            Ok(mut active) => *active = preset.to_owned(),
            Err(error) => tracing::warn!(
                target: "thegn::theme",
                error = %error,
                "active theme selection unavailable; catalog scans will not pin a theme"
            ),
        }
    }

    pub(crate) fn try_recv(&mut self) -> Option<ThemeStoreResult> {
        self.results.try_recv().ok()
    }

    /// Await the worker's first off-loop scan before composing the first frame.
    /// The directory I/O remains on the background thread; startup merely
    /// yields until the configured user preset can be resolved correctly.
    pub(crate) async fn initial_catalog(&mut self) -> (Vec<UserTheme>, Vec<String>) {
        match self.results.recv().await {
            Some(ThemeStoreResult::Catalog { themes, warnings }) => (themes, warnings),
            Some(_) => (
                Vec::new(),
                vec!["theme store returned an unexpected startup result".into()],
            ),
            None => (
                Vec::new(),
                vec!["theme store stopped before its startup scan completed".into()],
            ),
        }
    }

    fn send(&self, request: Request) -> Result<(), String> {
        self.request
            .as_ref()
            .ok_or_else(|| "theme store worker is unavailable".to_string())?
            .send(Work::Request(Box::new(request)))
            .map_err(|_| "theme store worker is unavailable".to_string())
    }
}

impl Drop for ThemeStore {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(request) = &self.request {
            // best-effort: worker may already have exited; the flag alone ends it
            let _ = request.send(Work::Shutdown);
        }
        if let Some(worker) = self.worker.take() {
            // Shutdown completes after the current filesystem operation and
            // every user mutation accepted before Drop; watcher work is dropped.
            if worker.join().is_err() {
                tracing::warn!(target: "thegn::theme", "theme store worker panicked");
            }
        }
    }
}

fn worker(
    request_rx: mpsc::Receiver<Work>,
    result_tx: tokio_mpsc::UnboundedSender<ThemeStoreResult>,
    waker: Option<TerminalWaker>,
    themes_dir: PathBuf,
    config_path: PathBuf,
    watcher_tx: mpsc::Sender<Work>,
    state: WorkerState,
) {
    let WorkerState { shutdown, active } = state;
    if let Err(error) = std::fs::create_dir_all(&themes_dir) {
        tracing::warn!(target: "thegn::theme", error = %error, "theme directory unavailable");
    }
    let watcher_shutdown = Arc::clone(&shutdown);
    let watcher_result = RecommendedWatcher::new(
        move |result: notify::Result<Event>| {
            if watcher_shutdown.load(Ordering::Acquire) {
                return;
            }
            if result.is_ok_and(|event| {
                matches!(
                    event.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                )
            }) {
                let _ = watcher_tx.send(Work::Changed);
            }
        },
        crate::diff_watch::watcher_config(),
    );
    let mut watcher = match watcher_result {
        Ok(watcher) => Some(watcher),
        Err(error) => {
            tracing::warn!(target: "thegn::theme", error = %error, "theme watcher unavailable");
            None
        }
    };
    if let Some(w) = watcher.as_mut()
        && let Err(error) = w.watch(&themes_dir, RecursiveMode::NonRecursive)
    {
        tracing::warn!(target: "thegn::theme", error = %error, "theme directory watch unavailable");
    }

    publish_catalog(&themes_dir, &result_tx, waker.as_ref(), &active);
    'run: while let Ok(work) = request_rx.recv() {
        if shutdown.load(Ordering::Acquire) {
            finish_shutdown(
                Some(work),
                Vec::new(),
                &mut watcher,
                &request_rx,
                &themes_dir,
                &config_path,
                &result_tx,
                waker.as_ref(),
            );
            break;
        }
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
                    if shutdown.load(Ordering::Acquire) {
                        finish_shutdown(
                            None,
                            deferred,
                            &mut watcher,
                            &request_rx,
                            &themes_dir,
                            &config_path,
                            &result_tx,
                            waker.as_ref(),
                        );
                        break 'run;
                    }
                    match request_rx.recv_timeout(remaining) {
                        Ok(Work::Changed) => {}
                        Ok(Work::Shutdown) => {}
                        Ok(request) => deferred.push(request),
                        Err(mpsc::RecvTimeoutError::Timeout) => break,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                if shutdown.load(Ordering::Acquire) {
                    finish_shutdown(
                        None,
                        deferred,
                        &mut watcher,
                        &request_rx,
                        &themes_dir,
                        &config_path,
                        &result_tx,
                        waker.as_ref(),
                    );
                    break 'run;
                }
                publish_catalog(&themes_dir, &result_tx, waker.as_ref(), &active);
                let mut deferred = deferred.into_iter();
                while let Some(request) = deferred.next() {
                    if shutdown.load(Ordering::Acquire) {
                        finish_shutdown(
                            Some(request),
                            deferred.collect(),
                            &mut watcher,
                            &request_rx,
                            &themes_dir,
                            &config_path,
                            &result_tx,
                            waker.as_ref(),
                        );
                        break 'run;
                    }
                    process_request(
                        request,
                        &themes_dir,
                        &config_path,
                        &result_tx,
                        waker.as_ref(),
                        Some(&active),
                    );
                }
            }
            Work::Request(request) => process_request(
                Work::Request(request),
                &themes_dir,
                &config_path,
                &result_tx,
                waker.as_ref(),
                Some(&active),
            ),
            Work::Shutdown => {
                finish_shutdown(
                    None,
                    Vec::new(),
                    &mut watcher,
                    &request_rx,
                    &themes_dir,
                    &config_path,
                    &result_tx,
                    waker.as_ref(),
                );
                break;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn finish_shutdown(
    first: Option<Work>,
    deferred: Vec<Work>,
    watcher: &mut Option<notify::RecommendedWatcher>,
    request_rx: &mpsc::Receiver<Work>,
    themes_dir: &Path,
    config_path: &Path,
    result_tx: &tokio_mpsc::UnboundedSender<ThemeStoreResult>,
    waker: Option<&TerminalWaker>,
) {
    // Stop notifications before draining, so an event storm cannot keep the
    // queue nonempty. Accepted user mutations remain lossless through shutdown.
    drop(watcher.take());
    if let Some(work) = first {
        process_shutdown_work(work, themes_dir, config_path, result_tx, waker);
    }
    for work in deferred {
        process_shutdown_work(work, themes_dir, config_path, result_tx, waker);
    }
    while let Ok(work) = request_rx.try_recv() {
        process_shutdown_work(work, themes_dir, config_path, result_tx, waker);
    }
}

fn process_shutdown_work(
    work: Work,
    themes_dir: &Path,
    config_path: &Path,
    result_tx: &tokio_mpsc::UnboundedSender<ThemeStoreResult>,
    waker: Option<&TerminalWaker>,
) {
    let Work::Request(request) = work else {
        return;
    };
    if matches!(request.as_ref(), Request::Scan) {
        return;
    }
    process_request(
        Work::Request(request),
        themes_dir,
        config_path,
        result_tx,
        waker,
        None,
    );
}

fn process_request(
    work: Work,
    themes_dir: &Path,
    config_path: &Path,
    result_tx: &tokio_mpsc::UnboundedSender<ThemeStoreResult>,
    waker: Option<&TerminalWaker>,
    active: Option<&Mutex<String>>,
) {
    let Work::Request(request) = work else {
        return;
    };
    match *request {
        Request::Scan => {
            if let Some(active) = active {
                publish_catalog(themes_dir, result_tx, waker, active);
            }
        }
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
                write_theme_selection(config_path, &preset, overrides.as_deref()).map(|_| theme);
            publish(result_tx, waker, ThemeStoreResult::Applied(result));
        }
    }
}

fn publish_catalog(
    dir: &Path,
    tx: &tokio_mpsc::UnboundedSender<ThemeStoreResult>,
    waker: Option<&TerminalWaker>,
    active: &Mutex<String>,
) {
    let preset = match active.lock() {
        Ok(preset) => Some(preset.clone()),
        Err(error) => {
            tracing::warn!(
                target: "thegn::theme",
                error = %error,
                "active theme selection unavailable; catalog scan will not pin a theme"
            );
            None
        }
    };
    let (themes, warnings) = scan_dir(dir, preset.as_deref());
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

pub(crate) fn scan_dir(dir: &Path, active: Option<&str>) -> (Vec<UserTheme>, Vec<String>) {
    let mut themes = Vec::new();
    let mut warnings = Vec::new();
    if let Err(e) = std::fs::create_dir_all(dir) {
        warnings.push(format!("theme directory unavailable: {e}"));
        return (themes, warnings);
    }
    let pinned = active.and_then(|name| {
        let slug = thegn_core::util::slugify(name);
        (!slug.is_empty()
            && slug.len() <= 80
            && !thegn_core::theme::PRESETS
                .iter()
                .any(|preset| thegn_core::util::slugify(preset) == slug))
        .then(|| format!("{slug}.toml"))
    });
    let paths = bounded_theme_paths(dir, pinned.as_deref(), &mut warnings);
    let mut exact = HashMap::<String, usize>::new();
    let mut normalized = HashMap::<String, usize>::new();
    for path in paths {
        match read_bounded(&path).and_then(|bytes| {
            let text = std::str::from_utf8(&bytes).map_err(|_| "not UTF-8".to_string())?;
            UserTheme::from_toml(text).map_err(|e| e.to_string())
        }) {
            Ok(theme) => {
                *exact.entry(theme.meta.name.clone()).or_default() += 1;
                let identity = thegn_core::util::slugify(&theme.meta.name);
                *normalized.entry(identity.clone()).or_default() += 1;
                if identity.is_empty() || identity.len() > 80 {
                    warnings.push(format!(
                        "{}: theme name has an empty or overlong storage identity",
                        path.display()
                    ));
                    continue;
                }
                if thegn_core::theme::PRESETS
                    .iter()
                    .any(|preset| thegn_core::util::slugify(preset) == identity)
                {
                    warnings.push(format!(
                        "{}: user theme `{}` conflicts with a built-in preset identity",
                        path.display(),
                        theme.meta.name
                    ));
                    continue;
                }
                let expected = format!("{identity}.toml");
                if path.file_name().and_then(|name| name.to_str()) != Some(expected.as_str()) {
                    warnings.push(format!(
                        "{}: filename does not match theme name `{}` (expected `{expected}`)",
                        path.display(),
                        theme.meta.name
                    ));
                    continue;
                }
                themes.push(theme);
            }
            Err(e) => warnings.push(format!("{}: {e}", path.display())),
        }
    }
    for (name, count) in &exact {
        if *count > 1 {
            warnings.push(format!(
                "ambiguous user theme identity `{name}`; duplicate exact name"
            ));
        }
    }
    for (name, count) in &normalized {
        if *count > 1 {
            warnings.push(format!("ambiguous normalized user theme identity `{name}`"));
        }
    }
    themes.retain(|theme| {
        let exact_count = exact.get(&theme.meta.name).copied().unwrap_or(0);
        let normalized_count = normalized
            .get(&thegn_core::util::slugify(&theme.meta.name))
            .copied()
            .unwrap_or(0);
        exact_count <= 1 && normalized_count <= 1
    });
    themes.sort_by(|a, b| a.meta.name.cmp(&b.meta.name));
    warnings.sort();
    (themes, warnings)
}

fn bounded_theme_paths(
    dir: &Path,
    pinned: Option<&str>,
    warnings: &mut Vec<String>,
) -> Vec<PathBuf> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            warnings.push(format!("theme directory could not be read: {error}"));
            return Vec::new();
        }
    };
    // Keep only the lexically first bounded set of actual theme files. A
    // bounded max-heap avoids collecting/sorting an untrusted directory, while
    // examining every name makes the result independent of filesystem order
    // and prevents unrelated junk from consuming the theme-file allowance.
    let mut paths = BinaryHeap::with_capacity(MAX_THEME_FILES + 1);
    let mut pinned_path = None;
    let mut theme_file_count = 0usize;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warnings.push(format!("theme directory entry could not be read: {error}"));
                continue;
            }
        };
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("toml") {
            continue;
        }
        let kind = match entry.file_type() {
            Ok(kind) => kind,
            Err(error) => {
                warnings.push(format!("{}: metadata unavailable: {error}", path.display()));
                continue;
            }
        };
        if !kind.is_file() {
            continue;
        }
        theme_file_count += 1;
        if pinned.is_some_and(|p| entry.file_name() == std::ffi::OsStr::new(p)) {
            pinned_path = Some(path);
            continue;
        }
        if paths.len() < MAX_THEME_FILES {
            paths.push(path);
        } else if paths.peek().is_some_and(|largest| path < *largest) {
            paths.pop();
            paths.push(path);
        }
    }
    if theme_file_count > MAX_THEME_FILES {
        let pin_note = pinned_path
            .as_ref()
            .and_then(|path| path.file_name())
            .and_then(|name| name.to_str())
            .map(|name| format!(" plus the selected theme `{name}`"))
            .unwrap_or_default();
        warnings.push(format!(
            "theme directory contains {theme_file_count} theme files; loading the first {MAX_THEME_FILES} by name{pin_note}"
        ));
    }
    let mut paths = paths.into_vec();
    if let Some(path) = pinned_path {
        paths.push(path);
    }
    paths.sort();
    paths
}

pub(crate) fn read_bounded(path: &Path) -> Result<Vec<u8>, String> {
    let mut file = crate::platform::open_nofollow(path).map_err(|e| e.to_string())?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.file_type().is_file() {
        return Err("theme source is not a regular file".into());
    }
    let size = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
    if size > MAX_THEME_FILE_BYTES {
        return Err(format!(
            "theme file is {size} bytes; maximum is {MAX_THEME_FILE_BYTES}"
        ));
    }
    let mut bytes = Vec::with_capacity(size.min(MAX_THEME_FILE_BYTES + 1));
    Read::by_ref(&mut file)
        .take((MAX_THEME_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > MAX_THEME_FILE_BYTES {
        return Err(format!(
            "theme file exceeds {MAX_THEME_FILE_BYTES} bytes while reading"
        ));
    }
    Ok(bytes)
}

fn valid_slug(name: &str) -> Result<String, String> {
    let slug = thegn_core::util::slugify(name);
    if slug.is_empty() || slug.len() > 80 {
        return Err("theme name must produce a non-empty slug of at most 80 characters".into());
    }
    Ok(slug)
}

pub(crate) fn write_theme(dir: &Path, theme: &UserTheme) -> Result<(), String> {
    theme.validate().map_err(|e| e.to_string())?;
    if thegn_core::theme::PRESETS.iter().any(|preset| {
        thegn_core::util::slugify(preset) == thegn_core::util::slugify(&theme.meta.name)
    }) {
        return Err(format!(
            "theme name `{}` is reserved by a built-in preset",
            theme.meta.name
        ));
    }
    let slug = valid_slug(&theme.meta.name)?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let destination = dir.join(format!("{slug}.toml"));
    match read_bounded(&destination) {
        Ok(bytes) => {
            let existing = UserTheme::from_toml(
                std::str::from_utf8(&bytes).map_err(|_| "existing theme is not UTF-8")?,
            )
            .map_err(|error| format!("existing theme identity is ambiguous: {error}"))?;
            if existing.meta.name != theme.meta.name {
                return Err(format!(
                    "theme filename collision: `{}` already belongs to `{}`",
                    destination.display(),
                    existing.meta.name
                ));
            }
        }
        Err(_) if !destination.exists() => {}
        Err(error) => return Err(format!("cannot verify existing theme identity: {error}")),
    }
    let text = theme.to_toml().map_err(|e| e.to_string())?;
    atomic_write(&destination, text.as_bytes(), None)
}

pub(crate) fn write_theme_selection(
    path: &Path,
    preset: &str,
    overrides: Option<&ThemeOverrides>,
) -> Result<(), String> {
    use toml_edit::{DocumentMut, Item, Table, value};
    let expected = match thegn_core::fsperm::open_regular_file_nofollow(path) {
        Ok(mut file) => {
            let metadata = file
                .metadata()
                .map_err(|e| format!("inspect {}: {e}", path.display()))?;
            if !thegn_core::fsperm::owned_by_current_user(&metadata) {
                return Err("config file is not owned by the current user".into());
            }
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)
                .map_err(|e| format!("read {}: {e}", path.display()))?;
            Some(bytes)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    let text = expected
        .as_deref()
        .map(std::str::from_utf8)
        .transpose()
        .map_err(|e| format!("config is not UTF-8: {e}"))?
        .unwrap_or("");
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
        return thegn_core::config_write::write_document_fenced(path, &doc, expected.as_deref())
            .map_err(|e| e.to_string());
    };
    if let Some(accent) = &overrides.accent {
        theme_table.insert("accent", value(accent));
    }
    if let Some(focus) = &overrides.focus_border {
        theme_table.insert("focus_border", value(focus));
    }
    let insert_colors = |colors: &mut Table| {
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
    thegn_core::config_write::write_document_fenced(path, &doc, expected.as_deref())
        .map_err(|e| e.to_string())
}

struct PendingTemp(Option<PathBuf>);

impl Drop for PendingTemp {
    fn drop(&mut self) {
        // best-effort: a failed atomic write must not strand its private temp.
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn atomic_write(
    destination: &Path,
    bytes: &[u8],
    permissions: Option<std::fs::Permissions>,
) -> Result<(), String> {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let base = destination
        .file_name()
        .ok_or_else(|| "write destination has no file name".to_string())?;
    let mut opened = None;
    for _ in 0..128 {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let mut name = base.to_os_string();
        name.push(format!(".tmp.{}.{sequence}", std::process::id()));
        let path = parent.join(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => {
                opened = Some((path, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    let (temporary, mut file) = opened
        .ok_or_else(|| "could not allocate a unique temporary file for atomic write".to_string())?;
    let mut pending = PendingTemp(Some(temporary));
    file.write_all(bytes).map_err(|e| e.to_string())?;
    if let Some(permissions) = permissions {
        file.set_permissions(permissions)
            .map_err(|e| e.to_string())?;
    }
    file.sync_all().map_err(|e| e.to_string())?;
    drop(file);
    std::fs::rename(pending.0.as_ref().expect("temporary path"), destination)
        .map_err(|e| e.to_string())?;
    pending.0 = None;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("thegn-theme-{label}-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn private_temp_dir() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        thegn_core::fsperm::ensure_private_dir(temp.path()).unwrap();
        temp
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

        write_theme_selection(&path, "local-paper", None).unwrap();
        let user_selected = std::fs::read_to_string(&path).unwrap();
        assert!(user_selected.contains("preset = \"local-paper\""));
        assert!(!user_selected.contains("text = \"#"));

        let mut overrides = ThemeOverrides::default();
        overrides.colors.text = Some("#abcdef".into());
        write_theme_selection(&path, "local-paper", Some(&overrides)).unwrap();
        let edited = std::fs::read_to_string(&path).unwrap();
        assert!(edited.contains("preset = \"local-paper\""));
        assert!(edited.contains("text = \"#abcdef\""));
        assert!(edited.contains("bg0 = \"#010203\""));
        assert!(edited.contains("# keep bg0"));
        std::fs::remove_dir_all(dir).unwrap();
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
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
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
                WorkerState {
                    shutdown: worker_shutdown,
                    active: Arc::new(Mutex::new(String::new())),
                },
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
        std::fs::remove_dir_all(dir).unwrap();
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
        let (_, warnings) = scan_dir(&dir, None);
        assert!(warnings.iter().any(|warning| {
            warning.contains(path.to_string_lossy().as_ref())
                && warning.contains(thegn_core::theme::PRESETS[0])
        }));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn save_slug_collision_preserves_the_existing_theme() {
        let dir = temp_dir("save-collision");
        let first = theme("Night/Sky");
        write_theme(&dir, &first).unwrap();
        let path = dir.join(format!(
            "{}.toml",
            thegn_core::util::slugify(&first.meta.name)
        ));
        let before = std::fs::read(&path).unwrap();
        let error = write_theme(&dir, &theme("Night-Sky")).unwrap_err();
        assert!(error.contains("filename collision"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn scan_rejects_filename_mismatch_and_duplicate_exact_identity() {
        let dir = temp_dir("identity-scan");
        let duplicate = theme("shared").to_toml().unwrap();
        std::fs::write(dir.join("shared.toml"), &duplicate).unwrap();
        std::fs::write(dir.join("other.toml"), &duplicate).unwrap();
        let (themes, warnings) = scan_dir(&dir, None);
        assert!(themes.is_empty(), "ambiguous theme must not enter catalog");
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("filename does not match"))
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("ambiguous user theme identity"))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn scan_rejects_normalized_name_collisions() {
        let dir = temp_dir("normalized-scan");
        for (filename, name) in [("one.toml", "Night-Sky"), ("two.toml", "night_sky")] {
            let candidate = theme(name);
            let path = dir.join(filename);
            std::fs::write(path, candidate.to_toml().unwrap()).unwrap();
        }
        let (themes, warnings) = scan_dir(&dir, None);
        assert!(themes.is_empty(), "normalized duplicates must not resolve");
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("ambiguous normalized user theme identity"))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn save_refuses_reserved_builtin_name() {
        let dir = temp_dir("reserved-name");
        let error = write_theme(&dir, &theme(thegn_core::theme::PRESETS[0])).unwrap_err();
        assert!(error.contains("reserved by a built-in"), "{error}");
        assert!(std::fs::read_dir(&dir).unwrap().next().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unicode_slug_collision_preserves_the_first_theme() {
        let dir = temp_dir("unicode-collision");
        let first = theme("Café");
        write_theme(&dir, &first).unwrap();
        let before = std::fs::read(dir.join("caf.toml")).unwrap();
        let error = write_theme(&dir, &theme("Cafè")).unwrap_err();
        assert!(error.contains("filename collision"), "{error}");
        assert_eq!(std::fs::read(dir.join("caf.toml")).unwrap(), before);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn save_rejects_empty_and_overlong_storage_names() {
        let dir = temp_dir("invalid-name");
        let empty = write_theme(&dir, &theme(" ")).unwrap_err();
        assert!(empty.contains("must not be empty"), "{empty}");
        let overlong = write_theme(&dir, &theme(&"x".repeat(81))).unwrap_err();
        assert!(overlong.contains("80 characters"), "{overlong}");
        assert!(std::fs::read_dir(&dir).unwrap().next().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn catalog_filters_junk_before_applying_the_theme_file_cap() {
        let dir = temp_dir("bounded-catalog");
        for index in 0..300 {
            std::fs::write(dir.join(format!("{index:03}-junk.txt")), b"junk").unwrap();
        }
        std::fs::write(
            dir.join("still-visible.toml"),
            theme("still-visible").to_toml().unwrap(),
        )
        .unwrap();

        let (themes, warnings) = scan_dir(&dir, None);
        assert_eq!(
            themes
                .iter()
                .map(|theme| theme.meta.name.as_str())
                .collect::<Vec<_>>(),
            ["still-visible"]
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn catalog_cap_never_hides_the_selected_theme() {
        let temp = private_temp_dir();
        let dir = temp.path().join("themes");
        std::fs::create_dir_all(&dir).unwrap();
        for index in 0..=256 {
            let candidate = theme(&format!("t{index:03}"));
            std::fs::write(
                dir.join(format!("t{index:03}.toml")),
                candidate.to_toml().unwrap(),
            )
            .unwrap();
        }
        let selected = theme("zz-selected");
        std::fs::write(dir.join("zz-selected.toml"), selected.to_toml().unwrap()).unwrap();

        let (themes, warnings) = scan_dir(&dir, Some("zz-selected"));
        assert_eq!(themes.len(), 257);
        assert!(themes.iter().any(|item| item.meta.name == "zz-selected"));
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("zz-selected.toml"))
        );

        let (themes, _) = scan_dir(&dir, None);
        assert!(!themes.iter().any(|item| item.meta.name == "zz-selected"));
    }

    #[test]
    fn pinned_theme_still_obeys_identity_checks() {
        let temp = private_temp_dir();
        let dir = temp.path().join("themes");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("zz-selected.toml"),
            theme("different-name").to_toml().unwrap(),
        )
        .unwrap();

        let (themes, warnings) = scan_dir(&dir, Some("zz-selected"));
        assert!(themes.is_empty());
        assert!(warnings.iter().any(|warning| {
            warning.contains("filename does not match") && warning.contains("zz-selected.toml")
        }));
    }

    #[test]
    fn store_scan_uses_the_active_preset() {
        let temp = private_temp_dir();
        let themes = temp.path().join("themes");
        std::fs::create_dir_all(&themes).unwrap();
        for index in 0..=256 {
            let candidate = theme(&format!("t{index:03}"));
            std::fs::write(
                themes.join(format!("t{index:03}.toml")),
                candidate.to_toml().unwrap(),
            )
            .unwrap();
        }
        let selected = theme("zz-selected");
        std::fs::write(themes.join("zz-selected.toml"), selected.to_toml().unwrap()).unwrap();
        let other = theme("zz-other");
        std::fs::write(themes.join("zz-other.toml"), other.to_toml().unwrap()).unwrap();
        let mut store = ThemeStore::start(
            None,
            themes,
            temp.path().join("config.toml"),
            "zz-selected".into(),
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let (catalog, _) = runtime.block_on(store.initial_catalog());
        assert!(catalog.iter().any(|item| item.meta.name == "zz-selected"));

        store.set_active("zz-other");
        store.scan().unwrap();
        let (catalog, _) = runtime.block_on(store.initial_catalog());
        assert!(catalog.len() >= 256);
        assert!(catalog.iter().any(|item| item.meta.name == "zz-other"));
        assert!(!catalog.iter().any(|item| item.meta.name == "zz-selected"));
    }

    #[test]
    fn atomic_theme_and_config_writes_ignore_predictable_temp_symlinks() {
        let dir = temp_dir("temp-symlinks");
        let victim = dir.join("victim");
        std::fs::write(&victim, b"do not replace").unwrap();

        let saved = theme("paper");
        let old_theme_temp = dir.join("paper.toml.tmp");
        if crate::platform::symlink_file_for_test(&victim, &old_theme_temp).is_err() {
            let _ = std::fs::remove_dir_all(dir);
            return;
        }
        write_theme(&dir, &saved).unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"do not replace");
        assert!(
            std::fs::symlink_metadata(&old_theme_temp)
                .unwrap()
                .file_type()
                .is_symlink()
        );

        let config = dir.join("config.toml");
        std::fs::write(&config, "[theme]\npreset = \"prism\"\n").unwrap();
        let mut permissions = std::fs::metadata(&config).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&config, permissions).unwrap();
        let old_config_temp = config.with_extension("toml.tmp");
        crate::platform::symlink_file_for_test(&victim, &old_config_temp).unwrap();
        write_theme_selection(&config, "storm", None).unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"do not replace");
        assert!(
            std::fs::symlink_metadata(&old_config_temp)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(std::fs::metadata(&config).unwrap().permissions().readonly());
        assert!(
            std::fs::read_to_string(&config)
                .unwrap()
                .contains("preset = \"storm\"")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn bounded_reader_rejects_a_final_component_symlink() {
        let dir = temp_dir("read-symlink");
        let target = dir.join("target.toml");
        let link = dir.join("link.toml");
        std::fs::write(&target, theme("target").to_toml().unwrap()).unwrap();
        if crate::platform::symlink_file_for_test(&target, &link).is_ok() {
            assert!(read_bounded(&link).is_err());
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn bounded_reader_rejects_a_fifo_without_blocking() {
        let dir = temp_dir("read-fifo");
        let fifo = dir.join("blocking.toml");
        if !crate::platform::test_fifo_supported() {
            let _ = std::fs::remove_dir_all(dir);
            return;
        }
        crate::platform::test_fifo(&fifo).unwrap();

        assert!(read_bounded(&fifo).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn disconnected_worker_requests_return_an_error() {
        let (request, request_rx) = mpsc::channel();
        drop(request_rx);
        let (_result_tx, results) = tokio_mpsc::unbounded_channel();
        let store = ThemeStore {
            request: Some(request),
            results,
            shutdown: Arc::new(AtomicBool::new(false)),
            active: Arc::new(Mutex::new(String::new())),
            worker: None,
        };

        assert_eq!(
            store.save(theme("unavailable")).unwrap_err(),
            "theme store worker is unavailable"
        );
    }

    fn drop_with_timeout(store: ThemeStore) {
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            drop(store);
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("theme store did not stop within three seconds");
    }

    #[test]
    fn dropping_store_joins_worker_and_releases_watcher() {
        let dir = temp_dir("drop-joins");
        for cycle in 0..8 {
            let themes = dir.join(format!("themes-{cycle}"));
            let store = ThemeStore::start(None, themes, dir.join("config.toml"), String::new());
            let probe = store.request.clone().unwrap();
            drop_with_timeout(store);
            // The worker owns the only receiver; a failed send proves it exited.
            assert!(probe.send(Work::Changed).is_err());
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn shutdown_discards_a_large_watcher_backlog() {
        let dir = temp_dir("drop-backlog");
        let themes = dir.join("themes");
        let store = ThemeStore::start(None, themes, dir.join("config.toml"), String::new());
        for _ in 0..50_000 {
            store.request.as_ref().unwrap().send(Work::Changed).unwrap();
        }
        drop_with_timeout(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn accepted_save_is_written_before_shutdown_returns() {
        let dir = temp_dir("drop-save");
        let themes = dir.join("themes");
        let store = ThemeStore::start(None, themes.clone(), dir.join("config.toml"), String::new());
        store.save(theme("saved-at-shutdown")).unwrap();
        drop_with_timeout(store);
        assert!(themes.join("saved-at-shutdown.toml").is_file());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn failed_worker_start_degrades_without_hanging_startup_catalog() {
        let (request, _request_rx) = mpsc::channel();
        let (result_tx, results) = tokio_mpsc::unbounded_channel();
        drop(result_tx);
        let store = ThemeStore::from_worker_spawn(
            Err(std::io::Error::other("simulated spawn failure")),
            request,
            results,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(String::new())),
        );
        assert_eq!(
            store.save(theme("unavailable")).unwrap_err(),
            "theme store worker is unavailable"
        );

        let mut store = store;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let (themes, warnings) = runtime.block_on(store.initial_catalog());
        assert!(themes.is_empty());
        assert!(warnings.iter().any(|warning| warning.contains("stopped")));
    }
}
