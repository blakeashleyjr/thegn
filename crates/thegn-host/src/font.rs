use crate::palette::PaletteItem;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const RECOMMENDED_FONTS: &[&str] = &[
    "VictorMono Nerd Font",
    "JetBrainsMono Nerd Font",
    "CaskaydiaCove Nerd Font",
    "SauceCodePro Nerd Font",
    "Monoid Nerd Font",
    "Iosevka Nerd Font",
    "Inconsolata Nerd Font",
    "Hack Nerd Font",
    "FiraCode Nerd Font",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FontRow {
    pub family: String,
    pub label: String,
}

/// Total wall-clock budget for the `fc-list` helper.
const FC_DEADLINE: Duration = Duration::from_secs(3);
/// Stdout byte cap for the helper; the helper is killed when it is reached.
const FC_MAX_OUTPUT: usize = 4 * 1024 * 1024;
/// Wall-clock budget for the directory fallback.
const SCAN_DEADLINE: Duration = Duration::from_secs(2);
/// Directory entries the fallback may inspect, across all roots.
const SCAN_MAX_ENTRIES: usize = 100_000;
/// Total bytes of entry names the fallback may inspect.
const SCAN_MAX_NAME_BYTES: usize = 8 * 1024 * 1024;
/// Families surfaced to the palette.
const MAX_RESULTS: usize = 5_000;
/// A single entry name longer than this is not a font file name.
const MAX_NAME_BYTES: usize = 512;

/// Why a discovery result is partial.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Truncation {
    /// `fc-list` produced more than [`FC_MAX_OUTPUT`] bytes.
    HelperOutput,
    /// The scan hit its entry or name-byte budget.
    ScanBudget,
    /// More than [`MAX_RESULTS`] families were found.
    Results,
}

/// The typed outcome of one discovery run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryStatus {
    Complete,
    Truncated(Truncation),
    /// No source could enumerate fonts; carries the actionable reason.
    Unavailable(String),
    TimedOut,
    Canceled,
}

/// Families plus how trustworthy the list is. `source_errors` names every source
/// that failed or was skipped, so a successful-looking list is never silently
/// incomplete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    pub rows: Vec<FontRow>,
    pub status: DiscoveryStatus,
    pub source_errors: Vec<String>,
}

impl Discovery {
    fn bare(status: DiscoveryStatus, source_errors: Vec<String>) -> Self {
        Self {
            rows: Vec::new(),
            status,
            source_errors,
        }
    }

    /// Palette rows for the discovered families.
    pub fn items(&self) -> Vec<PaletteItem> {
        self.rows
            .iter()
            .map(|row| PaletteItem::new(format!("font:{}", row.family), row.label.clone()))
            .collect()
    }

    /// One status line when the list is unusable or partial; `None` when a
    /// complete list should simply open.
    pub fn notice(&self) -> Option<String> {
        let errs = if self.source_errors.is_empty() {
            String::new()
        } else {
            format!(" ({})", self.source_errors.join("; "))
        };
        match &self.status {
            DiscoveryStatus::Complete if self.rows.is_empty() => {
                Some(format!("No fonts found{errs}"))
            }
            DiscoveryStatus::Complete => None,
            DiscoveryStatus::Truncated(t) => Some(format!(
                "Font list truncated ({}){errs}",
                match t {
                    Truncation::HelperOutput => "fc-list output cap",
                    Truncation::ScanBudget => "directory scan budget",
                    Truncation::Results => "result cap",
                }
            )),
            DiscoveryStatus::Unavailable(why) => Some(format!("Font list failed: {why}{errs}")),
            DiscoveryStatus::TimedOut => Some(format!("Font list timed out{errs}")),
            DiscoveryStatus::Canceled => Some("Font list canceled".into()),
        }
    }

    /// Whether the picker should open with whatever rows exist.
    pub fn usable(&self) -> bool {
        !self.rows.is_empty()
            && matches!(
                self.status,
                DiscoveryStatus::Complete | DiscoveryStatus::Truncated(_)
            )
    }
}

/// Run one bounded discovery. Blocking: call only from a worker thread.
pub fn discover(cancel: &AtomicBool) -> Discovery {
    let exe = resolve_fc_list(&fc_list_search_dirs(), std::env::var_os("PATH").as_deref());
    discover_with(exe.as_deref(), cancel, &macos_font_dirs())
}

fn discover_with(fc_list: Option<&Path>, cancel: &AtomicBool, dirs: &[PathBuf]) -> Discovery {
    let mut errors = Vec::new();
    let mut timed_out = false;
    match fc_list {
        Some(exe) => {
            let mut cmd = Command::new(exe);
            cmd.args([":", "family"]);
            match crate::preview_jobs::bounded_capture(&mut cmd, cancel, FC_DEADLINE, FC_MAX_OUTPUT)
            {
                Ok(bytes) => return finish_rows(&bytes, false, errors),
                Err(crate::preview_jobs::CaptureError::Capped(bytes)) => {
                    return finish_rows(&bytes, true, errors);
                }
                Err(crate::preview_jobs::CaptureError::Cancelled) => {
                    return Discovery::bare(DiscoveryStatus::Canceled, errors);
                }
                Err(crate::preview_jobs::CaptureError::Timeout) => {
                    timed_out = true;
                    errors.push("fc-list exceeded its deadline".into());
                }
                Err(crate::preview_jobs::CaptureError::Spawn(e)) => {
                    errors.push(format!("fc-list failed to start: {e}"));
                }
                Err(crate::preview_jobs::CaptureError::Failed) => {
                    errors.push("fc-list failed".into());
                }
            }
        }
        None => errors.push("fc-list not found in an admitted system directory".into()),
    }
    // Stock macOS has no fontconfig: fall back to the standard font directories.
    // Elsewhere there is nothing to fall back to, so surface the real reason.
    if cfg!(target_os = "macos") {
        let scan = font_rows_from_dirs(dirs, cancel);
        errors.extend(scan.errors);
        return match scan.stop {
            ScanStop::Canceled => Discovery::bare(DiscoveryStatus::Canceled, errors),
            ScanStop::Deadline => Discovery::bare(DiscoveryStatus::TimedOut, errors),
            ScanStop::Budget | ScanStop::Done => {
                let truncated = scan.stop == ScanStop::Budget;
                if scan.rows.is_empty() {
                    Discovery::bare(
                        DiscoveryStatus::Unavailable("no fonts found under ~/Library/Fonts".into()),
                        errors,
                    )
                } else {
                    cap_results(
                        scan.rows,
                        truncated.then_some(Truncation::ScanBudget),
                        errors,
                    )
                }
            }
        };
    }
    if timed_out {
        return Discovery::bare(DiscoveryStatus::TimedOut, errors);
    }
    let why = errors.first().cloned().unwrap_or_default();
    Discovery::bare(DiscoveryStatus::Unavailable(why), errors)
}

fn finish_rows(stdout: &[u8], output_capped: bool, errors: Vec<String>) -> Discovery {
    let mut text = String::from_utf8_lossy(stdout).into_owned();
    if output_capped {
        // The cut may land mid-line; drop the partial tail rather than list a
        // truncated family name.
        if let Some(i) = text.rfind('\n') {
            text.truncate(i);
        }
    }
    let rows = font_rows_from_fc_list(&text);
    cap_results(
        rows,
        output_capped.then_some(Truncation::HelperOutput),
        errors,
    )
}

fn cap_results(
    mut rows: Vec<FontRow>,
    truncation: Option<Truncation>,
    source_errors: Vec<String>,
) -> Discovery {
    let mut truncation = truncation;
    if rows.len() > MAX_RESULTS {
        rows.truncate(MAX_RESULTS);
        truncation.get_or_insert(Truncation::Results);
    }
    Discovery {
        rows,
        status: truncation.map_or(DiscoveryStatus::Complete, DiscoveryStatus::Truncated),
        source_errors,
    }
}

/// Well-known system and package-manager directories `fc-list` is resolved from
/// first, in order. Ambient `PATH` is only a filtered last resort
/// ([`resolve_fc_list`]).
fn fc_list_search_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from("/run/current-system/sw/bin")];
    if let Some(user) = std::env::var_os("USER") {
        dirs.push(
            PathBuf::from("/etc/profiles/per-user")
                .join(user)
                .join("bin"),
        );
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        dirs.push(home.join(".nix-profile/bin"));
        let state = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(".local/state"));
        dirs.push(state.join("nix/profile/bin"));
    }
    for d in [
        "/nix/var/nix/profiles/default/bin",
        "/usr/bin",
        "/usr/local/bin",
        "/opt/homebrew/bin",
        "/opt/local/bin",
        "/home/linuxbrew/.linuxbrew/bin",
        "/bin",
    ] {
        dirs.push(PathBuf::from(d));
    }
    dirs
}

/// First `fc-list` in `dirs` that passes the executable-identity admission; when
/// none does, the first absolute `path_var` entry holding an admitted one. PATH
/// entries get the same admission (regular, executable, not group/world-writable)
/// and relative entries are ignored.
fn resolve_fc_list(dirs: &[PathBuf], path_var: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let admitted = |d: &PathBuf| {
        let p = d.join("fc-list");
        crate::platform::exe_admitted(&p).then_some(p)
    };
    dirs.iter().find_map(admitted).or_else(|| {
        let path_var = path_var?;
        std::env::split_paths(path_var)
            .filter(|d| d.is_absolute())
            .find_map(|d| admitted(&d))
    })
}

/// Single-flight owner of font discovery. One worker at a time; a request made
/// while one is running coalesces into it. Dropping the owner cancels the run.
#[derive(Default)]
pub struct DiscoveryOwner {
    generation: u64,
    in_flight: bool,
    cancel: Arc<AtomicBool>,
}

/// A finished run, tagged with the generation that requested it.
pub struct DiscoveryResult {
    pub generation: u64,
    pub discovery: Discovery,
}

/// What [`DiscoveryOwner::request`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    Started,
    /// A run is already in flight; this request joined it.
    Coalesced,
    /// The worker thread could not be spawned.
    Failed,
}

impl DiscoveryOwner {
    /// Start a run unless one is already in flight.
    pub fn request(
        &mut self,
        tx: tokio::sync::mpsc::UnboundedSender<DiscoveryResult>,
        waker: termwiz::terminal::TerminalWaker,
    ) -> Request {
        if !self.begin() {
            return Request::Coalesced;
        }
        let generation = self.generation;
        let cancel = Arc::clone(&self.cancel);
        let spawned = std::thread::Builder::new()
            .name("thegn-font-discovery".into())
            .spawn(move || {
                crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
                // A panic must still deliver a result, or `in_flight` would stay
                // set and every later request would coalesce into nothing.
                let discovery =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| discover(&cancel)))
                        .unwrap_or_else(|_| {
                            Discovery::bare(
                                DiscoveryStatus::Unavailable("discovery panicked".into()),
                                Vec::new(),
                            )
                        });
                if tx
                    .send(DiscoveryResult {
                        generation,
                        discovery,
                    })
                    .is_ok()
                {
                    let _ = waker.wake(); // best-effort: waker pulse: a nudge must never fail the worker
                }
            });
        if spawned.is_err() {
            self.in_flight = false;
            return Request::Failed;
        }
        Request::Started
    }

    fn begin(&mut self) -> bool {
        if self.in_flight {
            return false;
        }
        self.generation += 1;
        self.in_flight = true;
        self.cancel = Arc::new(AtomicBool::new(false));
        true
    }

    /// Accept a result only for the current in-flight generation.
    pub fn accept(&mut self, generation: u64) -> bool {
        if self.in_flight && generation == self.generation {
            self.in_flight = false;
            true
        } else {
            false
        }
    }

    #[cfg(test)]
    fn in_flight(&self) -> bool {
        self.in_flight
    }
}

impl Drop for DiscoveryOwner {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

/// The three directories macOS resolves fonts from, user-first.
fn macos_font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join("Library/Fonts"));
    }
    dirs.push(PathBuf::from("/Library/Fonts"));
    dirs.push(PathBuf::from("/System/Library/Fonts"));
    dirs
}

/// How deep to descend under each font directory.
///
/// macOS resolves its font directories **recursively**, and a flat `read_dir`
/// misses most of what is actually installed. Measured on macOS 26:
///   * depth 1 — `/System/Library/Fonts/Supplemental/`, where the bulk of the
///     shipped system faces live: 81 → 370 files;
///   * depth 6 — nix-darwin's `/Library/Fonts/Nix Fonts/<hash>-<pkg>/share/
///     fonts/opentype/`;
///   * depth 8 — the Nerd Font packages' extra `truetype/NerdFonts/<Family>/`
///     nesting. This is where FiraCode Nerd Font actually sits, so a flat scan
///     reported **zero** of `RECOMMENDED_FONTS` as available on a machine that
///     had one installed.
///
/// 8 is a real bound, not "deep enough for now": the deepest font on that
/// machine is at 8, and the whole walk costs ~0.5 ms (vs 0.15 ms flat) on the
/// explicit `SwitchFont` action only. The cap is what keeps a font picker from
/// becoming a filesystem walk if someone points a font dir at their home.
const FONT_SCAN_DEPTH: usize = 8;

/// Derive families from font FILENAMES under `dirs` — the fontconfig-free
/// fallback, bounded by entry, name-byte, time and cancel budgets. Descends [`FONT_SCAN_DEPTH`] levels; see there for why flat was wrong.
///
/// Reading real family names would mean parsing each font's `name` table; the
/// filename is a good enough key here because the only consumer writes the
/// chosen string into an alacritty `font.normal.family`, and Nerd Font
/// distributions name their files after the family they register. A style suffix
/// (`-Regular`, ` Bold Italic`) is stripped so all faces of a family collapse to
/// one entry, matching what `fc-list : family` yields.
fn font_rows_from_dirs(dirs: &[PathBuf], cancel: &AtomicBool) -> Scan {
    let mut st = ScanState {
        families: BTreeSet::new(),
        errors: Vec::new(),
        entries: 0,
        name_bytes: 0,
        started: Instant::now(),
        deadline: SCAN_DEADLINE,
        max_entries: SCAN_MAX_ENTRIES,
        stop: ScanStop::Done,
    };
    font_rows_from_dirs_in(dirs, cancel, &mut st)
}

fn font_rows_from_dirs_in(dirs: &[PathBuf], cancel: &AtomicBool, st: &mut ScanState) -> Scan {
    for dir in dirs {
        collect_font_families(dir, FONT_SCAN_DEPTH, cancel, st);
        if st.stop != ScanStop::Done {
            break;
        }
    }
    Scan {
        rows: rank_families(std::mem::take(&mut st.families)),
        errors: std::mem::take(&mut st.errors),
        stop: st.stop,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanStop {
    Done,
    Budget,
    Deadline,
    Canceled,
}

struct Scan {
    rows: Vec<FontRow>,
    errors: Vec<String>,
    stop: ScanStop,
}

struct ScanState {
    families: BTreeSet<String>,
    errors: Vec<String>,
    entries: usize,
    name_bytes: usize,
    started: Instant,
    deadline: Duration,
    max_entries: usize,
    stop: ScanStop,
}

impl ScanState {
    /// Charge one entry against every budget; `false` stops the walk.
    fn admit(&mut self, name_len: usize, cancel: &AtomicBool) -> bool {
        if self.stop != ScanStop::Done {
            return false;
        }
        if cancel.load(Ordering::Acquire) {
            self.stop = ScanStop::Canceled;
        } else if self.started.elapsed() >= self.deadline {
            self.stop = ScanStop::Deadline;
        } else {
            self.entries += 1;
            self.name_bytes += name_len;
            if self.entries > self.max_entries || self.name_bytes > SCAN_MAX_NAME_BYTES {
                self.stop = ScanStop::Budget;
            }
        }
        self.stop == ScanStop::Done
    }

    fn note_unreadable(&mut self, dir: &Path, e: &std::io::Error) {
        // A standard font directory that simply does not exist is normal.
        if e.kind() != std::io::ErrorKind::NotFound && self.errors.len() < 8 {
            self.errors.push(format!("{}: {e}", dir.display()));
        }
    }
}

/// Add every font family found in `dir` to the state, descending at most
/// `depth` more levels and within the entry, name-byte, time and cancel budgets.
/// A missing directory is skipped silently; any other read failure is recorded
/// in `st.errors` so the picker can say the list may be incomplete.
fn collect_font_families(dir: &Path, depth: usize, cancel: &AtomicBool, st: &mut ScanState) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            st.note_unreadable(dir, &e);
            return;
        }
    };
    for ent in entries {
        let ent = match ent {
            Ok(ent) => ent,
            Err(e) => {
                st.note_unreadable(dir, &e);
                continue;
            }
        };
        let name = ent.file_name();
        let name = name.to_string_lossy();
        if !st.admit(name.len(), cancel) {
            return;
        }
        if name.len() > MAX_NAME_BYTES {
            continue;
        }
        // `file_type` avoids a stat per entry on the common (file) path and,
        // unlike `is_dir`, does not follow symlinks — a font dir that links to
        // itself must not send this into a loop.
        if ent.file_type().is_ok_and(|t| t.is_dir()) {
            if depth > 0 {
                collect_font_families(&ent.path(), depth - 1, cancel, st);
                if st.stop != ScanStop::Done {
                    return;
                }
            }
            continue;
        }
        let Some((stem, ext)) = name.rsplit_once('.') else {
            continue;
        };
        if !matches!(
            ext.to_ascii_lowercase().as_str(),
            "ttf" | "otf" | "ttc" | "dfont"
        ) {
            continue;
        }
        let family = family_from_font_filename(stem);
        if validate_family(&family).is_ok() && !is_short_nerd_font_alias(&family) {
            st.families.insert(family);
        }
    }
}

/// Strip a style suffix from a font filename stem, yielding the family.
/// `"JetBrainsMonoNerdFont-BoldItalic"` → `"JetBrainsMonoNerdFont"`.
fn family_from_font_filename(stem: &str) -> String {
    const STYLES: &[&str] = &[
        "thin",
        "extralight",
        "ultralight",
        "light",
        "regular",
        "book",
        "medium",
        "semibold",
        "demibold",
        "bold",
        "extrabold",
        "black",
        "heavy",
        "italic",
        "oblique",
    ];
    // Split on the last '-' (the near-universal `Family-Style` convention) and
    // drop the tail only when every word in it is a style token, so a family
    // that legitimately contains a hyphen survives.
    let base = match stem.rsplit_once('-') {
        Some((head, tail)) if !head.is_empty() && is_all_styles(tail, STYLES) => head,
        _ => stem,
    };
    base.trim().to_string()
}

/// Whether `s` is made up entirely of style words (camelCase or space/underscore
/// separated), e.g. `"BoldItalic"`, `"Semi Bold"`, `"regular"`.
fn is_all_styles(s: &str, styles: &[&str]) -> bool {
    let lower = s.to_ascii_lowercase();
    let mut rest = lower.replace([' ', '_'], "");
    if rest.is_empty() {
        return false;
    }
    while !rest.is_empty() {
        // Longest match first, so "extrabold" isn't consumed as "bold".
        let Some(hit) = styles
            .iter()
            .filter(|st| rest.starts_with(**st))
            .max_by_key(|st| st.len())
        else {
            return false;
        };
        rest = rest[hit.len()..].to_string();
    }
    true
}

pub fn font_rows_from_fc_list(fc_list: &str) -> Vec<FontRow> {
    let mut families = BTreeSet::new();
    for line in fc_list.lines() {
        let rest = line.split_once(':').map(|(_, rest)| rest).unwrap_or(line);
        let family_segment = rest
            .split_once(":style=")
            .map(|(families, _)| families)
            .unwrap_or(rest);
        for family in family_segment.split(',').map(str::trim) {
            if family.is_empty() || is_short_nerd_font_alias(family) {
                continue;
            }
            families.insert(family.to_string());
        }
    }
    rank_families(families)
}

/// Label + order a deduped family set: recommended fonts first (in
/// `RECOMMENDED_FONTS` order), then everything else case-insensitively. Shared by
/// both enumeration paths so the picker looks identical either way.
fn rank_families(families: BTreeSet<String>) -> Vec<FontRow> {
    let recommended_order: BTreeMap<String, usize> = RECOMMENDED_FONTS
        .iter()
        .enumerate()
        .map(|(idx, name)| (normalize_family(name), idx))
        .collect();

    let mut rows: Vec<_> = families
        .into_iter()
        .filter(|family| validate_family(family).is_ok())
        .map(|family| {
            let recommended_idx = recommended_order.get(&normalize_family(&family)).copied();
            let label = if recommended_idx.is_some() {
                format!("★ Recommended — {family}")
            } else {
                family.clone()
            };
            (
                recommended_idx,
                family.to_ascii_lowercase(),
                FontRow { family, label },
            )
        })
        .collect();
    rows.sort_by(|a, b| match (a.0, b.0) {
        (Some(ai), Some(bi)) => ai.cmp(&bi).then_with(|| a.1.cmp(&b.1)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.1.cmp(&b.1),
    });
    rows.into_iter().map(|(_, _, row)| row).collect()
}

/// The Alacritty config the font picker writes to: `$THEGN_ALACRITTY_CONFIG`
/// (set by the `.app` launcher, but only when Alacritty is the terminal that
/// actually runs), else Alacritty's own XDG location.
///
/// `None` when neither a usable override nor an existing XDG config exists —
/// the caller must decline rather than guess. An absolute override is returned
/// even when its file is missing so a typo is reported against that path. Empty
/// and relative overrides are ignored; a relative path must never resolve
/// against the process CWD, which may be whichever worktree tab is focused.
pub fn alacritty_config_path() -> Option<PathBuf> {
    alacritty_config_path_with(
        std::env::var_os("THEGN_ALACRITTY_CONFIG"),
        &thegn_core::util::xdg_config_home(),
        |path| path.is_file(),
    )
}

fn alacritty_config_path_with(
    env: Option<OsString>,
    xdg_config_home: &Path,
    is_file: impl FnOnce(&Path) -> bool,
) -> Option<PathBuf> {
    let is_override = env
        .as_ref()
        .is_some_and(|value| PathBuf::from(value).is_absolute());
    let candidate = alacritty_config_path_from(env, xdg_config_home)?;
    if is_override {
        Some(candidate)
    } else {
        is_file(&candidate).then_some(candidate)
    }
}

/// Use only absolute overrides. Ignore empty and relative values, falling back
/// to Alacritty's XDG location so the process CWD can never select the config.
fn alacritty_config_path_from(env: Option<OsString>, xdg_config_home: &Path) -> Option<PathBuf> {
    if let Some(p) = env.map(PathBuf::from).filter(|p| p.is_absolute()) {
        return Some(p);
    }
    // Alacritty reads `$XDG_CONFIG_HOME/alacritty/alacritty.toml` on every
    // platform (and also `~/.config/...` on macOS, which is where thegn's own
    // `xdg_config_home` points by default).
    Some(xdg_config_home.join("alacritty/alacritty.toml"))
}

/// The terminal the font picker is being asked to reconfigure.
///
/// Resolved from the terminal that is **actually running**, not the one the
/// installer happened to pick: `TERM_PROGRAM`, falling back to `LC_TERMINAL`
/// (which survives ssh) and then `TERM`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalKind {
    Alacritty,
    Ghostty,
    Kitty,
    /// Identified, but its font cannot be set by editing a text config:
    /// WezTerm's is Lua (a parse-and-rewrite problem with no safe fallback),
    /// and Terminal.app / iTerm2 keep theirs in a plist or dynamic profile that
    /// needs `defaults write` plus a restart. Declining with instructions beats
    /// half-editing someone's config.
    Unsupported(&'static str),
    Unknown,
}

/// Identify the running terminal from its own environment.
pub fn detect_terminal(term_program: Option<&str>, term: Option<&str>) -> TerminalKind {
    let hay = format!(
        "{} {}",
        term_program.unwrap_or("").to_ascii_lowercase(),
        term.unwrap_or("").to_ascii_lowercase()
    );
    // Order matters only in that each name is distinctive; no substring of one
    // terminal's name appears in another's.
    if hay.contains("alacritty") {
        TerminalKind::Alacritty
    } else if hay.contains("ghostty") {
        TerminalKind::Ghostty
    } else if hay.contains("kitty") {
        TerminalKind::Kitty
    } else if hay.contains("wezterm") {
        TerminalKind::Unsupported("WezTerm")
    } else if hay.contains("iterm") {
        TerminalKind::Unsupported("iTerm2")
    } else if hay.contains("apple_terminal") {
        TerminalKind::Unsupported("Terminal.app")
    } else {
        TerminalKind::Unknown
    }
}

/// How to tell a user to set the font themselves, for a terminal thegn will not
/// edit. Actionable rather than apologetic — the exact line, in the exact file.
fn manual_instructions(name: &str, family: &str) -> String {
    match name {
        "WezTerm" => format!(
            "thegn can't set WezTerm's font (its config is Lua). Add to ~/.wezterm.lua:              config.font = wezterm.font('{family}')"
        ),
        "iTerm2" => {
            format!("thegn can't set iTerm2's font. Settings → Profiles → Text → Font → {family}")
        }
        _ => format!(
            "thegn can't set Terminal.app's font. Settings → Profiles → Text → Font → {family}"
        ),
    }
}

/// Ghostty's config file. macOS keeps it under Application Support; every other
/// platform uses XDG.
fn ghostty_config_path() -> PathBuf {
    if cfg!(target_os = "macos")
        && let Some(home) = std::env::var_os("HOME")
    {
        let p =
            PathBuf::from(&home).join("Library/Application Support/com.mitchellh.ghostty/config");
        if p.is_file() {
            return p;
        }
    }
    thegn_core::util::xdg_config_home().join("ghostty/config")
}

fn kitty_config_path() -> PathBuf {
    thegn_core::util::xdg_config_home().join("kitty/kitty.conf")
}

/// Set the terminal's font family, for whichever terminal is actually running.
///
/// Previously this always patched an Alacritty config — 4th of the 5 terminals
/// the macOS `.app` launcher will start, and not the one it prefers — so on a
/// Ghostty session it edited a file nothing was reading and reported success.
pub fn apply_font_family(family: &str) -> Result<PathBuf, String> {
    let env = thegn_core::termcaps::TermEnv::from_env();
    apply_font_family_for(
        detect_terminal(env.program_name(), env.term.as_deref()),
        family,
    )
}

/// [`apply_font_family`] with the terminal chosen by the caller — the seam the
/// tests drive.
pub fn apply_font_family_for(kind: TerminalKind, family: &str) -> Result<PathBuf, String> {
    validate_family(family)?;
    match kind {
        TerminalKind::Alacritty => {
            let path = alacritty_config_path().ok_or_else(|| {
                "no Alacritty config found — set THEGN_ALACRITTY_CONFIG, or create \
                 ~/.config/alacritty/alacritty.toml"
                    .to_string()
            })?;
            apply_font_family_to_path(&path, family)?;
            Ok(path)
        }
        TerminalKind::Ghostty => {
            let path = ghostty_config_path();
            write_simple_key(
                &path,
                "font-family",
                &format!("font-family = {family}"),
                family,
            )?;
            Ok(path)
        }
        TerminalKind::Kitty => {
            let path = kitty_config_path();
            write_simple_key(
                &path,
                "font_family",
                &format!("font_family {family}"),
                family,
            )?;
            Ok(path)
        }
        TerminalKind::Unsupported(name) => Err(manual_instructions(name, family)),
        TerminalKind::Unknown => Err(format!(
            "thegn doesn't know how to set the font for this terminal — set it to \
             {family} yourself"
        )),
    }
}

/// Patch a `key value` / `key = value` line-oriented config (Ghostty, kitty),
/// appending it when absent. Creates the file (and its directory) if needed —
/// both terminals treat a missing config as empty defaults, so writing one is
/// the same as editing it.
fn write_simple_key(path: &Path, key: &str, new_line: &str, family: &str) -> Result<(), String> {
    validate_family(family)?;
    let (observed, observed_exists) = match read_config(path) {
        Ok(bytes) => (bytes, true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Vec::new(), false),
        Err(e) => return Err(config_read_error(path, e)),
    };
    let current = String::from_utf8(observed.clone())
        .map_err(|e| format!("read {} failed: {e}", path.display()))?;
    let mut out: Vec<String> = Vec::new();
    let mut replaced = false;
    for line in current.lines() {
        let t = line.trim_start();
        // Only an active (uncommented) assignment of this key is replaced, so a
        // commented example in the user's config stays as documentation.
        let is_key = !t.starts_with('#')
            && t.starts_with(key)
            && t[key.len()..].starts_with(|c: char| c.is_whitespace() || c == '=');
        if is_key && !replaced {
            out.push(new_line.to_string());
            replaced = true;
        } else if !is_key {
            out.push(line.to_string());
        }
    }
    if !replaced {
        out.push(new_line.to_string());
    }
    let mut body = out.join("\n");
    body.push('\n');
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("create {} failed: {e}", dir.display()))?;
    }
    replace_config(path, &observed, observed_exists, body.as_bytes())
}

fn apply_font_family_to_path(path: &Path, family: &str) -> Result<(), String> {
    let observed = read_config(path).map_err(|e| config_read_error(path, e))?;
    let current = String::from_utf8(observed.clone())
        .map_err(|e| format!("read {} failed: {e}", path.display()))?;
    let patched = patch_alacritty_font_family(&current, family)?;
    replace_config(path, &observed, true, patched.as_bytes())
}

fn read_config(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut file = thegn_core::fsperm::open_regular_file_nofollow(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn config_read_error(path: &Path, error: std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::InvalidInput
        && std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        format!(
            "{} is a symlink (managed dotfile?) — edit its target",
            path.display()
        )
    } else {
        format!("read {} failed: {error}", path.display())
    }
}

/// Publish only if the no-follow destination still has the bytes we observed.
/// Existing destinations must be owned by this user and have one link; refuse
/// ambiguity instead of breaking a hard link. Staging names are unpredictable
/// and exclusively created, and destination permissions are applied to the
/// staged file before the atomic rename.
fn replace_config(
    path: &Path,
    observed: &[u8],
    observed_exists: bool,
    new: &[u8],
) -> Result<(), String> {
    replace_config_with(
        path,
        observed,
        observed_exists,
        new,
        &mut random_staging_suffix,
    )
}

fn replace_config_with(
    path: &Path,
    observed: &[u8],
    observed_exists: bool,
    new: &[u8],
    suffix: &mut dyn FnMut() -> Result<String, String>,
) -> Result<(), String> {
    let (current, metadata) = match thegn_core::fsperm::open_regular_file_nofollow(path) {
        Ok(mut file) => {
            let metadata = file
                .metadata()
                .map_err(|e| format!("inspect {} failed: {e}", path.display()))?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)
                .map_err(|e| format!("read {} failed: {e}", path.display()))?;
            (bytes, Some(metadata))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !observed_exists => {
            (Vec::new(), None)
        }
        Err(e) => return Err(config_read_error(path, e)),
    };
    if current != observed || metadata.is_some() != observed_exists {
        return Err(format!("{} changed while editing; retry", path.display()));
    }
    if metadata
        .as_ref()
        .is_some_and(|metadata| metadata.permissions().readonly())
    {
        return Err(format!("{} is read-only; not modified", path.display()));
    }
    if let Some(metadata) = metadata.as_ref()
        && !thegn_core::fsperm::owned_by_current_user(metadata)
    {
        return Err(format!(
            "{} is owned by another user; not modified (edit it as its owner)",
            path.display()
        ));
    }
    if let Some(metadata) = metadata.as_ref()
        && thegn_core::fsperm::ensure_single_link_regular_file(metadata).is_err()
    {
        return Err(format!(
            "{} has other hard links; not modified (edit it directly, or remove the extra link)",
            path.display()
        ));
    }

    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    let temporary = stage_sibling(
        path,
        new,
        metadata.as_ref().map(|metadata| metadata.permissions()),
        suffix,
    )?;
    if let Err(error) = thegn_core::fsperm::rename_replace(&temporary, path) {
        let cleanup = std::fs::remove_file(&temporary);
        return match cleanup {
            Ok(()) => Err(format!("replace {} failed: {error}", path.display())),
            Err(cleanup_error) => Err(format!(
                "replace {} failed: {error}; remove staging file {} failed: {cleanup_error}",
                path.display(),
                temporary.display()
            )),
        };
    }
    thegn_core::fsperm::sync_parent_dir(parent).map_err(|e| {
        format!(
            "{} was replaced, but syncing {} failed: {e}",
            path.display(),
            parent.display()
        )
    })?;
    Ok(())
}

/// The staging create retries collisions without ever replacing an existing entry.
const STAGING_ATTEMPTS: usize = 8;

fn random_staging_suffix() -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| format!("no randomness for a staging name: {error}"))?;
    let mut suffix = String::with_capacity(32);
    for byte in bytes {
        suffix.push_str(&format!("{byte:02x}"));
    }
    Ok(suffix)
}

fn staging_cleanup_error(path: &Path, temporary: &Path, error: std::io::Error) -> String {
    match std::fs::remove_file(temporary) {
        Ok(()) => format!("stage {} failed: {error}; not modified", path.display()),
        Err(cleanup_error) => format!(
            "stage {} failed: {error}; remove staging file {} failed: {cleanup_error}; not modified",
            path.display(),
            temporary.display()
        ),
    }
}

fn stage_sibling(
    path: &Path,
    new: &[u8],
    permissions: Option<std::fs::Permissions>,
    suffix: &mut dyn FnMut() -> Result<String, String>,
) -> Result<PathBuf, String> {
    use std::io::Write as _;

    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| format!("{} has no file name", path.display()))?
        .to_string_lossy();

    for _ in 0..STAGING_ATTEMPTS {
        let random =
            suffix().map_err(|error| format!("stage {} failed: {error}", path.display()))?;
        let temporary = parent.join(format!(".{name}.tmp-{random}"));
        let mut file = match thegn_core::fsperm::create_owner_only_exclusive(&temporary) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("stage {} failed: {error}", path.display())),
        };
        if let Err(error) = file.write_all(new) {
            drop(file);
            return Err(staging_cleanup_error(path, &temporary, error));
        }
        if let Some(permissions) = permissions
            && let Err(error) = file.set_permissions(permissions)
        {
            drop(file);
            return Err(staging_cleanup_error(path, &temporary, error));
        }
        if let Err(error) = file.sync_all() {
            drop(file);
            return Err(staging_cleanup_error(path, &temporary, error));
        }
        return Ok(temporary);
    }

    Err(format!(
        "stage {} failed: could not create a unique staging file after {STAGING_ATTEMPTS} attempts",
        path.display()
    ))
}

pub fn patch_alacritty_font_family(input: &str, family: &str) -> Result<String, String> {
    validate_family(family)?;
    let escaped = family.replace('\\', "\\\\").replace('"', "\\\"");
    let mut changed = false;
    let mut out = Vec::new();
    for line in input.lines() {
        let indent_len = line.len() - line.trim_start().len();
        let indent = &line[..indent_len];
        let trimmed = line.trim_start();
        if !trimmed.starts_with('#') && trimmed.starts_with("normal = { family = ") {
            out.push(format!("{indent}normal = {{ family = \"{escaped}\" }}"));
            changed = true;
        } else {
            out.push(line.to_string());
        }
    }
    if !changed {
        return Err("no alacritty [font] normal.family line found".into());
    }
    let mut rendered = out.join("\n");
    if input.ends_with('\n') {
        rendered.push('\n');
    }
    Ok(rendered)
}

fn validate_family(family: &str) -> Result<(), String> {
    if family.trim().is_empty() {
        return Err("font family must not be empty or whitespace-only".into());
    }
    if family.chars().any(char::is_control) {
        return Err("font family must not contain control characters".into());
    }
    if family.len() > 256 {
        return Err("font family must be no more than 256 bytes".into());
    }
    Ok(())
}

fn normalize_family(family: &str) -> String {
    family
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn is_short_nerd_font_alias(family: &str) -> bool {
    let lower = family.to_ascii_lowercase();
    lower.ends_with(" nf") || lower.ends_with(" nfm") || lower.ends_with(" nfp")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_filenames_collapse_their_style_suffix_to_one_family() {
        // All faces of a family collapse to the same key…
        for stem in [
            "JetBrainsMonoNerdFont-Regular",
            "JetBrainsMonoNerdFont-Bold",
            "JetBrainsMonoNerdFont-BoldItalic",
            "JetBrainsMonoNerdFont-ExtraLight",
            "JetBrainsMonoNerdFont-Thin",
        ] {
            assert_eq!(
                family_from_font_filename(stem),
                "JetBrainsMonoNerdFont",
                "stem: {stem}"
            );
        }
        // …a bare family keeps its name…
        assert_eq!(family_from_font_filename("Menlo"), "Menlo");
        // …and a hyphen that is NOT a style suffix must survive, or families
        // like these would be silently truncated.
        assert_eq!(
            family_from_font_filename("Noto-Sans-Mono"),
            "Noto-Sans-Mono"
        );
        assert_eq!(family_from_font_filename("SF-Mono"), "SF-Mono");
    }

    #[test]
    fn style_suffix_detection_matches_longest_token_first() {
        const STYLES: &[&str] = &["bold", "extrabold", "italic", "regular", "semibold"];
        // "extrabold" must not be consumed as "extra" + "bold" (no "extra" token)
        // nor leave a dangling remainder.
        assert!(is_all_styles("ExtraBold", STYLES));
        assert!(is_all_styles("BoldItalic", STYLES));
        assert!(is_all_styles("Semi_Bold", STYLES) || is_all_styles("SemiBold", STYLES));
        assert!(!is_all_styles("Mono", STYLES));
        assert!(!is_all_styles("BoldMono", STYLES));
        assert!(!is_all_styles("", STYLES));
    }

    #[test]
    fn terminal_detection_prefers_the_running_terminal() {
        use TerminalKind::*;
        // TERM_PROGRAM is the first-hand answer…
        assert_eq!(
            detect_terminal(Some("ghostty"), Some("xterm-256color")),
            Ghostty
        );
        assert_eq!(
            detect_terminal(Some("Apple_Terminal"), None),
            Unsupported("Terminal.app")
        );
        assert_eq!(
            detect_terminal(Some("iTerm.app"), None),
            Unsupported("iTerm2")
        );
        assert_eq!(
            detect_terminal(Some("WezTerm"), None),
            Unsupported("WezTerm")
        );
        // …and TERM carries it when TERM_PROGRAM does not (kitty, alacritty).
        assert_eq!(detect_terminal(None, Some("xterm-kitty")), Kitty);
        assert_eq!(detect_terminal(None, Some("alacritty")), Alacritty);
        // Nothing recognisable ⇒ decline rather than guess a config to edit.
        assert_eq!(detect_terminal(None, Some("xterm-256color")), Unknown);
        assert_eq!(detect_terminal(None, None), Unknown);
    }

    #[test]
    fn unsupported_terminals_decline_with_instructions_and_write_nothing() {
        // WezTerm's config is Lua and Terminal.app/iTerm2 keep theirs in a
        // plist — half-editing someone's config is worse than declining, so
        // these must fail with the exact line to add themselves.
        for (kind, needle) in [
            (TerminalKind::Unsupported("WezTerm"), "wezterm.font"),
            (TerminalKind::Unsupported("iTerm2"), "Profiles"),
            (TerminalKind::Unsupported("Terminal.app"), "Profiles"),
        ] {
            let err = apply_font_family_for(kind, "Hack Nerd Font").unwrap_err();
            assert!(err.contains("Hack Nerd Font"), "{err}");
            assert!(err.contains(needle), "{err}");
        }
        let err = apply_font_family_for(TerminalKind::Unknown, "Hack Nerd Font").unwrap_err();
        assert!(err.contains("Hack Nerd Font"), "{err}");
    }

    #[test]
    fn simple_key_configs_are_patched_in_place_and_created_when_absent() {
        let tmp = tempfile::tempdir().unwrap();

        // Ghostty: replace the ACTIVE assignment, leave a commented example as
        // documentation, and preserve every unrelated line.
        let g = tmp.path().join("ghostty/config");
        std::fs::create_dir_all(g.parent().unwrap()).unwrap();
        std::fs::write(
            &g,
            "# font-family = Old Example\nfont-family = Menlo\ntheme = dark\n",
        )
        .unwrap();
        write_simple_key(
            &g,
            "font-family",
            "font-family = Hack Nerd Font",
            "Hack Nerd Font",
        )
        .unwrap();
        let out = std::fs::read_to_string(&g).unwrap();
        assert!(out.contains("font-family = Hack Nerd Font"), "{out}");
        assert!(!out.contains("font-family = Menlo"), "{out}");
        assert!(
            out.contains("# font-family = Old Example"),
            "comment kept: {out}"
        );
        assert!(out.contains("theme = dark"), "unrelated keys kept: {out}");
        // Idempotent: applying twice must not accumulate duplicate keys.
        write_simple_key(
            &g,
            "font-family",
            "font-family = Hack Nerd Font",
            "Hack Nerd Font",
        )
        .unwrap();
        let out2 = std::fs::read_to_string(&g).unwrap();
        assert_eq!(out2.matches("font-family = Hack").count(), 1, "{out2}");

        // kitty uses `key value` with no `=`, and a missing file is the same as
        // an empty one — both terminals treat absence as defaults.
        let k = tmp.path().join("kitty/kitty.conf");
        write_simple_key(
            &k,
            "font_family",
            "font_family Iosevka Nerd Font",
            "Iosevka Nerd Font",
        )
        .unwrap();
        let out = std::fs::read_to_string(&k).unwrap();
        assert_eq!(out.trim(), "font_family Iosevka Nerd Font");
    }

    #[test]
    fn simple_key_read_failures_preserve_the_original_file() {
        let tmp = tempfile::tempdir().unwrap();
        let invalid_utf8 = tmp.path().join("invalid.conf");
        let original = [0xff, 0xfe, b'\n'];
        std::fs::write(&invalid_utf8, original).unwrap();

        let err =
            write_simple_key(&invalid_utf8, "font_family", "font_family Hack", "Hack").unwrap_err();
        assert!(err.starts_with("read "), "{err}");
        assert_eq!(std::fs::read(&invalid_utf8).unwrap(), original);

        let directory = tmp.path().join("is-a-directory");
        std::fs::create_dir(&directory).unwrap();
        let err =
            write_simple_key(&directory, "font_family", "font_family Hack", "Hack").unwrap_err();
        assert!(err.starts_with("read "), "{err}");
        assert!(directory.is_dir());
    }

    #[test]
    fn relative_alacritty_override_falls_through_to_xdg_path() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg = tmp.path();
        assert_eq!(
            alacritty_config_path_from(Some(OsString::from("config/alacritty.toml")), xdg),
            Some(xdg.join("alacritty/alacritty.toml"))
        );
        assert_eq!(
            alacritty_config_path_from(Some(OsString::new()), xdg),
            Some(xdg.join("alacritty/alacritty.toml"))
        );
        assert_eq!(
            alacritty_config_path_from(Some(xdg.join("override.toml").into_os_string()), xdg),
            Some(xdg.join("override.toml"))
        );
        let missing_override = xdg.join("missing-override.toml");
        assert_eq!(
            alacritty_config_path_with(
                Some(missing_override.clone().into_os_string()),
                xdg,
                |_| false,
            ),
            Some(missing_override)
        );
        assert_eq!(
            alacritty_config_path_with(None, xdg, |_| false),
            None,
            "a missing XDG default should still be ignored"
        );
    }

    #[test]
    fn config_replacement_refuses_symlinks_and_preserves_the_target() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target.conf");
        let link = tmp.path().join("config.conf");
        let original = b"font_family Menlo\n";
        std::fs::write(&target, original).unwrap();
        if thegn_core::fsperm::create_symlink(&target, &link, false).is_err() {
            return;
        }

        let error = write_simple_key(&link, "font_family", "font_family Hack", "Hack").unwrap_err();
        assert!(error.contains("symlink"), "{error}");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&target).unwrap(), original);
    }

    #[test]
    fn config_replacement_rejects_a_newer_observed_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("kitty.conf");
        let observed = b"font_family Menlo\n";
        let newer = b"font_family Iosevka\n";
        std::fs::write(&config, newer).unwrap();

        let error = replace_config(&config, observed, true, b"font_family Hack\n").unwrap_err();
        assert!(error.contains("changed while editing; retry"), "{error}");
        assert_eq!(std::fs::read(&config).unwrap(), newer);
    }

    #[test]
    fn config_replacement_distinguishes_absence_from_an_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("kitty.conf");
        std::fs::write(&config, b"").unwrap();

        let error = replace_config(&config, b"", false, b"font_family Hack\n").unwrap_err();
        assert!(error.contains("changed while editing; retry"), "{error}");
        assert_eq!(std::fs::read(&config).unwrap(), b"");
    }

    #[test]
    fn config_replacement_is_atomic_and_leaves_no_staging_sibling() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("kitty.conf");
        let observed = b"font_family Menlo\n";
        std::fs::write(&config, observed).unwrap();

        replace_config(&config, observed, true, b"font_family Hack\n").unwrap();

        assert_eq!(std::fs::read(&config).unwrap(), b"font_family Hack\n");
        assert!(std::fs::read_dir(tmp.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp-")
        }));
    }

    #[test]
    fn config_replacement_refuses_readonly_permissions() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("kitty.conf");
        let observed = b"font_family Menlo\n";
        std::fs::write(&config, observed).unwrap();
        let mut permissions = std::fs::metadata(&config).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&config, permissions).unwrap();
        let expected_permissions = std::fs::metadata(&config).unwrap().permissions();

        let error = replace_config(&config, observed, true, b"font_family Hack\n").unwrap_err();
        assert!(error.contains("is read-only; not modified"), "{error}");
        assert_eq!(std::fs::read(&config).unwrap(), observed);
        assert_eq!(
            std::fs::metadata(&config).unwrap().permissions(),
            expected_permissions
        );
    }

    #[test]
    fn config_replacement_refuses_a_hardlinked_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("kitty.conf");
        let other = tmp.path().join("kitty-copy.conf");
        let observed = b"font_family Menlo\n";
        std::fs::write(&config, observed).unwrap();
        std::fs::hard_link(&config, &other).unwrap();
        if thegn_core::fsperm::extra_hard_links(&std::fs::metadata(&config).unwrap()) == 0 {
            return;
        }

        let error = replace_config(&config, observed, true, b"font_family Hack\n").unwrap_err();

        assert!(error.contains("has other hard links"), "{error}");
        assert_eq!(std::fs::read(&config).unwrap(), observed);
        assert_eq!(std::fs::read(&other).unwrap(), observed);
        assert!(std::fs::read_dir(tmp.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp-")
        }));
    }

    #[test]
    fn config_replacement_refuses_a_destination_owned_by_another_user() {
        let path = Path::new("/etc/passwd");
        let Ok(mut file) = thegn_core::fsperm::open_regular_file_nofollow(path) else {
            return;
        };
        let Ok(metadata) = file.metadata() else {
            return;
        };
        if thegn_core::fsperm::owned_by_current_user(&metadata) {
            return;
        }
        let mut observed = Vec::new();
        if file.read_to_end(&mut observed).is_err() {
            return;
        }

        let result = replace_config(path, &observed, true, b"x");

        assert!(matches!(result, Err(ref error) if error.contains("owned by another user")));
    }

    #[test]
    fn config_replacement_preserves_writable_permissions_exactly() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("kitty.conf");
        let observed = b"font_family Menlo\n";
        std::fs::write(&config, observed).unwrap();
        let expected_permissions = std::fs::metadata(&config).unwrap().permissions();
        assert!(!expected_permissions.readonly());

        replace_config(&config, observed, true, b"font_family Hack\n").unwrap();

        assert_eq!(std::fs::read(&config).unwrap(), b"font_family Hack\n");
        assert_eq!(
            std::fs::metadata(&config).unwrap().permissions(),
            expected_permissions
        );
    }

    #[test]
    fn discovery_drops_control_bearing_families() {
        let rows = font_rows_from_fc_list(
            "/f.ttf: Evil\u{1b}]0;x\u{7}Name:style=Regular\n/g.ttf: Hack:style=Regular\n",
        );
        assert_eq!(
            rows.iter()
                .map(|row| row.family.as_str())
                .collect::<Vec<_>>(),
            ["Hack"]
        );

        let tmp = tempfile::tempdir().unwrap();
        let hostile = tmp.path().join("Evil\tFamily-Regular.ttf");
        if std::fs::write(&hostile, b"").is_ok() {
            let scan = font_rows_from_dirs(&[tmp.path().to_path_buf()], &AtomicBool::new(false));
            assert!(scan.rows.is_empty(), "{:?}", scan.rows);
        }
    }

    #[test]
    fn font_family_control_characters_are_rejected_without_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("kitty.conf");
        let original = b"font_family Menlo\nbackground #101010\n";
        std::fs::write(&config, original).unwrap();

        for family in ["Hack\ninclude /tmp/evil", "A\rB", "A\u{1b}B"] {
            let err = validate_family(family).unwrap_err();
            assert!(err.contains("control characters"), "{err}");
            assert!(!err.contains('\n') && !err.contains('\r') && !err.contains('\u{1b}'));

            let err = write_simple_key(
                &config,
                "font_family",
                &format!("font_family {family}"),
                family,
            )
            .unwrap_err();
            assert!(err.contains("control characters"), "{err}");
            assert_eq!(std::fs::read(&config).unwrap(), original);
        }

        let input = "[font]\nnormal = { family = \"Menlo\" }\n";
        for family in ["A\rB", "A\u{1b}B"] {
            let err = patch_alacritty_font_family(input, family).unwrap_err();
            assert!(err.contains("control characters"), "{err}");
        }
    }

    #[test]
    fn font_family_validation_rejects_blank_and_oversized_values() {
        let oversized = "x".repeat(257);
        for family in [" \t", oversized.as_str()] {
            let err = validate_family(family).unwrap_err();
            assert!(err.contains("empty") || err.contains("256 bytes"), "{err}");
        }
    }

    fn sh(script: &str) -> Command {
        let mut c = Command::new("sh");
        c.args(["-c", script]);
        c
    }

    fn run_helper(script: &str, cancel: &AtomicBool, deadline: Duration, cap: usize) -> Discovery {
        // Mirrors `discover_with`'s helper arm so the same primitive is exercised
        // with a fake helper (the real resolver only admits system directories).
        let mut cmd = sh(script);
        match crate::preview_jobs::bounded_capture(&mut cmd, cancel, deadline, cap) {
            Ok(b) => finish_rows(&b, false, vec![]),
            Err(crate::preview_jobs::CaptureError::Capped(b)) => finish_rows(&b, true, vec![]),
            Err(crate::preview_jobs::CaptureError::Timeout) => {
                Discovery::bare(DiscoveryStatus::TimedOut, vec![])
            }
            Err(crate::preview_jobs::CaptureError::Cancelled) => {
                Discovery::bare(DiscoveryStatus::Canceled, vec![])
            }
            Err(e) => Discovery::bare(DiscoveryStatus::Unavailable(format!("{e:?}")), vec![]),
        }
    }

    #[test]
    fn hung_helper_with_descendant_times_out_promptly() {
        let started = Instant::now();
        let d = run_helper(
            "sleep 30 & sleep 30",
            &AtomicBool::new(false),
            Duration::from_millis(200),
            1024,
        );
        assert_eq!(d.status, DiscoveryStatus::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn output_flood_is_capped_and_marked_truncated() {
        let d = run_helper(
            "yes 'Flood Family' | head -c 5000000",
            &AtomicBool::new(false),
            Duration::from_secs(10),
            64 * 1024,
        );
        assert_eq!(
            d.status,
            DiscoveryStatus::Truncated(Truncation::HelperOutput)
        );
        assert!(d.notice().unwrap().contains("truncated"));
    }

    #[test]
    fn cancel_stops_a_running_helper() {
        let cancel = AtomicBool::new(true);
        let started = Instant::now();
        let d = run_helper("sleep 30", &cancel, Duration::from_secs(10), 1024);
        assert_eq!(d.status, DiscoveryStatus::Canceled);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn result_cap_truncates_with_typed_status() {
        let text: String = (0..MAX_RESULTS + 10)
            .map(|i| format!("Family{i}\n"))
            .collect();
        let d = finish_rows(text.as_bytes(), false, vec![]);
        assert_eq!(d.rows.len(), MAX_RESULTS);
        assert_eq!(d.status, DiscoveryStatus::Truncated(Truncation::Results));
    }

    #[test]
    fn scan_breadth_budget_stops_with_truncation() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..20 {
            std::fs::write(tmp.path().join(format!("Fam{i}-Regular.ttf")), b"").unwrap();
        }
        let mut st = ScanState {
            families: BTreeSet::new(),
            errors: vec![],
            entries: 0,
            name_bytes: 0,
            started: Instant::now(),
            deadline: Duration::from_secs(5),
            max_entries: 5,
            stop: ScanStop::Done,
        };
        let scan = font_rows_from_dirs_in(
            &[tmp.path().to_path_buf()],
            &AtomicBool::new(false),
            &mut st,
        );
        assert_eq!(scan.stop, ScanStop::Budget);
        assert!(scan.rows.len() <= 5);
    }

    #[test]
    fn scan_deadline_and_cancel_stop_the_walk() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("A-Regular.ttf"), b"").unwrap();
        let mk = |deadline| ScanState {
            families: BTreeSet::new(),
            errors: vec![],
            entries: 0,
            name_bytes: 0,
            started: Instant::now(),
            deadline,
            max_entries: 100,
            stop: ScanStop::Done,
        };
        let dirs = [tmp.path().to_path_buf()];
        let s = font_rows_from_dirs_in(&dirs, &AtomicBool::new(false), &mut mk(Duration::ZERO));
        assert_eq!(s.stop, ScanStop::Deadline);
        let s = font_rows_from_dirs_in(
            &dirs,
            &AtomicBool::new(true),
            &mut mk(Duration::from_secs(5)),
        );
        assert_eq!(s.stop, ScanStop::Canceled);
    }

    #[test]
    fn unreadable_source_is_reported_not_swallowed() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("not-a-dir");
        std::fs::write(&file, b"").unwrap();
        let scan = font_rows_from_dirs(&[file], &AtomicBool::new(false));
        assert_eq!(scan.errors.len(), 1, "{:?}", scan.errors);
    }

    #[test]
    fn helper_resolution_ignores_ambient_path_and_untrusted_files() {
        let tmp = tempfile::tempdir().unwrap();
        // A non-executable file named fc-list is not an admitted identity.
        std::fs::write(tmp.path().join("fc-list"), b"#!/bin/sh\n").unwrap();
        assert_eq!(resolve_fc_list(&[tmp.path().to_path_buf()], None), None);
        // Nor is it picked up through PATH.
        assert_eq!(resolve_fc_list(&[], Some(tmp.path().as_os_str())), None);
        // The search list is fixed system directories, never `$PATH` entries.
        let dirs = fc_list_search_dirs();
        assert!(dirs.iter().all(|d| d.is_absolute()));
        assert!(!dirs.contains(&tmp.path().to_path_buf()));
    }

    #[test]
    fn resolver_prefers_known_dirs_then_falls_back_to_admitted_path_entries() {
        if !crate::platform::test_mode_bits_supported() {
            return;
        }
        let mk = |dir: &Path, mode: u32| {
            let f = dir.join("fc-list");
            std::fs::write(&f, b"#!/bin/sh\n").unwrap();
            crate::platform::test_set_mode(&f, mode).unwrap();
            f
        };
        let known = tempfile::tempdir().unwrap();
        let on_path = tempfile::tempdir().unwrap();
        let writable = tempfile::tempdir().unwrap();
        let k = mk(known.path(), 0o755);
        let p = mk(on_path.path(), 0o755);
        mk(writable.path(), 0o777);
        let path = std::env::join_paths([writable.path(), on_path.path()]).unwrap();
        // Known dirs win over PATH.
        assert_eq!(
            resolve_fc_list(&[known.path().to_path_buf()], Some(&path)),
            Some(k)
        );
        // No known match: PATH is scanned, the writable entry is rejected.
        assert_eq!(resolve_fc_list(&[], Some(&path)), Some(p));
        // Relative PATH entries are ignored.
        assert_eq!(resolve_fc_list(&[], Some(std::ffi::OsStr::new("."))), None);
    }

    #[test]
    fn search_dirs_cover_home_manager_and_nix_profiles() {
        let dirs = fc_list_search_dirs();
        for want in [
            "/nix/var/nix/profiles/default/bin",
            "/home/linuxbrew/.linuxbrew/bin",
            "/run/current-system/sw/bin",
        ] {
            assert!(dirs.contains(&PathBuf::from(want)), "{want}");
        }
    }

    #[test]
    fn owner_coalesces_and_rejects_stale_generations() {
        let mut o = DiscoveryOwner::default();
        assert!(o.begin());
        assert!(!o.begin(), "second request coalesces while in flight");
        assert!(!o.accept(o.generation + 1), "wrong generation is stale");
        assert!(o.in_flight());
        let g = o.generation;
        assert!(o.accept(g));
        assert!(!o.accept(g), "a result is accepted once");
        assert!(o.begin());
        assert!(!o.accept(g), "the previous generation can never apply");
    }

    #[test]
    fn dropping_the_owner_cancels_the_run() {
        let o = DiscoveryOwner::default();
        let flag = Arc::clone(&o.cancel);
        drop(o);
        assert!(flag.load(Ordering::Acquire));
    }

    #[test]
    fn notices_distinguish_every_outcome() {
        let mk = |status| Discovery::bare(status, vec!["src: boom".into()]);
        assert!(
            mk(DiscoveryStatus::TimedOut)
                .notice()
                .unwrap()
                .contains("timed out")
        );
        assert!(
            mk(DiscoveryStatus::Canceled)
                .notice()
                .unwrap()
                .contains("canceled")
        );
        let n = mk(DiscoveryStatus::Unavailable("x".into()))
            .notice()
            .unwrap();
        assert!(n.contains("x") && n.contains("src: boom"));
        assert!(
            mk(DiscoveryStatus::Complete)
                .notice()
                .unwrap()
                .contains("No fonts")
        );
    }

    #[test]
    fn dir_enumeration_finds_fonts_macos_actually_installs() {
        // The three real layouts a flat `read_dir` missed, at their real depths
        // (copied from an actual nix-darwin Mac — a shallower fixture would have
        // passed against a `FONT_SCAN_DEPTH` that still misses in the field,
        // which is exactly the mistake this fixture exists to prevent).
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("Menlo.ttc"), b"").unwrap();

        // depth 1 — /System/Library/Fonts/Supplemental
        let supp = root.join("Supplemental");
        std::fs::create_dir_all(&supp).unwrap();
        std::fs::write(supp.join("Courier New.ttf"), b"").unwrap();

        // depth 6 — Nix Fonts/<hash>-<pkg>/share/fonts/opentype
        let nix_otf = root.join("Nix Fonts/abc123-jetbrains-mono-2.304/share/fonts/opentype");
        std::fs::create_dir_all(&nix_otf).unwrap();
        std::fs::write(nix_otf.join("JetBrainsMono-Regular.otf"), b"").unwrap();

        // depth 8 — …/share/fonts/truetype/NerdFonts/<Family>, where the Nerd
        // Font packages actually put their files.
        let nix_nf = root.join(
            "Nix Fonts/def456-nerd-fonts-fira-code-3.5.0/share/fonts/truetype/NerdFonts/FiraCode",
        );
        std::fs::create_dir_all(&nix_nf).unwrap();
        std::fs::write(nix_nf.join("FiraCodeNerdFont-Regular.ttf"), b"").unwrap();
        std::fs::write(nix_nf.join("FiraCodeNerdFont-Bold.ttf"), b"").unwrap();

        // One level past the budget — proves the depth cap is real, so a font
        // directory can never turn the picker into a filesystem walk.
        let deep = nix_nf.join("too/deep");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("Unreachable-Regular.ttf"), b"").unwrap();

        let families: Vec<String> =
            font_rows_from_dirs(&[root.to_path_buf()], &AtomicBool::new(false))
                .rows
                .into_iter()
                .map(|r| r.family)
                .collect();
        assert!(families.contains(&"Menlo".to_string()), "{families:?}");
        assert!(
            families.contains(&"Courier New".to_string()),
            "{families:?}"
        );
        assert!(
            families.contains(&"JetBrainsMono".to_string()),
            "{families:?}"
        );
        assert!(
            families.contains(&"FiraCodeNerdFont".to_string()),
            "the nix-store nesting is the case that made this a bug: {families:?}"
        );
        assert!(
            !families.contains(&"Unreachable".to_string()),
            "depth cap must hold: {families:?}"
        );
    }

    #[test]
    fn dir_enumeration_dedupes_faces_and_ranks_like_fc_list() {
        let dir = std::env::temp_dir().join(format!("tg-fontdir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        std::fs::create_dir_all(&dir).expect("mkdir");
        for f in [
            "JetBrainsMonoNerdFont-Regular.ttf",
            "JetBrainsMonoNerdFont-Bold.ttf",
            "JetBrainsMonoNerdFont-Italic.otf",
            "Menlo.ttc",
            "NotAFont.txt", // wrong extension — ignored
            "README",       // no extension at all — ignored
        ] {
            std::fs::write(dir.join(f), b"").expect("write");
        }
        let rows = font_rows_from_dirs(std::slice::from_ref(&dir), &AtomicBool::new(false)).rows;
        let families: Vec<&str> = rows.iter().map(|r| r.family.as_str()).collect();
        assert_eq!(families.len(), 2, "rows: {families:?}");
        assert!(families.contains(&"Menlo"));
        assert!(families.contains(&"JetBrainsMonoNerdFont"));
        let _ = std::fs::remove_dir_all(&dir); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        // A missing directory is skipped, not an error.
        let missing =
            font_rows_from_dirs(&[PathBuf::from("/no/such/dir")], &AtomicBool::new(false));
        assert!(missing.rows.is_empty());
        assert!(missing.errors.is_empty(), "absence is not an error");
    }

    #[test]
    fn parses_fc_list_families_dedupes_and_prioritizes_recommended_fonts() {
        let fc_list = "\
FiraCode Nerd Font,FiraCode NF\n\
ZedMono Nerd Font,ZedMono NF\n\
JetBrainsMono Nerd Font,JetBrainsMono NF\n\
/path/FiraBold.ttf: FiraCode Nerd Font:style=Bold\n";

        let rows = font_rows_from_fc_list(fc_list);

        let labels: Vec<_> = rows.iter().map(|row| row.label.as_str()).collect();
        assert_eq!(labels[0], "★ Recommended — JetBrainsMono Nerd Font");
        assert_eq!(labels[1], "★ Recommended — FiraCode Nerd Font");
        assert!(labels.contains(&"ZedMono Nerd Font"));
        assert_eq!(
            rows.iter()
                .filter(|row| row.family == "FiraCode Nerd Font")
                .count(),
            1
        );
    }

    #[test]
    fn patch_alacritty_font_family_updates_only_normal_family_line() {
        let input = "\
[font]\n\
normal = { family = \"FiraCode Nerd Font\" }\n\
size = 13\n\
# normal = { family = \"Commented\" }\n";

        let patched = patch_alacritty_font_family(input, "JetBrainsMono Nerd Font").unwrap();

        assert!(patched.contains("normal = { family = \"JetBrainsMono Nerd Font\" }"));
        assert!(patched.contains("# normal = { family = \"Commented\" }"));
        assert!(patched.contains("size = 13"));
    }

    #[test]
    fn staging_suffix_is_32_random_hex_chars() {
        let first = random_staging_suffix().unwrap();
        let second = random_staging_suffix().unwrap();

        assert_eq!(first.len(), 32);
        assert!(first.chars().all(|character| character.is_ascii_hexdigit()));
        assert_eq!(second.len(), 32);
        assert!(
            second
                .chars()
                .all(|character| character.is_ascii_hexdigit())
        );
        assert_ne!(first, second);
    }

    #[test]
    fn staging_skips_a_precreated_name_without_touching_it() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("kitty.conf");
        let observed = b"font_family Menlo\n";
        std::fs::write(&config, observed).unwrap();
        let planted = tmp.path().join(".kitty.conf.tmp-aaaa");
        std::fs::write(&planted, b"planted").unwrap();
        let suffixes = ["aaaa", "bbbb"];
        let mut next_suffix = suffixes.iter();
        let mut suffix = || Ok(next_suffix.next().unwrap().to_string());

        replace_config_with(&config, observed, true, b"font_family Hack\n", &mut suffix).unwrap();

        assert_eq!(std::fs::read(&config).unwrap(), b"font_family Hack\n");
        assert_eq!(std::fs::read(&planted).unwrap(), b"planted");
        assert!(!tmp.path().join(".kitty.conf.tmp-bbbb").exists());
    }

    #[test]
    fn staging_gives_up_after_bounded_attempts() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("kitty.conf");
        let observed = b"font_family Menlo\n";
        std::fs::write(&config, observed).unwrap();
        let planted = tmp.path().join(".kitty.conf.tmp-aaaa");
        std::fs::write(&planted, b"planted").unwrap();
        let mut calls = 0;
        let mut suffix = || {
            calls += 1;
            Ok("aaaa".to_string())
        };

        let result =
            replace_config_with(&config, observed, true, b"font_family Hack\n", &mut suffix);

        assert!(
            matches!(result, Err(ref error) if error.contains("could not create a unique staging file"))
        );
        assert_eq!(std::fs::read(&config).unwrap(), observed);
        assert_eq!(calls, STAGING_ATTEMPTS);
        assert_eq!(std::fs::read(&planted).unwrap(), b"planted");
        assert!(std::fs::read_dir(tmp.path()).unwrap().all(|entry| {
            let entry = entry.unwrap();
            !entry.file_name().to_string_lossy().contains(".tmp-") || entry.path() == planted
        }));
    }

    #[test]
    fn staged_file_carries_the_destination_permissions() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("kitty.conf");
        std::fs::write(&config, b"old").unwrap();
        let mut permissions = std::fs::metadata(&config).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&config, permissions.clone()).unwrap();
        let permissions = std::fs::metadata(&config).unwrap().permissions();
        let mut suffix = || Ok("cccc".to_string());

        let staged = stage_sibling(&config, b"x", Some(permissions.clone()), &mut suffix).unwrap();

        assert_eq!(
            std::fs::metadata(&staged).unwrap().permissions(),
            permissions,
            "the staged file must inherit the explicitly read-only destination permissions"
        );
        assert_eq!(std::fs::read(&staged).unwrap(), b"x");
    }
}
